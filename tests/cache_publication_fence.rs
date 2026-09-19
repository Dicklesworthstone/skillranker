#![cfg(target_os = "linux")]
//! Real lease and production cache stores; no model or storage doubles.
use skillranker::cache::{
    CachedResponseEntry, CoordinationError, CoordinationKey, CoordinationPolicy, LeaderContext,
    LeaseAcquisition, LeaseCoordinator, RequestFingerprint, RequestStage, SqliteLeaseCoordinator,
};
use skillranker::jev::codec::Usage;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::{
    CacheAccess, CacheLocation, CacheOpen, CacheStore, StoreError, open_cache,
};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

#[test]
fn successor_cannot_acquire_during_publication_callback() {
    let dir = directory();
    let coordinator = SqliteLeaseCoordinator::open(dir.join("leases.sqlite3")).unwrap();
    let key = CoordinationKey::from_bytes([1; 32]);
    let policy = CoordinationPolicy::default();
    let leader = leading(coordinator.acquire(key, 1000, &policy).unwrap());
    let store = open(&dir);
    coordinator
        .with_active_lease(
            &leader,
            Duration::from_millis(25),
            || 1001,
            || {
                // An independent SQLite connection sees an expired lease, but cannot
                // replace it while the publisher owns the writer transaction.
                assert_eq!(
                    coordinator.acquire(key, 7000, &policy).unwrap_err(),
                    CoordinationError::StorageBusy
                );
                let invocation = ProcessInvocation::enter().unwrap();
                let cx = invocation.request_cx().unwrap();
                let stored = store
                    .record_response(&invocation, &cx, [3; 32], entry(b"owner-a"), now())
                    .unwrap();
                assert!(invocation.shutdown());
                assert_eq!(read(stored), b"owner-a");
            },
        )
        .unwrap()
        .unwrap();
    let successor = leading(coordinator.acquire(key, 7000, &policy).unwrap());
    assert!(successor.fencing_generation > leader.fencing_generation);
    let mut invoked = false;
    assert!(
        coordinator
            .with_active_lease(
                &leader,
                Duration::from_millis(25),
                || 1001,
                || {
                    invoked = true;
                }
            )
            .unwrap()
            .is_none()
    );
    assert!(!invoked, "stale owner callback executed");
}

#[test]
fn stale_owner_cannot_replace_successors_actual_cache_body() {
    let dir = directory();
    let path = dir.join("leases.sqlite3");
    let coordinator = SqliteLeaseCoordinator::open(&path).unwrap();
    let key = CoordinationKey::from_bytes([2; 32]);
    let policy = CoordinationPolicy::default();
    let a = leading(coordinator.acquire(key, now(), &policy).unwrap());
    let store = write(open(&dir), &path, &a, b"owner-a").unwrap();
    coordinator
        .complete(key, a.owner_token, a.fencing_generation, now())
        .unwrap();
    let b = leading(coordinator.force_reacquire(key, now(), &policy).unwrap());
    let store = write(store, &path, &b, b"owner-b").unwrap();
    assert_eq!(
        write(store, &path, &a, b"stale-a").unwrap_err(),
        StoreError::LeaseSuperseded
    );
    assert_eq!(read(open(&dir)), b"owner-b");
    coordinator
        .complete(key, b.owner_token, b.fencing_generation, now())
        .unwrap();
    assert_eq!(
        write(open(&dir), &path, &b, b"completed-b").unwrap_err(),
        StoreError::LeaseSuperseded
    );
    assert_eq!(read(open(&dir)), b"owner-b");
    let bodies: i64 = rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM sr_response_cache", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        bodies, 0,
        "coordination state must not hold response bodies"
    );
}

#[test]
fn expired_owner_does_not_create_a_cache_response() {
    let dir = directory();
    let path = dir.join("leases.sqlite3");
    let coordinator = SqliteLeaseCoordinator::open(&path).unwrap();
    let a = leading(
        coordinator
            .acquire(
                CoordinationKey::from_bytes([5; 32]),
                1000,
                &CoordinationPolicy::default(),
            )
            .unwrap(),
    );
    assert_eq!(
        write(open(&dir), &path, &a, b"expired").unwrap_err(),
        StoreError::LeaseSuperseded
    );
    let rows: i64 = rusqlite::Connection::open(dir.join("cache.sqlite3"))
        .unwrap()
        .query_row("SELECT count(*) FROM sr_cache_response", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn optional_cache_refusal_and_unavailable_lease_are_distinct() {
    let dir = directory();
    let path = dir.join("leases.sqlite3");
    let coordinator = SqliteLeaseCoordinator::open(&path).unwrap();
    let leader = leading(
        coordinator
            .acquire(
                CoordinationKey::from_bytes([6; 32]),
                now(),
                &CoordinationPolicy::default(),
            )
            .unwrap(),
    );
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let mut oversized = entry(b"not-written");
    oversized.response_bytes = vec![0; skillranker::jev::codec::MAX_RESPONSE_BYTES + 1];
    let error = open(&dir)
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
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(
        write(open(&dir), &path, &leader, b"blocked").unwrap_err(),
        StoreError::LeaseUnavailable
    );
    lock.execute_batch("ROLLBACK").unwrap();
    let cache = rusqlite::Connection::open(dir.join("cache.sqlite3")).unwrap();
    assert_eq!(
        cache
            .query_row("SELECT count(*) FROM sr_cache_response", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let stored = write(open(&dir), &path, &leader, b"healthy").unwrap();
    assert_eq!(read(stored), b"healthy");
}
