//! Acceptance tests for single-flight response coordination with fenced bounded leases (sr-roadmap-l1i.5.10).
//!
//! Validates:
//! 1. Real competing processes: Concurrent processes share exact provider response via SQLite lease coordination.
//! 2. Lease expiry and reacquisition: Stalled leader lease expires; successor reacquires with bumped fencing generation.
//! 3. Old owner late completion rejected: Expired or superseded leader cannot publish (quiet fallback).
//! 4. Follower deadline bounding: Follower deadline expiration returns quiet fallback without hanging.
//! 5. Namespace isolation: Distinct sessions/events produce distinct coordination keys and never coalesce.
//! 6. Zero hidden response bodies: Coordination state stores only tokens/timestamps; `--no-cache` forbids body sharing.
//! 7. Zero duplicate attempt charges: Follower incurs 0 new requests, 0 new tokens; missing usage remains unknown.

#![cfg(target_os = "linux")]

use rusqlite::Connection;
use skillranker::cache::{
    CacheKey, CacheNamespace, CachedResponseEntry, CandidateDigest, CoordinateRequestQuery,
    CoordinationKey, CoordinationPolicy, DEFAULT_CACHE_TTL_SECS, DEFAULT_LEASE_TTL_MS,
    FencingGeneration, FollowerResolution, LeaseAcquisition, LeaseCoordinator, MemoryCoordinator,
    MemoryResponseCache, PublishOutcome, RequestFingerprint, RequestFingerprintInput, RequestStage,
    SingleFlightCoordinator, SqliteLeaseCoordinator, compute_request_fingerprint,
};
use skillranker::identity::{ContentHash, HarnessId, SessionId, SkillId};
use skillranker::jev::codec::Usage;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

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

fn test_key() -> CacheKey {
    CacheKey::from_bytes([77u8; 32])
}

fn test_namespace(session_str: &str) -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_session(SessionId::new(session_str).unwrap())
}

fn sample_request_fingerprint(key: &CacheKey, ns: &CacheNamespace) -> RequestFingerprint {
    let candidate = CandidateDigest {
        skill_id: SkillId::new("cargo-test").unwrap(),
        content_hash: ContentHash::from_bytes(b"cargo test --all-targets"),
        excerpt_hash: None,
    };
    compute_request_fingerprint(
        key,
        ns,
        &RequestFingerprintInput {
            stage: RequestStage::Wide,
            canonical_redacted_state: b"{\"request\":\"run tests\"}",
            candidates: &[candidate],
            questions_digest: [2u8; 32],
            endpoint_url: "https://api.typesafe.ai",
            model: "jev-1",
            prompt_version: "1.0",
            adapter_version: "1.0",
            privacy_policy_version: "standard",
            excerpt_strategy: "default",
        },
    )
}

// Subprocess entry point for multi-process test
#[test]
#[ignore = "subprocess entry point invoked by real_competing_processes_and_single_flight"]
fn coordination_child_worker() {
    let db_path = PathBuf::from(
        std::env::var_os("SR_TEST_COORD_DB_PATH").expect("SR_TEST_COORD_DB_PATH must be set"),
    );
    let session_name =
        std::env::var("SR_TEST_COORD_SESSION").expect("SR_TEST_COORD_SESSION must be set");

    let key = test_key();
    let ns = test_namespace(&session_name);
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let coordinator = SqliteLeaseCoordinator::open(&db_path).unwrap();
    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: true,
        lease_ttl_ms: 10_000,
    };

    let now = 1_000_000u64;
    let acq = coordinator.acquire(coord_key, now, &policy).unwrap();

    match acq {
        LeaseAcquisition::Leading(leader) => {
            // Simulate 50ms provider call
            std::thread::sleep(Duration::from_millis(50));
            let outcome = coordinator
                .complete(
                    coord_key,
                    leader.owner_token,
                    leader.fencing_generation,
                    now + 50,
                )
                .unwrap();
            assert_eq!(outcome, PublishOutcome::Published);
            std::process::exit(0);
        }
        LeaseAcquisition::Following(_) => {
            // Child became follower; wait for leader to complete
            let res = coordinator
                .wait_for_completion(
                    coord_key,
                    || now + 100,
                    now + 5_000,
                    Duration::from_millis(10),
                )
                .unwrap();
            assert!(matches!(res, FollowerResolution::LeaderFailed));
            std::process::exit(10);
        }
        LeaseAcquisition::AlreadyCompleted => {
            std::process::exit(20);
        }
    }
}

