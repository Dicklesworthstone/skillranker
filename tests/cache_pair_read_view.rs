#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Production cache reads over real WAL stores; no timing sleeps or test doubles.

use skillranker::cache::{CachedResponseEntry, RequestFingerprint, RequestStage};
use skillranker::jev::codec::Usage;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::{
    CacheAccess, CacheLocation, CacheOpen, CacheStore, StoreError, open_cache,
};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const NAMESPACE: [u8; 32] = [3; 32];

fn directory() -> PathBuf {
    let path = PathBuf::from(format!(
        "/tmp/sr-cache-read-view-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

fn open(path: &Path) -> CacheStore {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let CacheOpen::Ready(store) = open_cache(
        &invocation,
        &cx,
        CacheAccess::Initialize,
        CacheLocation::Directory(path.to_owned()),
    )
    .unwrap() else {
        panic!("cache unavailable")
    };
    assert!(invocation.shutdown());
    *store
}

fn write(store: CacheStore, stage: RequestStage, body: &[u8]) -> CacheStore {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let entry = CachedResponseEntry {
        stage,
        request_fingerprint: RequestFingerprint::from_bytes([4; 32]),
        response_bytes: body.to_vec(),
        received_at_unix_ms: now,
        ttl_seconds: 600,
        model: "jev-test".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        attempt_id: None,
    };
    let store = store
        .record_response(&invocation, &cx, NAMESPACE, entry, now)
        .unwrap();
    assert!(invocation.shutdown());
    store
}

fn read(
    store: CacheStore,
    namespace: [u8; 32],
    stage: RequestStage,
) -> Result<(CacheStore, Option<CachedResponseEntry>), StoreError> {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.response(
        &invocation,
        &cx,
        namespace,
        stage,
        RequestFingerprint::from_bytes([4; 32]),
    );
    assert!(invocation.shutdown());
    result
}

fn seed() -> (PathBuf, CacheStore) {
    let path = directory();
    let store = write(open(&path), RequestStage::Wide, b"old-wide");
    let store = write(store, RequestStage::Rerank, b"old-rerank");
    (path, store)
}

fn replace_pair(writer: &rusqlite::Connection) {
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    for (stage, body) in [
        ("wide", b"new-wide".as_slice()),
        ("rerank", b"new-rerank".as_slice()),
    ] {
        assert_eq!(
            writer
                .execute(
                    "UPDATE sr_cache_response SET response=?1 WHERE namespace=?2 AND stage=?3",
                    rusqlite::params![body, &NAMESPACE[..], stage],
                )
                .unwrap(),
            1
        );
    }
}

fn body(entry: Option<CachedResponseEntry>) -> Vec<u8> {
    entry.unwrap().response_bytes
}

#[test]
fn unchanged_wide_and_rerank_are_still_cache_hits() {
    let (_, store) = seed();
    let (store, wide) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    let (_, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    assert_eq!(body(wide), b"old-wide");
    assert_eq!(body(rerank), b"old-rerank");
}

#[test]
fn committed_replacement_between_reads_is_rejected_and_fresh_reopen_succeeds() {
    let (path, store) = seed();
    let writer = rusqlite::Connection::open(path.join("cache.sqlite3")).unwrap();
    let mode: String = writer
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let (store, wide) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    assert_eq!(body(wide), b"old-wide");
    replace_pair(&writer);
    writer.execute_batch("COMMIT").unwrap();
    assert_eq!(
        read(store, NAMESPACE, RequestStage::Rerank).unwrap_err(),
        StoreError::StaleGeneration
    );
    drop(writer);
    let (store, wide) = read(open(&path), NAMESPACE, RequestStage::Wide).unwrap();
    let (_, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    assert_eq!(body(wide), b"new-wide");
    assert_eq!(body(rerank), b"new-rerank");
}

#[test]
fn rolled_back_peer_write_does_not_invalidate_the_pair() {
    let (path, store) = seed();
    let writer = rusqlite::Connection::open(path.join("cache.sqlite3")).unwrap();
    let (store, wide) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    replace_pair(&writer);
    writer.execute_batch("ROLLBACK").unwrap();
    let (_, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    assert_eq!(body(wide), b"old-wide");
    assert_eq!(body(rerank), b"old-rerank");
}

#[test]
fn uncommitted_wal_write_remains_invisible_to_the_reader() {
    let (path, store) = seed();
    let writer = rusqlite::Connection::open(path.join("cache.sqlite3")).unwrap();
    let (store, wide) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    replace_pair(&writer);
    let (_, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(body(wide), b"old-wide");
    assert_eq!(body(rerank), b"old-rerank");
}

#[test]
fn same_connection_write_cannot_hide_behind_unchanged_data_version() {
    let (_, store) = seed();
    let (store, wide) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    assert_eq!(body(wide), b"old-wide");
    let store = write(store, RequestStage::Rerank, b"new-rerank");
    assert_eq!(
        read(store, NAMESPACE, RequestStage::Rerank).unwrap_err(),
        StoreError::StaleGeneration
    );
}

#[test]
fn a_new_wide_lookup_starts_a_fresh_view_on_the_same_connection() {
    let (path, store) = seed();
    let writer = rusqlite::Connection::open(path.join("cache.sqlite3")).unwrap();
    let (store, _) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    replace_pair(&writer);
    writer.execute_batch("COMMIT").unwrap();
    let (store, wide) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    let (_, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    assert_eq!(body(wide), b"new-wide");
    assert_eq!(body(rerank), b"new-rerank");
}

#[test]
fn read_only_peer_does_not_invalidate_the_pair() {
    let (path, store) = seed();
    let peer = rusqlite::Connection::open(path.join("cache.sqlite3")).unwrap();
    let (store, _) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    assert_eq!(
        peer.query_row("SELECT count(*) FROM sr_cache_response", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let (_, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    assert_eq!(body(rerank), b"old-rerank");
}

#[test]
fn standalone_rerank_and_other_namespace_do_not_inherit_a_wide_view() {
    let (path, store) = seed();
    let (store, rerank) = read(store, NAMESPACE, RequestStage::Rerank).unwrap();
    assert_eq!(body(rerank), b"old-rerank");
    let (store, _) = read(store, NAMESPACE, RequestStage::Wide).unwrap();
    let writer = rusqlite::Connection::open(path.join("cache.sqlite3")).unwrap();
    replace_pair(&writer);
    writer.execute_batch("COMMIT").unwrap();
    let (_, other) = read(store, [9; 32], RequestStage::Rerank).unwrap();
    assert!(other.is_none());
}
