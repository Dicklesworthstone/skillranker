//! Real files and SQLite; no provider requests or simulated accounting store.
use super::super::{
    ACCOUNTING_FILE, Admission, AllowancePaths, COUNTER, SetupStep, admit, apply_setup, inspect,
    preview_setup, wall_clock_ms,
};
use super::*;
use crate::jev::{CanonicalOrigin, RankingStage};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::sync::atomic::Ordering;

const LOCK: Duration = Duration::from_millis(200);

fn paths() -> AllowancePaths {
    let root = std::fs::canonicalize("/tmp")
        .expect("resolve trusted system temporary directory")
        .join(format!(
            "sr-accounting-files-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    AllowancePaths::new(&root.join("config"), &root.join("cache"))
}

fn ready() -> AllowancePaths {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    paths
}

fn private_file(path: &Path, len: u64) {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.set_len(len).unwrap();
}

fn image(path: &Path) -> (Vec<u8>, u32) {
    (
        fs::read(path).unwrap(),
        fs::metadata(path).unwrap().mode() & 0o7777,
    )
}

fn report(paths: &AllowancePaths) -> super::super::Inspection {
    inspect(
        paths,
        &CanonicalOrigin::production(),
        wall_clock_ms().unwrap(),
    )
}

#[test]
fn private_creation_retains_real_durable_debits() {
    let paths = ready();
    let writer = open_writer(&paths.accounting, false).unwrap();
    assert_eq!(
        writer
            .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert_eq!(
        writer
            .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        fs::metadata(&paths.accounting).unwrap().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        fs::metadata(paths.accounting.parent().unwrap())
            .unwrap()
            .mode()
            & 0o7777,
        0o700
    );
    drop(writer);
    let result = admit(
        &paths,
        &CanonicalOrigin::production(),
        RankingStage::Wide,
        true,
        LOCK,
    )
    .unwrap();
    assert!(matches!(result, Admission::Debited(_)));
    assert_eq!(report(&paths).charged_attempts, Some(1));
    assert!(preview_setup(&paths, 3).unwrap().unchanged);
}

#[test]
fn existing_main_file_permissions_are_never_repaired() {
    for mode in [0o644, 0o660, 0o700, 0o4600] {
        let paths = ready();
        fs::set_permissions(&paths.accounting, fs::Permissions::from_mode(mode)).unwrap();
        let before = image(&paths.accounting);
        let guard = fs::read(&paths.guard).unwrap();
        assert!(open_writer(&paths.accounting, false).is_err());
        assert!(read(&paths.accounting, read_meta).is_err());
        assert!(apply_setup(&paths, 4, LOCK, None).is_err());
        assert_eq!(image(&paths.accounting), before);
        assert_eq!(fs::read(&paths.guard).unwrap(), guard);
    }
}

#[test]
fn private_read_only_files_remain_inspectable_but_cannot_be_used_as_writers() {
    let paths = ready();
    {
        let db = Connection::open(&paths.accounting).unwrap();
        db.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))
            .unwrap();
    }
    fs::set_permissions(&paths.accounting, fs::Permissions::from_mode(0o400)).unwrap();
    fs::set_permissions(
        paths.accounting.parent().unwrap(),
        fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    let before = image(&paths.accounting);
    assert_eq!(report(&paths).health, "ready");
    assert_eq!(report(&paths).charged_attempts, Some(0));
    assert!(open_writer(&paths.accounting, false).is_err());
    assert_eq!(image(&paths.accounting), before);
}

#[test]
fn foreign_and_partial_schemas_are_refused_before_writer_reconfiguration() {
    for sql in [
        "CREATE TABLE foreign_data(value TEXT)",
        "PRAGMA application_id=123",
        "PRAGMA user_version=2",
        "CREATE TABLE allowance_meta(id INTEGER)",
    ] {
        let paths = paths();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(paths.accounting.parent().unwrap())
            .unwrap();
        {
            let db = Connection::open(&paths.accounting).unwrap();
            db.execute_batch(sql).unwrap();
        }
        fs::set_permissions(&paths.accounting, fs::Permissions::from_mode(0o600)).unwrap();
        let before = image(&paths.accounting);
        assert!(open_writer(&paths.accounting, true).is_err());
        assert!(apply_setup(&paths, 3, LOCK, None).is_err());
        assert_eq!(image(&paths.accounting), before);
        assert!(
            !paths.guard.exists(),
            "foreign accounting must not publish an intent"
        );
        assert!(
            !paths
                .accounting
                .with_file_name("allowance.sqlite3-wal")
                .exists()
        );
    }
}

#[test]
fn symlink_fifo_and_hardlink_main_files_are_refused_without_opening_them() {
    for kind in ["symlink", "fifo", "hardlink"] {
        let paths = ready();
        let saved = paths.accounting.with_extension("saved");
        fs::rename(&paths.accounting, &saved).unwrap();
        match kind {
            "symlink" => symlink(&saved, &paths.accounting).unwrap(),
            "fifo" => {
                nix::unistd::mkfifo(&paths.accounting, Mode::from_bits_truncate(0o600)).unwrap()
            }
            _ => fs::hard_link(&saved, &paths.accounting).unwrap(),
        }
        let before = image(&saved);
        assert!(open_writer(&paths.accounting, false).is_err());
        assert!(read(&paths.accounting, read_meta).is_err());
        assert_eq!(image(&saved), before);
    }
}

#[test]
fn every_sqlite_sidecar_is_checked_before_reader_or_writer_access() {
    for suffix in ["-wal", "-shm", "-journal"] {
        for kind in ["symlink", "fifo", "hardlink", "mode"] {
            let paths = ready();
            let target = paths.accounting.with_extension("target");
            private_file(&target, 16);
            let side = paths
                .accounting
                .with_file_name(format!("{ACCOUNTING_FILE}{suffix}"));
            assert!(!side.exists());
            match kind {
                "symlink" => symlink(&target, &side).unwrap(),
                "fifo" => nix::unistd::mkfifo(&side, Mode::from_bits_truncate(0o600)).unwrap(),
                "hardlink" => fs::hard_link(&target, &side).unwrap(),
                _ => {
                    private_file(&side, 16);
                    fs::set_permissions(&side, fs::Permissions::from_mode(0o644)).unwrap();
                }
            }
            let before = image(&paths.accounting);
            let target_before = image(&target);
            assert!(open_writer(&paths.accounting, false).is_err());
            assert!(read(&paths.accounting, read_meta).is_err());
            assert_eq!(image(&paths.accounting), before);
            assert_eq!(image(&target), target_before);
        }
    }
}

#[test]
fn orphaned_sidecars_cannot_be_attached_to_fresh_accounting() {
    for suffix in ["-wal", "-shm", "-journal"] {
        let paths = paths();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(paths.accounting.parent().unwrap())
            .unwrap();
        let side = paths
            .accounting
            .with_file_name(format!("{ACCOUNTING_FILE}{suffix}"));
        private_file(&side, 16);
        let before = image(&side);
        assert!(open_writer(&paths.accounting, true).is_err());
        assert!(read(&paths.accounting, read_meta).is_err());
        assert!(!paths.accounting.exists());
        assert_eq!(image(&side), before);
    }
}

#[test]
fn unsafe_and_dangling_ancestors_are_not_absence_or_creation_authority() {
    let paths = paths();
    let root = paths.accounting.parent().unwrap().parent().unwrap();
    let outside = root.join("outside");
    fs::DirBuilder::new().mode(0o700).create(&outside).unwrap();
    symlink(&outside, root.join("alias")).unwrap();
    let through_alias = root.join("alias/new/allowance.sqlite3");
    assert!(read(&through_alias, read_meta).is_err());
    assert!(open_writer(&through_alias, true).is_err());
    assert!(!outside.join("new").exists());
    symlink("missing", root.join("dangling")).unwrap();
    assert!(read(&root.join("dangling/allowance.sqlite3"), read_meta).is_err());
    let shared = root.join("shared");
    fs::DirBuilder::new().mode(0o700).create(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(open_writer(&shared.join("new/allowance.sqlite3"), true).is_err());
    assert!(!shared.join("new").exists());
}

#[test]
fn held_directory_and_database_replacements_are_detected() {
    let paths = ready();
    let store = open_writer(&paths.accounting, false).unwrap();
    let parent = paths.accounting.parent().unwrap();
    fs::rename(parent, parent.with_extension("saved")).unwrap();
    fs::DirBuilder::new().mode(0o700).create(parent).unwrap();
    assert!(store.files.verify().is_err());
    drop(store);

    let paths = ready();
    let store = open_writer(&paths.accounting, false).unwrap();
    fs::rename(&paths.accounting, paths.accounting.with_extension("saved")).unwrap();
    private_file(&paths.accounting, 0);
    assert!(store.files.verify().is_err());
}

#[test]
fn main_and_sidecar_bounds_refuse_before_sqlite_parses_them() {
    let paths = ready();
    OpenOptions::new()
        .write(true)
        .open(&paths.accounting)
        .unwrap()
        .set_len(ACCOUNTING_MAX_BYTES as u64 + 1)
        .unwrap();
    assert!(open_writer(&paths.accounting, false).is_err());
    assert!(read(&paths.accounting, read_meta).is_err());
    let paths = ready();
    let side = paths.accounting.with_file_name("allowance.sqlite3-wal");
    private_file(&side, FAMILY_MAX_BYTES as u64 + 1);
    assert!(open_writer(&paths.accounting, false).is_err());
    assert!(read(&paths.accounting, read_meta).is_err());
    assert!(matches!(
        open_writer(&paths.accounting, false),
        Err(AllowanceError::State(message)) if message.contains("storage quota is exhausted")
    ));
    assert!(matches!(
        read(&paths.accounting, read_meta),
        Err(AllowanceError::State(message)) if message.contains("storage quota is exhausted")
    ));
}

#[test]
fn first_activation_crash_retry_and_limit_changes_keep_existing_charges() {
    for step in [SetupStep::IntentPublished, SetupStep::AccountingReady] {
        let paths = paths();
        let initial = apply_setup(&paths, 3, LOCK, Some(step)).unwrap();
        assert!(
            admit(
                &paths,
                &CanonicalOrigin::production(),
                RankingStage::Wide,
                true,
                LOCK
            )
            .is_err()
        );
        let resumed = apply_setup(&paths, 3, LOCK, None).unwrap();
        assert_eq!(initial.target.generation, resumed.target.generation);
        admit(
            &paths,
            &CanonicalOrigin::production(),
            RankingStage::Wide,
            true,
            LOCK,
        )
        .unwrap();
        apply_setup(&paths, 4, LOCK, None).unwrap();
        assert_eq!(report(&paths).charged_attempts, Some(1));
    }
}

#[test]
fn read_callback_cannot_publish_results_from_a_replaced_file() {
    let paths = ready();
    let result = read(&paths.accounting, |db| {
        let meta = read_meta(db)?;
        fs::rename(&paths.accounting, paths.accounting.with_extension("saved")).unwrap();
        private_file(&paths.accounting, 0);
        Ok(meta)
    });
    assert!(result.is_err());
}
