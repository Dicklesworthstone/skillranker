#![cfg(any(target_os = "linux", target_os = "macos"))]

use rusqlite::Connection;
use skillranker::cache::{CachedResponseEntry, RequestFingerprint, RequestStage};
use skillranker::jev::codec::Usage;
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
    // Schema v3 holds exactly metadata, key, responses and ownership leases.
    let mut tables = observer
        .prepare("SELECT name FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    tables.sort();
    assert_eq!(
        tables,
        [
            "sr_cache_key",
            "sr_cache_meta",
            "sr_cache_response",
            "sr_coordination_leases"
        ]
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
    // A 60 ms total against a 20 ms reserve leaves a 40 ms work budget, which
    // construction spends from; on a loaded shared worker it can spend all of it,
    // and the case then dies in `request_cx` with nothing wrong in the store
    // boundary it tests (sr-5n0b). Retry with a fresh clock rather than widen the
    // window, since the window is what makes the refusal below meaningful.
    let mut live = None;
    for _ in 0..16 {
        let clock = EntryClock::capture_with(
            DurationMillis::new("total", 60, 3000).unwrap(),
            DurationMillis::new("cleanup", 20, 3000).unwrap(),
        )
        .unwrap();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        if let Ok(cx) = invocation.request_cx() {
            live = Some((invocation, cx));
            break;
        }
        let _ = invocation.shutdown();
    }
    let (invocation, cx) =
        live.expect("no attempt in 16 left work budget after constructing a runtime");
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

const NOW: u64 = 1_800_000_000_000;

fn entry(stage: RequestStage, id: u8, received_at_unix_ms: u64, ttl: u32) -> CachedResponseEntry {
    CachedResponseEntry {
        stage,
        request_fingerprint: RequestFingerprint::from_bytes([id; 32]),
        response_bytes: format!("{{\"synthetic\":{id}}}").into_bytes(),
        received_at_unix_ms,
        ttl_seconds: ttl,
        model: "jev-latest".to_owned(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 100,
            output_tokens: 25,
        },
        attempt_id: None,
    }
}

fn record(
    store: CacheStore,
    namespace: [u8; 32],
    entry: CachedResponseEntry,
    now: u64,
) -> Result<CacheStore, StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.record_response(&invocation, &cx, namespace, entry, now);
    assert!(invocation.shutdown());
    result
}

fn read(
    store: CacheStore,
    namespace: [u8; 32],
    stage: RequestStage,
    id: u8,
) -> Result<(CacheStore, Option<CachedResponseEntry>), StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.response(
        &invocation,
        &cx,
        namespace,
        stage,
        RequestFingerprint::from_bytes([id; 32]),
    );
    assert!(invocation.shutdown());
    result
}

