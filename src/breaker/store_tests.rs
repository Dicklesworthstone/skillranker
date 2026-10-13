//! Real files and SQLite; no provider traffic or simulated storage.
use super::*;
use crate::breaker::{BASE_COOLDOWN_MS, BREAKER_FILE, Breaker, Outcome, Protection, Refusal};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const BUSY: Duration = Duration::from_millis(25);

fn home() -> PathBuf {
    let root = fs::canonicalize("/tmp").unwrap().join(format!(
        "sr-breaker-store-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    root
}

fn legacy(path: &Path) {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    drop(file);
    let db = Connection::open(path).unwrap();
    // Exercise the old initializer, including SQLite's removal of IF NOT EXISTS.
    db.execute_batch(&format!(
        "{};",
        DDL.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1)
    ))
    .unwrap();
    db.execute(
        "INSERT INTO breaker_state VALUES ('o',7,1,0,30000,500,'owner',6000,0)",
        [],
    )
    .unwrap();
}

fn unchanged_refusal(path: &Path) {
    let before = fs::read(path).unwrap();
    let mode = fs::metadata(path).unwrap().permissions().mode();
    assert!(open(path, BUSY, true).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(fs::metadata(path).unwrap().permissions().mode(), mode);
}

#[test]
fn new_store_is_private_identified_and_uses_verified_wal() {
    let root = home();
    let path = root.join("new").join(BREAKER_FILE);
    assert!(open(&path, BUSY, false).is_err());
    assert!(
        !root.join("new").exists(),
        "a read must not initialize storage"
    );
    let store = open(&path, BUSY, true).unwrap().unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    assert!(schema(&store.connection, false).unwrap() == Schema::Current);
    let mode: String = store
        .connection
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let sync: i64 = store
        .connection
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1, "breaker stays NORMAL, not enforcement accounting");
    store.verify().unwrap();
}

#[test]
fn exact_legacy_adoption_keeps_probe_and_cooldown_across_reopen() {
    let path = home().join(BREAKER_FILE);
    legacy(&path);
    {
        let store = open(&path, BUSY, false).unwrap().unwrap();
        verify_schema(&store.connection).unwrap();
        let row: (i64, String, i64, i64) = store.connection.query_row(
            "SELECT generation, probe_token, probe_expires_ms, open_until_ms FROM breaker_state WHERE origin='o'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        assert_eq!(row, (7, "owner".to_owned(), 6000, 500));
    }
    let breaker = Breaker::new(Some(path), "o");
    assert_eq!(
        breaker.admit(499, 100),
        Err(Refusal::CoolingDown { until_unix_ms: 500 })
    );
    assert_eq!(breaker.admit(501, 100), Err(Refusal::ProbeInFlight));
    assert_eq!(breaker.protection(), Protection::Shared);
}

#[test]
fn foreign_headers_schema_and_extra_objects_are_never_repaired() {
    for mutation in [
        "PRAGMA application_id=123",
        "PRAGMA user_version=2",
        "CREATE TABLE unrelated (secret TEXT); INSERT INTO unrelated VALUES ('keep')",
        "CREATE VIEW extra AS SELECT * FROM breaker_state",
        "CREATE INDEX extra ON breaker_state(open)",
        "CREATE TRIGGER extra AFTER UPDATE ON breaker_state BEGIN DELETE FROM breaker_state; END",
        "ALTER TABLE breaker_state ADD COLUMN unknown INTEGER",
    ] {
        let path = home().join(BREAKER_FILE);
        legacy(&path);
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(mutation).unwrap();
        }
        unchanged_refusal(&path);
        assert!(!path.with_file_name(format!("{BREAKER_FILE}-wal")).exists());
    }
    let path = home().join(BREAKER_FILE);
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE unrelated (v); INSERT INTO unrelated VALUES (42)")
            .unwrap();
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    unchanged_refusal(&path);
}

#[test]
fn damaged_and_oversized_files_remain_unchanged() {
    let path = home().join(BREAKER_FILE);
    fs::write(&path, b"this is not a SQLite database").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    unchanged_refusal(&path);
    let path = home().join(BREAKER_FILE);
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.set_len((MAX_BREAKER_BYTES + 1) as u64).unwrap();
    assert!(open(&path, BUSY, true).is_err());
    assert_eq!(
        file.metadata().unwrap().len(),
        (MAX_BREAKER_BYTES + 1) as u64
    );
}

#[test]
fn existing_modes_are_refused_not_chmodded() {
    let path = home().join(BREAKER_FILE);
    legacy(&path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    unchanged_refusal(&path);
    let root = home();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(open(&root.join(BREAKER_FILE), BUSY, true).is_err());
    assert!(!root.join(BREAKER_FILE).exists());
    assert_eq!(
        fs::metadata(root).unwrap().permissions().mode() & 0o7777,
        0o755
    );
}

#[test]
fn links_and_special_files_are_refused_before_sqlite() {
    for hard in [false, true] {
        let root = home();
        let target = root.join("keep.sqlite3");
        legacy(&target);
        let path = root.join(BREAKER_FILE);
        if hard {
            fs::hard_link(&target, &path).unwrap();
        } else {
            symlink(&target, &path).unwrap();
        }
        let before = fs::read(&target).unwrap();
        assert!(open(&path, BUSY, true).is_err());
        assert_eq!(fs::read(&target).unwrap(), before);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }
    let path = home().join(BREAKER_FILE);
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    assert!(open(&path, BUSY, true).is_err());
    let path = home().join(BREAKER_FILE);
    fs::create_dir(&path).unwrap();
    assert!(open(&path, BUSY, true).is_err());
    assert!(path.is_dir());
}

#[test]
fn ancestor_symlinks_and_shared_writable_parents_cannot_create_descendants() {
    let root = home();
    let real = root.join("real");
    fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
    let alias = root.join("alias");
    symlink(&real, &alias).unwrap();
    assert!(open(&alias.join("new").join(BREAKER_FILE), BUSY, true).is_err());
    assert!(!real.join("new").exists());
    fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(open(&real.join("new").join(BREAKER_FILE), BUSY, true).is_err());
    assert!(!real.join("new").exists());
}

#[test]
fn every_unsafe_sqlite_sidecar_is_refused_without_permission_repair() {
    for suffix in ["-wal", "-shm", "-journal"] {
        for link in [false, true] {
            let root = home();
            let path = root.join(BREAKER_FILE);
            legacy(&path);
            let sidecar = root.join(format!("{BREAKER_FILE}{suffix}"));
            let target = root.join("untouched");
            fs::write(&target, b"keep this data").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
            if link {
                symlink(&target, &sidecar).unwrap();
            } else {
                fs::hard_link(&target, &sidecar).unwrap();
            }
            let before = fs::read(&path).unwrap();
            assert!(open(&path, BUSY, false).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(fs::read(&target).unwrap(), b"keep this data");
            assert_eq!(
                fs::metadata(target).unwrap().permissions().mode() & 0o7777,
                0o644
            );
        }
    }
}

#[test]
fn a_held_store_rejects_file_or_parent_replacement() {
    for replace_parent in [false, true] {
        let root = home();
        let path = root.join(BREAKER_FILE);
        let store = open(&path, BUSY, true).unwrap().unwrap();
        if replace_parent {
            fs::rename(&root, root.with_extension("held")).unwrap();
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        } else {
            fs::rename(&path, root.join("held.sqlite3")).unwrap();
        }
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(store.verify().is_err());
    }
}

#[test]
fn only_failure_initialization_can_recover_an_empty_private_file() {
    let path = home().join(BREAKER_FILE);
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    assert!(open(&path, BUSY, false).unwrap().is_none());
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    // Exercise the public lazy path, not just the initialization helper.
    let breaker = Breaker::new(Some(path.clone()), "o");
    let ticket = breaker.admit(0, 3000).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(breaker.protection(), Protection::Shared);
    breaker.settle(
        &ticket,
        Outcome::Transient {
            retry_after_ms: None,
        },
        0,
    );
    let store = open(&path, BUSY, false).unwrap().unwrap();
    verify_schema(&store.connection).unwrap();
    let failures: i64 = store
        .connection
        .query_row(
            "SELECT failures FROM breaker_state WHERE origin='o'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failures, 1);
}

#[test]
fn refused_foreign_store_uses_local_breaker_without_modifying_the_file() {
    let path = home().join(BREAKER_FILE);
    fs::write(&path, b"keep foreign state").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let breaker = Breaker::new(Some(path.clone()), "o");
    for now in 0..3 {
        let ticket = breaker.admit(now, 3000).unwrap();
        breaker.settle(
            &ticket,
            Outcome::Transient {
                retry_after_ms: None,
            },
            now,
        );
    }
    assert_eq!(breaker.protection(), Protection::ProcessLocal);
    assert_eq!(
        breaker.admit(3, 3000),
        Err(Refusal::CoolingDown {
            until_unix_ms: 2 + BASE_COOLDOWN_MS
        })
    );
    assert_eq!(fs::read(path).unwrap(), b"keep foreign state");
}

#[test]
fn private_new_store_still_shares_failures_and_recovery() {
    let path = home().join(BREAKER_FILE);
    let first = Breaker::new(Some(path.clone()), "o");
    for now in 0..3 {
        let ticket = first.admit(now, 3000).unwrap();
        first.settle(
            &ticket,
            Outcome::Transient {
                retry_after_ms: None,
            },
            now,
        );
    }
    let second = Breaker::new(Some(path), "o");
    assert_eq!(
        second.admit(3, 3000),
        Err(Refusal::CoolingDown {
            until_unix_ms: 2 + BASE_COOLDOWN_MS
        })
    );
    let at = 2 + BASE_COOLDOWN_MS;
    let ticket = second.admit(at, 3000).unwrap();
    second.settle(&ticket, Outcome::Success, at + 1);
    assert!(first.admit(at + 2, 3000).is_ok());
    assert_eq!(first.protection(), Protection::Shared);
    assert_eq!(second.protection(), Protection::Shared);
}
