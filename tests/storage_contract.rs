#![cfg(target_os = "linux")]

use rusqlite::Connection;
use skillranker::limits::DurationMillis;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::storage::{
    CACHE_FILE, CACHE_QUOTA_BYTES, CACHE_SCHEMA_VERSION, CacheAccess, CacheLocation, CacheOpen,
    CacheStore, QUALIFIED_SQLITE_SOURCE_ID, QUALIFIED_SQLITE_VERSION, StoreError, linked_engine,
    open_cache,
};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

// Intentionally retained: repository policy forbids automatic tree deletion.
fn private_tree(case: &str) -> PathBuf {
    // RCH's TMPDIR can have group-writable ancestors. Use Linux's root-owned
    // sticky /tmp so the success fixture satisfies the production policy;
    // unsafe custom parents have separate negative tests above.
    let path = Path::new("/tmp").join(format!(
        "sr-storage-{case}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

fn open(path: &Path, access: CacheAccess) -> Result<CacheOpen, StoreError> {
    let start = Instant::now();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = open_cache(
        &invocation,
        &cx,
        access,
        CacheLocation::Directory(path.to_owned()),
    );
    eprintln!(
        "{}",
        serde_json::json!({"case":std::thread::current().name().unwrap_or("storage-open"), "stage":"cache-foundation", "schema":CACHE_SCHEMA_VERSION,
        "elapsed_ms":start.elapsed().as_millis(), "access":format!("{access:?}"),
        "result":match &result { Ok(CacheOpen::Ready(_))=>"ready".to_owned(), Ok(CacheOpen::Missing)=>"missing".to_owned(),
            Ok(CacheOpen::Disabled)=>"disabled".to_owned(), Err(e)=>format!("{e:?}") }})
    );
    assert!(invocation.shutdown());
    result
}

fn ready(path: &Path) -> CacheStore {
    match open(path, CacheAccess::Initialize).unwrap() {
        CacheOpen::Ready(store) => *store,
        other => panic!("expected initialized cache: {other:?}"),
    }
}

fn advance(
    store: CacheStore,
    expected: skillranker::storage::CacheStamp,
) -> Result<CacheStore, StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.advance_generation(&invocation, &cx, expected);
    assert!(invocation.shutdown());
    result
}

fn create_file(path: &Path) -> File {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .mode(0o600)
        .open(path)
        .unwrap()
}

fn raw_database(path: &Path) -> Connection {
    create_file(&path.join(CACHE_FILE));
    Connection::open(path.join(CACHE_FILE)).unwrap()
}

#[test]
fn actual_linked_engine_is_the_qualified_bundle() {
    let identity = linked_engine().unwrap();
    assert_eq!(identity.version, QUALIFIED_SQLITE_VERSION);
    assert_eq!(identity.source_id, QUALIFIED_SQLITE_SOURCE_ID);
    assert_eq!(identity.rust_dependency, "0.40.2");
    eprintln!(
        "{}",
        serde_json::json!({"case":"linked-engine", "sqlite":identity.version,
        "source_id":identity.source_id, "rusqlite":identity.rust_dependency, "result":"qualified"})
    );
}

#[test]
fn disabled_and_missing_cache_have_no_filesystem_effects() {
    let parent = private_tree("disabled");
    let absent = parent.join("uncreated").join("cache");
    assert!(matches!(
        open(&absent, CacheAccess::Disabled).unwrap(),
        CacheOpen::Disabled
    ));
    assert!(matches!(
        open(Path::new("relative/untrusted"), CacheAccess::Disabled).unwrap(),
        CacheOpen::Disabled
    ));
    assert!(matches!(
        open(&absent, CacheAccess::ExistingOnly).unwrap(),
        CacheOpen::Missing
    ));
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    drop(ready(&absent));
    assert!(absent.join(CACHE_FILE).is_file());
    assert!(!absent.join("ledger.sqlite3").exists());
    assert!(!absent.join("allowance.sqlite3").exists());
}

#[test]
fn repeated_initialization_preserves_identity_generation_and_private_wal_files() {
    let path = private_tree("initialize");
    let first = ready(&path);
    let initial = first.stamp();
    let advanced = advance(first, initial).unwrap();
    assert_eq!(advanced.stamp().generation(), 1);
    let second = ready(&path);
    assert_eq!(second.stamp(), advanced.stamp());
    assert_eq!(second.engine(), &linked_engine().unwrap());
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
    for suffix in ["", "-wal", "-shm"] {
        let metadata = fs::metadata(path.join(format!("{CACHE_FILE}{suffix}"))).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
        assert_eq!(metadata.nlink(), 1);
    }
    let observer = Connection::open(path.join(CACHE_FILE)).unwrap();
    assert_eq!(
        observer
            .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert_eq!(
        observer
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert!(!format!("{second:?}").contains(path.to_str().unwrap()));
}

#[test]
fn unsafe_ancestors_leaf_modes_and_relative_paths_are_refused() {
    let root = private_tree("paths");
    let target = root.join("target");
    DirBuilder::new().mode(0o700).create(&target).unwrap();
    symlink(&target, root.join("alias")).unwrap();
    assert_eq!(
        open(&root.join("alias/cache"), CacheAccess::Initialize).unwrap_err(),
        StoreError::UnsafePath
    );
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    assert_eq!(
        open(Path::new("relative/cache"), CacheAccess::Initialize).unwrap_err(),
        StoreError::UnsafePath
    );
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        open(&target, CacheAccess::Initialize).unwrap_err(),
        StoreError::Permissions
    );
    fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        open(&target.join("cache"), CacheAccess::Initialize).unwrap_err(),
        StoreError::Permissions
    );
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    drop(ready(&target));
}

#[test]
fn symlink_and_hardlink_main_or_sidecars_never_touch_the_target() {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        for hard in [false, true] {
            let root = private_tree("links");
            let victim = root.join("protected");
            create_file(&victim);
            fs::write(&victim, b"untouched synthetic marker").unwrap();
            let cache = root.join("cache");
            DirBuilder::new().mode(0o700).create(&cache).unwrap();
            let unsafe_file = cache.join(format!("{CACHE_FILE}{suffix}"));
            if hard {
                fs::hard_link(&victim, &unsafe_file).unwrap();
            } else {
                symlink(&victim, &unsafe_file).unwrap();
            }
            assert_eq!(
                open(&cache, CacheAccess::Initialize).unwrap_err(),
                StoreError::UnsafePath
            );
            assert_eq!(fs::read(&victim).unwrap(), b"untouched synthetic marker");
        }
    }
    drop(ready(&private_tree("link-success")));
}

#[test]
fn nonregular_and_readable_by_others_files_are_refused() {
    let fifo = private_tree("fifo");
    nix::unistd::mkfifo(
        &fifo.join(CACHE_FILE),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    assert_eq!(
        open(&fifo, CacheAccess::Initialize).unwrap_err(),
        StoreError::UnsafePath
    );
    let directory = private_tree("directory");
    fs::create_dir(directory.join(CACHE_FILE)).unwrap();
    assert_eq!(
        open(&directory, CacheAccess::Initialize).unwrap_err(),
        StoreError::UnsafePath
    );
    let permissions = private_tree("mode");
    create_file(&permissions.join(CACHE_FILE));
    fs::set_permissions(
        permissions.join(CACHE_FILE),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        open(&permissions, CacheAccess::Initialize).unwrap_err(),
        StoreError::Permissions
    );
    assert_eq!(
        fs::metadata(permissions.join(CACHE_FILE)).unwrap().mode() & 0o7777,
        0o644
    );
    fs::set_permissions(
        permissions.join(CACHE_FILE),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    drop(ready(&permissions));
}

#[test]
fn uninitialized_corrupt_foreign_and_newer_stores_are_not_repaired() {
    let blank = private_tree("blank");
    create_file(&blank.join(CACHE_FILE));
    assert_eq!(
        open(&blank, CacheAccess::ExistingOnly).unwrap_err(),
        StoreError::Uninitialized
    );
    assert_eq!(fs::metadata(blank.join(CACHE_FILE)).unwrap().len(), 0);
    drop(ready(&blank));
    for case in ["corrupt", "foreign", "newer"] {
        let path = private_tree(case);
        if case == "corrupt" {
            create_file(&path.join(CACHE_FILE));
            fs::write(path.join(CACHE_FILE), b"not a SQLite database").unwrap();
        } else {
            let db = raw_database(&path);
            if case == "foreign" {
                db.execute_batch("CREATE TABLE private_data(value TEXT); INSERT INTO private_data VALUES ('synthetic');").unwrap();
            } else {
                db.pragma_update(None, "user_version", 99).unwrap();
            }
        }
        let before = fs::read(path.join(CACHE_FILE)).unwrap();
        let error = open(&path, CacheAccess::Initialize).unwrap_err();
        assert_eq!(
            error,
            match case {
                "corrupt" => StoreError::Corrupt,
                "foreign" => StoreError::WrongStore,
                _ => StoreError::NewerSchema { version: 99 },
            }
        );
        assert_eq!(fs::read(path.join(CACHE_FILE)).unwrap(), before);
    }
}

#[test]
fn changed_metadata_schema_is_refused_without_modifying_rows() {
    let path = private_tree("schema");
    drop(ready(&path));
    let writer = Connection::open(path.join(CACHE_FILE)).unwrap();
    writer
        .execute_batch("CREATE TABLE unexpected(value INTEGER); INSERT INTO unexpected VALUES (7)")
        .unwrap();
    assert_eq!(
        open(&path, CacheAccess::ExistingOnly).unwrap_err(),
        StoreError::IncompatibleSchema
    );
    assert_eq!(
        writer
            .query_row("SELECT value FROM unexpected", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
}

#[test]
fn real_writer_lock_is_bounded_and_retry_after_release_succeeds() {
    let path = private_tree("busy");
    let store = ready(&path);
    let stamp = store.stamp();
    let writer = Connection::open(path.join(CACHE_FILE)).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(advance(store, stamp).unwrap_err(), StoreError::Busy);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "bounded busy wait took {elapsed:?}"
    );
    assert_eq!(
        writer
            .query_row("SELECT generation FROM sr_cache_meta", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    writer.execute_batch("ROLLBACK").unwrap();
    let reopened = ready(&path);
    let advanced = advance(reopened, stamp).unwrap();
    assert_eq!(advanced.stamp().generation(), 1);
    eprintln!(
        "{}",
        serde_json::json!({"case":"writer-contention", "stage":"generation", "elapsed_ms":elapsed.as_millis(), "result":"busy-then-success"})
    );
}

#[test]
fn stale_generation_and_foreign_incarnation_cannot_mutate() {
    let path = private_tree("generation");
    let first = ready(&path);
    let old = first.stamp();
    let second = ready(&path);
    let winner = advance(first, old).unwrap();
    assert_eq!(
        advance(second, old).unwrap_err(),
        StoreError::StaleGeneration
    );
    assert_eq!(ready(&path).stamp(), winner.stamp());
    let other = ready(&private_tree("incarnation"));
    assert_eq!(
        advance(ready(&path), other.stamp()).unwrap_err(),
        StoreError::StoreReplaced
    );
    assert_eq!(ready(&path).stamp(), winner.stamp());
}

#[test]
fn maximum_generation_never_wraps() {
    let path = private_tree("overflow");
    drop(ready(&path));
    let writer = Connection::open(path.join(CACHE_FILE)).unwrap();
    writer
        .execute("UPDATE sr_cache_meta SET generation=?1", [i64::MAX])
        .unwrap();
    let store = ready(&path);
    let stamp = store.stamp();
    assert_eq!(
        advance(store, stamp).unwrap_err(),
        StoreError::GenerationExhausted
    );
    assert_eq!(ready(&path).stamp().generation(), i64::MAX as u64);
}

#[test]
fn cancellation_prevents_initialization_and_generation_writes() {
    let parent = private_tree("cancelled");
    let absent = parent.join("absent");
    let store = ready(&parent);
    let stamp = store.stamp();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    invocation.cancel_user(&cx);
    assert!(
        open_cache(
            &invocation,
            &cx,
            CacheAccess::Initialize,
            CacheLocation::Directory(absent.clone())
        )
        .is_err()
    );
    assert!(!absent.exists());
    assert!(store.advance_generation(&invocation, &cx, stamp).is_err());
    // Even cancellation and an invalid path cannot turn disabled persistence
    // into an attempted effect or an error.
    assert!(matches!(
        open_cache(
            &invocation,
            &cx,
            CacheAccess::Disabled,
            CacheLocation::Platform
        )
        .unwrap(),
        CacheOpen::Disabled
    ));
    assert!(invocation.shutdown());
    let reopened = ready(&parent);
    assert_eq!(reopened.stamp(), stamp);
    assert_eq!(advance(reopened, stamp).unwrap().stamp().generation(), 1);
}

#[test]
fn expired_work_window_does_not_create_a_directory() {
    let parent = private_tree("deadline");
    let path = parent.join("absent");
    let clock = EntryClock::capture_with(
        DurationMillis::new("total", 60, 3000).unwrap(),
        DurationMillis::new("cleanup", 20, 3000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    std::thread::sleep(Duration::from_millis(65));
    assert!(
        open_cache(
            &invocation,
            &cx,
            CacheAccess::Initialize,
            CacheLocation::Directory(path.clone())
        )
        .is_err()
    );
    assert!(!path.exists());
    let _ = invocation.shutdown();
    drop(ready(&path));
}

#[test]
fn oversized_cache_is_refused_before_sqlite_can_mutate_it() {
    let path = private_tree("quota");
    let file = create_file(&path.join(CACHE_FILE));
    file.set_len(CACHE_QUOTA_BYTES).unwrap();
    assert_eq!(
        open(&path, CacheAccess::Initialize).unwrap_err(),
        StoreError::Quota
    );
    assert_eq!(file.metadata().unwrap().len(), CACHE_QUOTA_BYTES);
    drop(ready(&private_tree("quota-success")));
}

#[test]
fn replacing_the_directory_is_detected_before_generation_update() {
    let root = private_tree("replacement");
    let path = root.join("current");
    let store = ready(&path);
    let stamp = store.stamp();
    fs::rename(&path, root.join("retained-original")).unwrap();
    let replacement = ready(&path);
    let replacement_stamp = replacement.stamp();
    assert_eq!(
        advance(store, stamp).unwrap_err(),
        StoreError::StoreReplaced
    );
    assert_eq!(ready(&path).stamp(), replacement_stamp);
}

#[test]
fn sidecar_permissions_are_checked_before_opening_an_existing_store() {
    let path = private_tree("sidecar-mode");
    let store = ready(&path);
    let stamp = store.stamp();
    for suffix in ["-wal", "-shm"] {
        let sidecar = path.join(format!("{CACHE_FILE}{suffix}"));
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            open(&path, CacheAccess::ExistingOnly).unwrap_err(),
            StoreError::Permissions
        );
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(ready(&path).stamp(), stamp);
    }
}

#[test]
#[ignore = "subprocess entry point; invoked by abrupt_process_exit_preserves_only_committed_generation"]
fn crash_writer_child() {
    let path = PathBuf::from(
        std::env::var_os("SR_TEST_STORAGE_CRASH_PATH").expect("test-only child path"),
    );
    let store = ready(&path);
    let stamp = store.stamp();
    let _committed = advance(store, stamp).unwrap();
    let writer = Connection::open(path.join(CACHE_FILE)).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE sr_cache_meta SET generation=99;")
        .unwrap();
    // Exit skips both SQLite and Rust destructors. This tests process death,
    // not power-loss durability or an injected simulation of commit success.
    std::process::exit(73);
}

#[test]
fn abrupt_process_exit_preserves_only_committed_generation() {
    let path = private_tree("crash");
    let original = ready(&path).stamp();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_writer_child", "--ignored", "--nocapture"])
        .env_clear()
        .env("SR_TEST_STORAGE_CRASH_PATH", &path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("owned crash-test child exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(status.code(), Some(73));
    let reopened = ready(&path);
    assert_eq!(reopened.stamp().generation(), original.generation() + 1);
    let committed = reopened.stamp();
    assert_eq!(
        advance(reopened, committed).unwrap().stamp().generation(),
        2
    );
    eprintln!(
        "{}",
        serde_json::json!({"case":"process-death", "stage":"recovery", "schema":CACHE_SCHEMA_VERSION,
        "elapsed_ms":start.elapsed().as_millis(), "result":"committed-only", "child_exit":73})
    );
}
