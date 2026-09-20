#![cfg(target_os = "linux")]
//! Real lease and production cache stores; no model or storage doubles.
use skillranker::cache::{
    CachedResponseEntry, CoordinationKey, LeaderContext, LeaseAcquisition, RequestFingerprint,
    RequestStage,
};
use skillranker::jev::codec::Usage;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::{
    CacheAccess, CacheLocation, CacheOpen, CacheStore, StoreError, open_cache,
};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}
fn directory() -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/sr-cache-fence-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    dir
}
fn leading(value: LeaseAcquisition) -> LeaderContext {
    match value {
        LeaseAcquisition::Leading(leader) => leader,
        _ => panic!("expected leader"),
    }
}
fn open(dir: &Path) -> CacheStore {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let CacheOpen::Ready(store) = open_cache(
        &invocation,
        &cx,
        CacheAccess::Initialize,
        CacheLocation::Directory(dir.to_owned()),
    )
    .unwrap() else {
        panic!("cache unavailable")
    };
    assert!(invocation.shutdown());
    *store
}
fn entry(body: &[u8]) -> CachedResponseEntry {
    CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: RequestFingerprint::from_bytes([4; 32]),
        response_bytes: body.to_vec(),
        received_at_unix_ms: now(),
        ttl_seconds: 600,
        model: "jev-test".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        attempt_id: None,
    }
}
fn write(
    store: CacheStore,
    path: &Path,
    leader: &LeaderContext,
    body: &[u8],
) -> Result<CacheStore, StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.record_response_fenced(
        &invocation,
        &cx,
        [3; 32],
        entry(body),
        (path.to_owned(), leader.clone()),
    );
    assert!(invocation.shutdown());
    result
}
fn read(store: CacheStore) -> Vec<u8> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let (_, value) = store
        .response(
            &invocation,
            &cx,
            [3; 32],
            RequestStage::Wide,
            RequestFingerprint::from_bytes([4; 32]),
        )
        .unwrap();
    assert!(invocation.shutdown());
    value.unwrap().response_bytes
}

fn acquire(
    store: CacheStore,
    key: CoordinationKey,
    refresh: bool,
) -> Result<(CacheStore, LeaderContext), StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store
        .acquire_lease(&invocation, &cx, key, refresh)
        .map(|(store, result)| (store, leading(result)));
    assert!(invocation.shutdown());
    result
}
fn complete(store: CacheStore, leader: &LeaderContext) -> CacheStore {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let (store, outcome) = store
        .complete_lease(&invocation, &cx, leader.clone())
        .unwrap();
    assert_eq!(outcome, skillranker::cache::PublishOutcome::Published);
    assert!(invocation.shutdown());
    store
}

#[test]
fn publication_and_acquisition_share_the_actual_cache_writer() {
    let dir = directory();
    let path = dir.join("cache.sqlite3");
    let key = CoordinationKey::from_bytes([1; 32]);
    let (store, leader) = acquire(open(&dir), key, false).unwrap();
    let contender = open(&dir);
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(
        write(store, &path, &leader, b"blocked").unwrap_err(),
        StoreError::Busy
    );
    assert_eq!(
        acquire(contender, CoordinationKey::from_bytes([9; 32]), false).unwrap_err(),
        StoreError::Busy
    );
    assert_eq!(
        lock.query_row("SELECT count(*) FROM sr_cache_response", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    lock.execute_batch("ROLLBACK").unwrap();
    let stored = write(open(&dir), &path, &leader, b"owner-a").unwrap();
    assert_eq!(read(stored), b"owner-a");
    assert!(
        !dir.join("leases.sqlite3").exists(),
        "separate lease database created"
    );
    let helper_bodies: i64 = lock
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='sr_response_cache'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        helper_bodies, 0,
        "incompatible helper response schema created"
    );
}

#[test]
fn stale_owner_cannot_replace_successors_actual_cache_body() {
    let dir = directory();
    let path = dir.join("cache.sqlite3");
    let key = CoordinationKey::from_bytes([2; 32]);
    let (store, a) = acquire(open(&dir), key, false).unwrap();
    let store = write(store, &path, &a, b"owner-a").unwrap();
    let store = complete(store, &a);
    let (store, b) = acquire(store, key, true).unwrap();
    let store = write(store, &path, &b, b"owner-b").unwrap();
    assert_eq!(
        write(store, &path, &a, b"stale-a").unwrap_err(),
        StoreError::LeaseSuperseded
    );
    assert_eq!(read(open(&dir)), b"owner-b");
    let store = complete(open(&dir), &b);
    assert_eq!(
        write(store, &path, &b, b"completed-b").unwrap_err(),
        StoreError::LeaseSuperseded
    );
    assert_eq!(read(open(&dir)), b"owner-b");
}

#[test]
fn expired_owner_does_not_create_a_cache_response() {
    let dir = directory();
    let path = dir.join("cache.sqlite3");
    let (store, a) = acquire(open(&dir), CoordinationKey::from_bytes([5; 32]), false).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE sr_coordination_leases SET acquired_at_unix_ms=0, expires_at_unix_ms=0",
        [],
    )
    .unwrap();
    assert_eq!(
        write(store, &path, &a, b"expired").unwrap_err(),
        StoreError::LeaseSuperseded
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM sr_cache_response", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn optional_recording_errors_are_distinct_from_supersession() {
    let dir = directory();
    let path = dir.join("cache.sqlite3");
    let (store, leader) = acquire(open(&dir), CoordinationKey::from_bytes([8; 32]), false).unwrap();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let mut oversized = entry(b"not-written");
    oversized.response_bytes = vec![0; skillranker::jev::codec::MAX_RESPONSE_BYTES + 1];
    let error = store
        .record_response_fenced(
            &invocation,
            &cx,
            [3; 32],
            oversized,
            (path.clone(), leader.clone()),
        )
        .unwrap_err();
    assert_eq!(error, StoreError::Quota);
    assert!(invocation.shutdown());
    assert_eq!(
        write(
            open(&dir),
            &dir.join("leases.sqlite3"),
            &leader,
            b"wrong-store"
        )
        .unwrap_err(),
        StoreError::LeaseUnavailable
    );
    assert!(!dir.join("leases.sqlite3").exists());
    let stored = write(open(&dir), &path, &leader, b"healthy").unwrap();
    assert_eq!(read(stored), b"healthy");
}