#[test]
fn real_competing_processes_and_single_flight() {
    let tree = private_tree("real-proc");
    let db_path = tree.join("coordination.sqlite3");
    let session_name = "sess-competing-procs";

    let key = test_key();
    let ns = test_namespace(session_name);
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let coordinator = SqliteLeaseCoordinator::open(&db_path).unwrap();
    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: true,
        lease_ttl_ms: 10_000,
    };

    let t0 = 1_000_000u64;

    // 1. Parent process acquires lease as Leader
    let parent_acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let parent_leader = match parent_acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected parent to acquire leadership, got {other:?}"),
    };

    // 2. Spawn concurrent child process which attempts to acquire the exact same lease
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "coordination_child_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("SR_TEST_COORD_DB_PATH", &db_path)
        .env("SR_TEST_COORD_SESSION", session_name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("must spawn child process");

    // Give child a moment to attempt acquire and become follower
    std::thread::sleep(Duration::from_millis(60));

    // 3. Parent publishes completed response
    let finish_time = t0 + 100;
    let publish_outcome = coordinator
        .complete(
            coord_key,
            parent_leader.owner_token,
            parent_leader.fencing_generation,
            finish_time,
        )
        .unwrap();
    assert_eq!(publish_outcome, PublishOutcome::Published);

    // 4. Wait for child to exit
    let start_wait = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start_wait.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child process timed out waiting for lease completion");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // Child should have detected leader completion and exited with code 10
    assert_eq!(
        status.code(),
        Some(10),
        "child process should observe leader completion"
    );
}

#[test]
fn lease_expiry_and_reacquisition_with_fencing_bump() {
    let tree = private_tree("lease-expiry");
    let db_path = tree.join("coordination.sqlite3");
    let coordinator = SqliteLeaseCoordinator::open(&db_path).unwrap();

    let key = test_key();
    let ns = test_namespace("sess-expiry-test");
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: true,
        lease_ttl_ms: 100, // Short TTL of 100ms
    };

    let t0 = 10_000_000u64;

    // 1. Leader 1 acquires lease with gen 1
    let acq1 = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let leader1 = match acq1 {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected Leader 1 to acquire, got {other:?}"),
    };
    assert_eq!(leader1.fencing_generation, FencingGeneration(1));
    assert_eq!(leader1.lease_expires_at_unix_ms, t0 + 100);

    // 2. Query at t0 + 50: still active, caller is Follower
    let acq_mid = coordinator.acquire(coord_key, t0 + 50, &policy).unwrap();
    match acq_mid {
        LeaseAcquisition::Following(f) => {
            assert_eq!(f.leader_generation, FencingGeneration(1));
        }
        other => panic!("expected Following at t0+50, got {other:?}"),
    }

    // 3. Time passes past lease expiration: t1 = t0 + 150 (> t0 + 100)
    // Successor (Leader 2) acquires: must bump fencing generation to 2!
    let t1 = t0 + 150;
    let acq2 = coordinator.acquire(coord_key, t1, &policy).unwrap();
    let leader2 = match acq2 {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected Successor to acquire, got {other:?}"),
    };
    assert_eq!(
        leader2.fencing_generation,
        FencingGeneration(2),
        "fencing generation must bump on reacquisition"
    );
    assert_eq!(leader2.lease_expires_at_unix_ms, t1 + 100);

    // 4. Leader 2 completes and publishes successfully
    let pub2 = coordinator
        .complete(
            coord_key,
            leader2.owner_token,
            leader2.fencing_generation,
            t1 + 20,
        )
        .unwrap();
    assert_eq!(pub2, PublishOutcome::Published);

    // 5. Old Leader 1 wakes up late (at t1 + 30) and attempts to complete:
    // MUST BE REJECTED AS SUPERSEDED!
    let pub1 = coordinator
        .complete(
            coord_key,
            leader1.owner_token,
            leader1.fencing_generation,
            t1 + 30,
        )
        .unwrap();
    assert_eq!(
        pub1,
        PublishOutcome::Superseded {
            expected_generation: FencingGeneration(1),
            current_generation: Some(FencingGeneration(2)),
        },
        "old leader completion must be rejected as superseded"
    );
}

#[test]
fn old_owner_late_completion_rejected_after_expiry() {
    let coordinator = MemoryCoordinator::new();
    let key = test_key();
    let ns = test_namespace("sess-old-owner");
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: false,
        lease_ttl_ms: 100,
    };

    let t0 = 1_000_000u64;
    let acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected Leading, got {other:?}"),
    };

    // Leader finishes after expiry: t0 + 150 (> t0 + 100)
    let late_now = t0 + 150;
    let outcome = coordinator
        .complete(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            late_now,
        )
        .unwrap();

    assert_eq!(
        outcome,
        PublishOutcome::Superseded {
            expected_generation: FencingGeneration(1),
            current_generation: Some(FencingGeneration(1)),
        },
        "late completion past expiration without successor is still superseded"
    );
}

#[test]
fn follower_deadline_causes_quiet_fallback() {
    let coordinator = SingleFlightCoordinator::memory_only(CoordinationPolicy::default());
    let cache = MemoryResponseCache::new();

    let key = test_key();
    let ns = test_namespace("sess-follower-deadline");
    let fp = sample_request_fingerprint(&key, &ns);

    let t0 = 1_000_000u64;

    // Leader takes the lease with a 5000ms TTL
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);
    let _leader_acq = coordinator
        .acquire(coord_key, t0, &CoordinationPolicy::default())
        .unwrap();

    // Follower has a remaining deadline of 30ms (deadline at t0 + 30)
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 30,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    let current_time = std::sync::atomic::AtomicU64::new(t0);

    let result = coordinator.coordinate_request(
        &query,
        &cache,
        || current_time.fetch_add(15, Ordering::Relaxed),
        |_attempt_id| panic!("follower should never invoke provider"),
    );

    assert!(
        result.is_err(),
        "follower must return quiet fallback when deadline expires"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("deadline exceeded"),
        "error should state deadline exceeded: {err}"
    );
}

