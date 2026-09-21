#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Production CacheStore/WAL publication. No provider calls or storage doubles.

use skillranker::cache::{
    CachedResponseEntry, CoordinationKey, LeaderContext, LeaseAcquisition, RequestFingerprint,
    RequestStage,
};
use skillranker::jev::codec::Usage;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::{
    CACHE_FILE, CacheAccess, CacheLocation, CacheOpen, CacheStore, StoreError, open_cache,
};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const NS: [u8; 32] = [3; 32];
const WIDE: RequestFingerprint = RequestFingerprint::from_bytes([4; 32]);
const RERANK: RequestFingerprint = RequestFingerprint::from_bytes([5; 32]);

fn now() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn directory() -> PathBuf {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = PathBuf::from(format!("/tmp/sr-atomic-cache-{}-{nonce}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

fn open(path: &Path) -> CacheStore {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let CacheOpen::Ready(store) = open_cache(
        &invocation, &cx, CacheAccess::Initialize, CacheLocation::Directory(path.to_owned()),
    ).unwrap() else {
        panic!("cache unavailable");
    };
    assert!(invocation.shutdown());
    *store
}

fn entry(stage: RequestStage, body: &[u8]) -> CachedResponseEntry {
    CachedResponseEntry {
        stage,
        request_fingerprint: match stage { RequestStage::Wide => WIDE, RequestStage::Rerank => RERANK },
        response_bytes: body.to_vec(),
        received_at_unix_ms: now(),
        ttl_seconds: 600,
        model: "jev-test".to_owned(),
        model_revision: None,
        original_usage: Usage { input_tokens: 1, output_tokens: 1 },
        attempt_id: None,
    }
}

fn publish(
    store: CacheStore,
    namespace: [u8; 32],
    wide: CachedResponseEntry,
    rerank: Option<CachedResponseEntry>,
    fence: Option<(PathBuf, LeaderContext)>,
) -> Result<CacheStore, StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.publish_evaluation(&invocation, &cx, namespace, wide, rerank, fence);
    assert!(invocation.shutdown());
    result
}

fn seed(path: &Path) {
    drop(publish(
        open(path), NS, entry(RequestStage::Wide, b"old-wide"),
        Some(entry(RequestStage::Rerank, b"old-rerank")), None,
    ).unwrap());
}

fn rows(path: &Path, namespace: [u8; 32]) -> Vec<(String, Vec<u8>)> {
    let db = rusqlite::Connection::open(path.join(CACHE_FILE)).unwrap();
    let mut query = db.prepare(
        "SELECT stage,response FROM sr_cache_response WHERE namespace=?1 ORDER BY stage",
    ).unwrap();
    query.query_map([&namespace[..]], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap().collect::<Result<_, _>>().unwrap()
}

fn old_rows() -> Vec<(String, Vec<u8>)> {
    vec![("rerank".into(), b"old-rerank".to_vec()), ("wide".into(), b"old-wide".to_vec())]
}

fn acquire(path: &Path) -> (CacheStore, LeaderContext) {
    let store = open(path);
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let (store, acquisition) = store.acquire_lease(
        &invocation, &cx, CoordinationKey::from_bytes([7; 32]), true,
    ).unwrap();
    let LeaseAcquisition::Leading(leader) = acquisition else { panic!("expected leader") };
    assert!(invocation.shutdown());
    (store, leader)
}

fn completed(path: &Path) -> bool {
    let db = rusqlite::Connection::open(path.join(CACHE_FILE)).unwrap();
    db.query_row("SELECT is_completed FROM sr_coordination_leases", [], |r| r.get(0)).unwrap()
}

#[test]
fn pair_and_completion_survive_close_and_reopen_together() {
    let path = directory();
    seed(&path);
    let (store, leader) = acquire(&path);
    drop(publish(
        store, NS, entry(RequestStage::Wide, b"new-wide"),
        Some(entry(RequestStage::Rerank, b"new-rerank")),
        Some((path.join(CACHE_FILE), leader)),
    ).unwrap());
    assert!(completed(&path));
    assert_eq!(rows(&path, NS), vec![
        ("rerank".into(), b"new-rerank".to_vec()), ("wide".into(), b"new-wide".to_vec()),
    ]);
    let store = open(&path);
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let (store, wide) = store.response(&invocation, &cx, NS, RequestStage::Wide, WIDE).unwrap();
    let (_, rerank) = store.response(&invocation, &cx, NS, RequestStage::Rerank, RERANK).unwrap();
    assert_eq!(wide.unwrap().response_bytes, b"new-wide");
    assert_eq!(rerank.unwrap().response_bytes, b"new-rerank");
    assert!(invocation.shutdown());
}

#[test]
fn failed_second_row_preserves_previous_pair_and_active_lease() {
    let path = directory();
    seed(&path);
    let (store, leader) = acquire(&path);
    let mut invalid = entry(RequestStage::Rerank, b"never-committed");
    // SQL integer conversion occurs when writing the second row, after the
    // namespace delete and first row insert. All of them must roll back.
    invalid.original_usage.input_tokens = u64::MAX;
    assert_eq!(publish(
        store, NS, entry(RequestStage::Wide, b"never-committed"), Some(invalid),
        Some((path.join(CACHE_FILE), leader.clone())),
    ).unwrap_err(), StoreError::Quota);
    assert_eq!(rows(&path, NS), old_rows());
    assert!(!completed(&path));
    drop(publish(
        open(&path), NS, entry(RequestStage::Wide, b"retry-wide"),
        Some(entry(RequestStage::Rerank, b"retry-rerank")),
        Some((path.join(CACHE_FILE), leader)),
    ).unwrap());
    assert!(completed(&path));
}

#[test]
fn wide_only_publication_removes_old_rerank_and_completes() {
    let path = directory();
    seed(&path);
    let (store, leader) = acquire(&path);
    drop(publish(store, NS, entry(RequestStage::Wide, b"low-need"), None,
                 Some((path.join(CACHE_FILE), leader))).unwrap());
    assert_eq!(rows(&path, NS), vec![("wide".into(), b"low-need".to_vec())]);
    assert!(completed(&path));
}

#[test]
fn replacing_shared_rerank_key_removes_other_wide_fingerprints() {
    let path = directory();
    seed(&path);
    let mut wide = entry(RequestStage::Wide, b"different-wide");
    wide.request_fingerprint = RequestFingerprint::from_bytes([9; 32]);
    drop(publish(open(&path), NS, wide,
                 Some(entry(RequestStage::Rerank, b"same-key-new-rerank")), None).unwrap());
    let db = rusqlite::Connection::open(path.join(CACHE_FILE)).unwrap();
    let old: i64 = db.query_row(
        "SELECT count(*) FROM sr_cache_response WHERE fingerprint=?1",
        [&WIDE.as_bytes()[..]], |r| r.get(0),
    ).unwrap();
    assert_eq!(old, 0);
    assert_eq!(rows(&path, NS).len(), 2);
}

#[test]
fn another_namespace_keeps_its_complete_pair() {
    let path = directory();
    seed(&path);
    drop(publish(open(&path), [8; 32], entry(RequestStage::Wide, b"other-wide"),
                 Some(entry(RequestStage::Rerank, b"other-rerank")), None).unwrap());
    assert_eq!(rows(&path, NS), old_rows());
    assert_eq!(rows(&path, [8; 32]).len(), 2);
}

#[test]
fn busy_writer_does_not_replace_either_stage_or_complete_lease() {
    let path = directory();
    seed(&path);
    let (store, leader) = acquire(&path);
    let lock = rusqlite::Connection::open(path.join(CACHE_FILE)).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(publish(store, NS, entry(RequestStage::Wide, b"blocked-wide"),
                      Some(entry(RequestStage::Rerank, b"blocked-rerank")),
                      Some((path.join(CACHE_FILE), leader))).unwrap_err(), StoreError::Busy);
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(rows(&path, NS), old_rows());
    assert!(!completed(&path));
}

#[test]
fn completed_publisher_cannot_replace_the_pair_again() {
    let path = directory();
    seed(&path);
    let (store, leader) = acquire(&path);
    let store = publish(store, NS, entry(RequestStage::Wide, b"old-wide"),
                        Some(entry(RequestStage::Rerank, b"old-rerank")),
                        Some((path.join(CACHE_FILE), leader.clone()))).unwrap();
    assert_eq!(publish(store, NS, entry(RequestStage::Wide, b"repeat-wide"),
                      Some(entry(RequestStage::Rerank, b"repeat-rerank")),
                      Some((path.join(CACHE_FILE), leader))).unwrap_err(), StoreError::LeaseSuperseded);
    assert_eq!(rows(&path, NS), old_rows());
    assert!(completed(&path));
}

#[test]
fn wrong_stage_model_and_combined_size_fail_before_replacement() {
    let path = directory();
    seed(&path);
    for wrong in 0..3 {
        let wide = entry(RequestStage::Wide, b"rejected-wide");
        let mut rerank = entry(RequestStage::Rerank, b"rejected-rerank");
        match wrong {
            0 => rerank.stage = RequestStage::Wide,
            1 => rerank.model = "different-model".into(),
            _ => rerank.model_revision = Some("different-revision".into()),
        }
        assert_eq!(publish(open(&path), NS, wide, Some(rerank), None).unwrap_err(),
                   StoreError::InvalidRecord);
        assert_eq!(rows(&path, NS), old_rows());
    }
    let mut wide = entry(RequestStage::Wide, b"x");
    wide.response_bytes = vec![b'x'; skillranker::jev::codec::MAX_RESPONSE_BYTES];
    assert_eq!(publish(open(&path), NS, wide, Some(entry(RequestStage::Rerank, b"x")), None)
               .unwrap_err(), StoreError::Quota);
    assert_eq!(rows(&path, NS), old_rows());
}

#[test]
fn wrong_store_fence_leaves_both_databases_untouched() {
    let path = directory();
    seed(&path);
    let (store, leader) = acquire(&path);
    let wrong = path.join("leases.sqlite3");
    assert_eq!(publish(store, NS, entry(RequestStage::Wide, b"rejected"), None,
                      Some((wrong.clone(), leader))).unwrap_err(), StoreError::LeaseUnavailable);
    assert_eq!(rows(&path, NS), old_rows());
    assert!(!wrong.exists());
    assert!(!completed(&path));
}