fn stored_key(path: &Path) -> Vec<u8> {
    Connection::open(path.join(CACHE_FILE))
        .unwrap()
        .query_row("SELECT key FROM sr_cache_key", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn the_fingerprint_key_is_random_stable_and_never_printed() {
    let (a, b) = (private_tree("key-a"), private_tree("key-b"));
    drop(ready(&a));
    drop(ready(&b));
    let first = stored_key(&a);
    assert_eq!(first.len(), 32);
    assert_ne!(first, vec![0; 32]);
    assert_ne!(first, stored_key(&b), "each store draws its own key");
    let reopened = ready(&a);
    assert_eq!(stored_key(&a), first, "reopening keeps the key");
    assert_eq!(
        format!("{:?}", reopened.fingerprint_key()),
        "CacheKey(<secret-key>)"
    );
    assert_eq!(
        fs::metadata(a.join(CACHE_FILE)).unwrap().mode() & 0o7777,
        0o600
    );
}

#[test]
fn responses_round_trip_only_by_exact_identity_and_generation() {
    let path = private_tree("responses");
    let namespace = [7; 32];
    let store = record(
        ready(&path),
        namespace,
        entry(RequestStage::Wide, 1, NOW, 600),
        NOW,
    )
    .unwrap();
    let (store, found) = read(store, namespace, RequestStage::Wide, 1).unwrap();
    let found = found.expect("the recorded response");
    let expected = entry(RequestStage::Wide, 1, NOW, 600);
    assert_eq!(found.response_bytes, expected.response_bytes);
    assert_eq!(found.received_at_unix_ms, NOW);
    assert_eq!(found.ttl_seconds, 600);
    assert_eq!(found.model, "jev-latest");
    assert_eq!(found.original_usage, expected.original_usage);
    // Another stage, fingerprint or namespace is a miss, never a neighbor.
    let (store, miss) = read(store, namespace, RequestStage::Rerank, 1).unwrap();
    assert!(miss.is_none());
    let (store, miss) = read(store, namespace, RequestStage::Wide, 2).unwrap();
    assert!(miss.is_none());
    let (store, miss) = read(store, [8; 32], RequestStage::Wide, 1).unwrap();
    assert!(miss.is_none());
    // A new generation makes earlier rows unreachable, and handles holding
    // the old stamp can neither read nor record.
    let stale_reader = ready(&path);
    let stale_writer = ready(&path);
    let advanced = advance(store, stale_reader.stamp()).unwrap();
    assert_eq!(
        read(stale_reader, namespace, RequestStage::Wide, 1).err(),
        Some(StoreError::StaleGeneration)
    );
    assert_eq!(
        record(
            stale_writer,
            namespace,
            entry(RequestStage::Wide, 9, NOW, 600),
            NOW
        )
        .err(),
        Some(StoreError::StaleGeneration)
    );
    let (_, gone) = read(advanced, namespace, RequestStage::Wide, 1).unwrap();
    assert!(gone.is_none(), "an earlier generation is never served");
}

#[test]
fn recording_prunes_expired_future_and_earlier_generation_rows() {
    let path = private_tree("prune");
    let namespace = [3; 32];
    let past = NOW - 700_000;
    let future = NOW + 60_000;
    let store = record(
        ready(&path),
        namespace,
        entry(RequestStage::Wide, 1, past, 600),
        past,
    )
    .unwrap();
    // At `future`, row 1 is past its TTL and is pruned.
    let store = record(
        store,
        namespace,
        entry(RequestStage::Wide, 2, future, 600),
        future,
    )
    .unwrap();
    // Back at NOW, row 2 is dated in the future and is pruned.
    drop(
        record(
            store,
            namespace,
            entry(RequestStage::Rerank, 3, NOW, 600),
            NOW,
        )
        .unwrap(),
    );
    let remaining: Vec<Vec<u8>> = Connection::open(path.join(CACHE_FILE))
        .unwrap()
        .prepare("SELECT fingerprint FROM sr_cache_response")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(remaining, vec![vec![3u8; 32]]);
}

#[test]
fn unbounded_entries_are_refused_before_any_write() {
    let path = private_tree("bounds");
    let namespace = [5; 32];
    let mut oversized = entry(RequestStage::Wide, 1, NOW, 600);
    oversized.response_bytes = vec![b' '; 2 * 1024 * 1024 + 1];
    let mut unnamed = entry(RequestStage::Wide, 1, NOW, 600);
    unnamed.model.clear();
    for bad in [
        entry(RequestStage::Wide, 1, NOW, 0),
        entry(RequestStage::Wide, 1, NOW, 601),
        oversized,
        unnamed,
    ] {
        assert_eq!(
            record(ready(&path), namespace, bad, NOW).err(),
            Some(StoreError::Quota)
        );
    }
    // Positive twin: a bounded entry records.
    let store = record(
        ready(&path),
        namespace,
        entry(RequestStage::Wide, 1, NOW, 600),
        NOW,
    )
    .unwrap();
    assert!(
        read(store, namespace, RequestStage::Wide, 1)
            .unwrap()
            .1
            .is_some()
    );
}

#[test]
fn a_version_one_store_is_refused_without_repair() {
    let path = private_tree("version-one");
    let db = raw_database(&path);
    db.execute_batch(
        "CREATE TABLE sr_cache_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        incarnation BLOB NOT NULL CHECK(length(incarnation)=16),
        generation INTEGER NOT NULL CHECK(generation>=0),
        schema_id TEXT NOT NULL
    ) STRICT;
    INSERT INTO sr_cache_meta VALUES (1, randomblob(16), 0, 'sr-cache-foundation-v1');
    PRAGMA application_id=1397900104;
    PRAGMA user_version=1;",
    )
    .unwrap();
    for access in [CacheAccess::ExistingOnly, CacheAccess::Initialize] {
        assert_eq!(
            open(&path, access).unwrap_err(),
            StoreError::IncompatibleSchema
        );
    }
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((version, tables), (1, 1), "nothing was added or rewritten");
}

#[test]
fn version_two_response_cache_is_preserved_without_automatic_migration() {
    let path = private_tree("version-two");
    let db = raw_database(&path);
    db.execute_batch(r#"CREATE TABLE sr_cache_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        incarnation BLOB NOT NULL CHECK(length(incarnation)=16),
        generation INTEGER NOT NULL CHECK(generation>=0),
        schema_id TEXT NOT NULL
    ) STRICT;
CREATE TABLE sr_cache_key (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        key BLOB NOT NULL CHECK(length(key)=32)
    ) STRICT;
CREATE TABLE sr_cache_response (
        generation INTEGER NOT NULL CHECK(generation>=0),
        namespace BLOB NOT NULL CHECK(length(namespace)=32),
        stage TEXT NOT NULL CHECK(stage IN ('wide','rerank')),
        fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32),
        response BLOB NOT NULL CHECK(length(response)<=2097152),
        received_at_unix_ms INTEGER NOT NULL CHECK(received_at_unix_ms>=0),
        ttl_seconds INTEGER NOT NULL CHECK(ttl_seconds BETWEEN 1 AND 600),
        model TEXT NOT NULL CHECK(length(model) BETWEEN 1 AND 256),
        model_revision TEXT CHECK(model_revision IS NULL OR length(model_revision)<=256),
        input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
        output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
        PRIMARY KEY (generation, namespace, stage, fingerprint)
    ) STRICT ;
INSERT INTO sr_cache_meta VALUES (1, zeroblob(16), 7, 'sr-cache-responses-v2');
INSERT INTO sr_cache_key VALUES (1, zeroblob(32));
INSERT INTO sr_cache_response VALUES (7, zeroblob(32), 'wide', zeroblob(32), x'010203', 1000, 600, 'jev-test', NULL, 1, 2);
PRAGMA application_id=1397900104;
PRAGMA user_version=2;
"#).unwrap();
    for access in [CacheAccess::ExistingOnly, CacheAccess::Initialize] {
        assert_eq!(
            open(&path, access).unwrap_err(),
            StoreError::IncompatibleSchema
        );
    }
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let body: Vec<u8> = db
        .query_row("SELECT response FROM sr_cache_response", [], |r| r.get(0))
        .unwrap();
    assert_eq!((version, tables, body), (2, 3, vec![1, 2, 3]));
}