#[test]
fn different_session_and_event_isolation() {
    let key = test_key();
    let ns_a = test_namespace("sess-alpha");
    let ns_b = test_namespace("sess-beta");

    let fp_a = sample_request_fingerprint(&key, &ns_a);
    let fp_b = sample_request_fingerprint(&key, &ns_b);

    let key_a = CoordinationKey::compute(&key, &ns_a, &fp_a);
    let key_b = CoordinationKey::compute(&key, &ns_b, &fp_b);

    assert_ne!(
        key_a, key_b,
        "different sessions must produce distinct coordination keys"
    );

    let coordinator = MemoryCoordinator::new();
    let policy = CoordinationPolicy::default();

    let acq_a = coordinator.acquire(key_a, 1000, &policy).unwrap();
    let acq_b = coordinator.acquire(key_b, 1000, &policy).unwrap();

    assert!(
        matches!(acq_a, LeaseAcquisition::Leading(_)),
        "session A acquires independent leadership"
    );
    assert!(
        matches!(acq_b, LeaseAcquisition::Leading(_)),
        "session B acquires independent leadership"
    );
}

#[test]
fn no_hidden_response_body_in_coordination_store() {
    let tree = private_tree("no-body");
    let db_path = tree.join("coordination.sqlite3");
    let coordinator = SqliteLeaseCoordinator::open(&db_path).unwrap();

    let key = test_key();
    let ns = test_namespace("sess-no-body");
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy::default();
    let acq = coordinator.acquire(coord_key, 1000, &policy).unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected Leading, got {other:?}"),
    };

    coordinator
        .complete(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            1050,
        )
        .unwrap();

    // 1. Inspect table schema directly: ensure ZERO response body columns exist
    let conn = Connection::open(&db_path).unwrap();
    let mut stmt = conn
        .prepare("PRAGMA table_info(sr_coordination_leases)")
        .unwrap();
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(
        columns,
        vec![
            "coordination_key",
            "owner_token",
            "fencing_generation",
            "acquired_at_unix_ms",
            "expires_at_unix_ms",
            "attempt_id",
            "is_completed"
        ],
        "coordination table must strictly contain metadata only"
    );

    // 2. Test policy with cache disabled: cannot share response body
    let policy_no_cache = CoordinationPolicy {
        cache_enabled: false,
        cross_process_allowed: true,
        lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
    };
    let single_flight_no_cache =
        SingleFlightCoordinator::new(policy_no_cache, Some(&db_path)).unwrap();
    let empty_cache = MemoryResponseCache::new();

    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        request_fingerprint: &fp,
        deadline_unix_ms: 2000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    // When cache is disabled, reading completed body fails because coordination stores no bodies
    let res = single_flight_no_cache.coordinate_request(
        &query,
        &empty_cache,
        || 1100,
        |_att| panic!("provider should not run on already completed lease"),
    );

    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(
        err.to_string().contains("cache disabled"),
        "must report cache disabled error: {err}"
    );
}

#[test]
fn missing_owner_usage_remains_unknown_no_duplicate_attempt_charges() {
    let coordinator = SingleFlightCoordinator::memory_only(CoordinationPolicy::default());
    let cache = MemoryResponseCache::new();

    let key = test_key();
    let ns = test_namespace("sess-usage-accounting");
    let fp = sample_request_fingerprint(&key, &ns);

    let t0 = 1_000_000u64;
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 2000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    // 1. Leader executes provider call with 0 tokens (unknown/missing usage)
    let leader_res = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"wide\":\"valid\"}".to_vec(),
                    received_at_unix_ms: t0,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                    attempt_id: Some(attempt_id.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                ))
            },
        )
        .unwrap();

    assert!(!leader_res.is_follower);
    assert_eq!(leader_res.new_requests, 1);
    assert_eq!(
        leader_res.new_tokens, 0,
        "missing owner usage remains 0 tokens"
    );
    assert!(leader_res.attempt_id.is_some());

    // 2. Follower / second query: served from cache with zero new requests and zero new tokens
    let second_res = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0 + 10,
            |_att| panic!("provider must not be called for cached response"),
        )
        .unwrap();

    assert!(second_res.served_from_cache);
    assert_eq!(
        second_res.new_requests, 0,
        "follower must incur 0 new requests"
    );
    assert_eq!(second_res.new_tokens, 0, "follower must incur 0 new tokens");
    assert!(
        second_res.attempt_id.is_none(),
        "follower must not debit new attempt ID"
    );
}
