#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Production CacheStore lease contracts. These tests use the same qualified
//! connection and transaction primitives as rank, not another coordinator.
//! The arbitrary storage bodies below establish storage delivery only;
//! real CLI/provider-pair, deadline and usage proof lives in real_rank_coordination.

use rusqlite::Connection;
use skillranker::cache::{
    CacheKey, CacheNamespace, CachedResponseEntry, CoordinationKey, FencingGeneration,
    LeaderContext, LeaseAcquisition, PublishOutcome, RequestFingerprint, RequestStage,
};
use skillranker::identity::{HarnessId, SessionId};
use skillranker::jev::codec::Usage;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::{
    CACHE_FILE, CacheAccess, CacheLocation, CacheOpen, CacheStore, open_cache,
};
use std::fs::DirBuilder;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
const NS: [u8; 32] = [3; 32];
const KEY: CoordinationKey = CoordinationKey::from_bytes([4; 32]);

// Drain both pipes concurrently, retaining a bounded prefix even when a child
// fails or floods diagnostics. The guard also reaps the child on parent panic.
struct CapturedChild {
    child: std::process::Child,
    readers: Vec<std::thread::JoinHandle<std::io::Result<Vec<u8>>>>,
}

impl CapturedChild {
    fn drain(
        reader: impl Read + Send + 'static,
    ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut retained = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    return Ok(retained);
                }
                let keep = count.min((64 * 1024_usize).saturating_sub(retained.len()));
                retained.extend_from_slice(&buffer[..keep]);
            }
        })
    }

    fn new(mut child: std::process::Child) -> Self {
        let stdout = Self::drain(child.stdout.take().expect("piped child stdout"));
        let stderr = Self::drain(child.stderr.take().expect("piped child stderr"));
        Self {
            child,
            readers: vec![stdout, stderr],
        }
    }

    fn diagnostics(&mut self) -> String {
        let _ = self.child.kill();
        self.child.wait().expect("reap coordination child");
        self.readers
            .drain(..)
            .enumerate()
            .map(|(index, reader)| {
                let bytes = reader
                    .join()
                    .expect("diagnostic reader panicked")
                    .expect("read child diagnostics");
                format!("stream {index}: {}", String::from_utf8_lossy(&bytes))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

fn private_tree(case: &str) -> PathBuf {
    let path = Path::new("/tmp").join(format!(
        "sr-coord-{case}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    DirBuilder::new().mode(0o700).create(&path).unwrap();
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
        panic!("cache unavailable");
    };
    assert!(invocation.shutdown());
    *store
}

fn acquire(
    store: CacheStore,
    key: CoordinationKey,
    refresh: bool,
) -> (CacheStore, LeaseAcquisition) {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store.acquire_lease(&invocation, &cx, key, refresh).unwrap();
    assert!(invocation.shutdown());
    result
}

fn leading(outcome: LeaseAcquisition) -> LeaderContext {
    match outcome {
        LeaseAcquisition::Leading(leader) => leader,
        other => panic!("expected leader, got {other:?}"),
    }
}

fn complete(store: CacheStore, leader: &LeaderContext) -> (CacheStore, PublishOutcome) {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = store
        .complete_lease(&invocation, &cx, leader.clone())
        .unwrap();
    assert!(invocation.shutdown());
    result
}

fn entry(stage: RequestStage) -> CachedResponseEntry {
    CachedResponseEntry {
        stage,
        request_fingerprint: RequestFingerprint::from_bytes(
            [match stage {
                RequestStage::Wide => 5,
                RequestStage::Rerank => 6,
            }; 32],
        ),
        response_bytes: stage.as_str().as_bytes().to_vec(),
        received_at_unix_ms: u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap(),
        ttl_seconds: 600,
        model: "synthetic-storage".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 2,
            output_tokens: 3,
        },
        attempt_id: None,
    }
}

#[test]
fn qualified_store_refresh_does_not_preempt_an_active_owner() {
    let path = private_tree("refresh");
    let (store, first) = acquire(open(&path), KEY, false);
    let first = leading(first);
    assert_eq!(first.fencing_generation, FencingGeneration(1));
    let (store, following) = acquire(store, KEY, true);
    let LeaseAcquisition::Following(follower) = following else {
        panic!("active owner preempted");
    };
    assert_eq!(follower.leader_generation, first.fencing_generation);
    let (store, outcome) = complete(store, &first);
    assert_eq!(outcome, PublishOutcome::Published);
    let (store, completed) = acquire(store, KEY, false);
    assert!(matches!(completed, LeaseAcquisition::AlreadyCompleted));
    let (store, second) = acquire(store, KEY, true);
    let second = leading(second);
    assert_eq!(second.fencing_generation, FencingGeneration(2));
    assert_ne!(second.owner_token, first.owner_token);
    let (store, obsolete) = complete(store, &first);
    assert!(matches!(
        obsolete,
        PublishOutcome::Superseded {
            current_generation: Some(FencingGeneration(2)),
            ..
        }
    ));
    assert_eq!(complete(store, &second).1, PublishOutcome::Published);
}

#[test]
fn namespace_keys_acquire_independent_production_leases() {
    let secret = CacheKey::from_bytes([7; 32]);
    let fp = RequestFingerprint::from_bytes([8; 32]);
    let a = CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_session(SessionId::new("session-a").unwrap());
    let b = CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_session(SessionId::new("session-b").unwrap());
    let a = CoordinationKey::compute(&secret, &a, &fp);
    let b = CoordinationKey::compute(&secret, &b, &fp);
    assert_ne!(a, b);
    let path = private_tree("namespaces");
    let (store, first) = acquire(open(&path), a, false);
    let first = leading(first);
    let (store, second) = acquire(store, b, false);
    let second = leading(second);
    assert_ne!(first.owner_token, second.owner_token);
    assert_eq!(complete(store, &first).1, PublishOutcome::Published);
    assert_eq!(complete(open(&path), &second).1, PublishOutcome::Published);
}

#[test]
fn metadata_completion_creates_neither_hidden_bodies_nor_an_alternate_store() {
    let path = private_tree("metadata");
    let (store, leader) = acquire(open(&path), KEY, false);
    let leader = leading(leader);
    assert_eq!(complete(store, &leader).1, PublishOutcome::Published);
    let db = Connection::open(path.join(CACHE_FILE)).unwrap();
    let columns: Vec<String> = db
        .prepare("PRAGMA table_info(sr_coordination_leases)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "coordination_key",
            "owner_token",
            "fencing_generation",
            "acquired_at_unix_ms",
            "expires_at_unix_ms",
            "attempt_id",
            "is_completed"
        ]
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM sr_cache_response", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='sr_response_cache'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(!path.join("leases.sqlite3").exists());
}

#[test]
#[ignore = "owned subprocess entry point for qualified_store_delivers_both_stages_across_processes"]
fn qualified_store_child_reader() {
    let path = PathBuf::from(std::env::var_os("SR_TEST_COORD_STORE").expect("store path"));
    let ready = PathBuf::from(std::env::var_os("SR_TEST_COORD_READY").expect("ready path"));
    let (store, outcome) = acquire(open(&path), KEY, false);
    assert!(matches!(outcome, LeaseAcquisition::Following(_)));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(ready)
        .unwrap()
        .write_all(b"following")
        .unwrap();
    let start = Instant::now();
    let mut store = store;
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "parent never completed"
        );
        let invocation = ProcessInvocation::enter().unwrap();
        let cx = invocation.request_cx().unwrap();
        let (next, lease) = store.lease(&invocation, &cx, KEY).unwrap();
        assert!(invocation.shutdown());
        store = next;
        if lease.unwrap().is_completed {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    for stage in [RequestStage::Wide, RequestStage::Rerank] {
        let expected = entry(stage);
        let (next, response) = store
            .response(&invocation, &cx, NS, stage, expected.request_fingerprint)
            .unwrap();
        store = next;
        let response = response.expect("completed lease must expose both bodies");
        assert_eq!(response.response_bytes, expected.response_bytes);
        assert_eq!(response.original_usage, expected.original_usage);
    }
    assert!(invocation.shutdown());
}

#[test]
fn qualified_store_delivers_both_stages_across_processes() {
    let path = private_tree("processes");
    let ready = path.join("reader.ready");
    let (store, outcome) = acquire(open(&path), KEY, false);
    let leader = leading(outcome);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("SR_TEST_COORD_STORE", &path)
        .env("SR_TEST_COORD_READY", &ready)
        .args([
            "--exact",
            "qualified_store_child_reader",
            "--ignored",
            "--nocapture",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = CapturedChild::new(child);
    let start = Instant::now();
    while !ready.exists() {
        if child.child.try_wait().unwrap().is_some() || start.elapsed() > Duration::from_secs(3) {
            panic!("reader did not contend: {}", child.diagnostics());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let store = store
        .publish_evaluation(
            &invocation,
            &cx,
            NS,
            entry(RequestStage::Wide),
            Some(entry(RequestStage::Rerank)),
            Some((path.join(CACHE_FILE), leader)),
        )
        .unwrap();
    assert!(invocation.shutdown());
    drop(store);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(8) {
            panic!("reader stalled: {}", child.diagnostics());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "reader failed: {}", child.diagnostics());
}
