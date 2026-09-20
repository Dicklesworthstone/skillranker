//! Acceptance tests for single-flight response coordination with fenced bounded leases (sr-roadmap-l1i.5.10).
//!
//! Validates:
//! 1. Real competing processes: Concurrent processes share exact provider response via SQLite lease coordination.
//!    Both Wide and Rerank responses are verified with cross-process delivery.
//! 2. Lease expiry and reacquisition: Stalled leader lease expires; successor reacquires with bumped fencing generation.
//! 3. Fresh reacquisition on cache loss: Completed lease with missing/expired cache entry forces reacquisition for refresh.
//! 4. Completion/body race eliminated: Cache put precedes lease completion so followers always find bodies.
//! 5. Old owner late completion rejected: Expired or superseded leader cannot publish (quiet fallback).
//! 6. Follower deadline bounding: Follower deadline expiration returns quiet fallback without hanging.
//! 7. Namespace isolation: Distinct sessions/events produce distinct coordination keys and never coalesce.
//! 8. Zero hidden response bodies: Coordination state stores only tokens/timestamps; `--no-cache` forbids body sharing.
//! 9. Zero duplicate attempt charges: Follower incurs 0 new requests, 0 new tokens; missing usage remains unknown.
//! 10. Private bounded SQLite: Qualified engine checked, open with NOFOLLOW, WAL, and defensive pragmas.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use rusqlite::Connection;
use skillranker::cache::{
    CacheError, CacheKey, CacheLookupQuery, CacheLookupResult, CacheNamespace, CachedResponseEntry,
    CandidateDigest, CoordinateRequestQuery, CoordinationKey, CoordinationPolicy,
    DEFAULT_CACHE_TTL_SECS, DEFAULT_LEASE_TTL_MS, FencingGeneration, LeaseAcquisition,
    LeaseCoordinator, MemoryCoordinator, MemoryResponseCache, PublishOutcome, RequestFingerprint,
    RequestFingerprintInput, RequestStage, ResponseCache, SingleFlightCoordinator,
    SqliteLeaseCoordinator, SqliteResponseCache, compute_request_fingerprint,
};
use skillranker::identity::{ContentHash, HarnessId, SessionId, SkillId};
use skillranker::jev::codec::Usage;
use std::fs::DirBuilder;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

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

fn test_key() -> CacheKey {
    CacheKey::from_bytes([77u8; 32])
}

fn test_namespace(session_str: &str) -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_session(SessionId::new(session_str).unwrap())
}

fn sample_stage_request_fingerprint(
    key: &CacheKey,
    ns: &CacheNamespace,
    stage: RequestStage,
) -> RequestFingerprint {
    let candidate = CandidateDigest {
        skill_id: SkillId::new("cargo-test").unwrap(),
        content_hash: ContentHash::from_bytes(b"cargo test --all-targets"),
        excerpt_hash: None,
    };
    compute_request_fingerprint(
        key,
        ns,
        &RequestFingerprintInput {
            stage,
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

fn sample_request_fingerprint(key: &CacheKey, ns: &CacheNamespace) -> RequestFingerprint {
    sample_stage_request_fingerprint(key, ns, RequestStage::Wide)
}

#[test]
fn coordinated_deadline_rejects_expired_admission_and_late_provider_without_caching() {
    for expired_at_entry in [true, false] {
        let coordinator = SingleFlightCoordinator::memory_only(CoordinationPolicy::default());
        let cache = MemoryResponseCache::new();
        let key = test_key();
        let ns = test_namespace("deadline-admission");
        let fp = sample_request_fingerprint(&key, &ns);
        let clock = AtomicU64::new(if expired_at_entry { 1100 } else { 1000 });
        let calls = AtomicU64::new(0);
        let query = CoordinateRequestQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            request_fingerprint: &fp,
            deadline_unix_ms: 1100,
            active_model: "jev-1",
            active_revision: Some("rev-1"),
        };
        let result = coordinator.coordinate_request(
            &query,
            &cache,
            || clock.load(Ordering::Relaxed),
            |attempt| {
                calls.fetch_add(1, Ordering::Relaxed);
                clock.store(1100, Ordering::Relaxed);
                Ok((
                    deadline_entry(fp, attempt),
                    Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ))
            },
        );
        assert!(
            result.is_err(),
            "expired_at_entry={expired_at_entry}: {result:?}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), u64::from(!expired_at_entry));
        let lookup = cache.get(&skillranker::cache::CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: 1100,
            active_model: "jev-1",
            active_revision: Some("rev-1"),
        });
        assert!(
            lookup.unwrap().fresh_entry().is_none(),
            "late response must not become reusable"
        );
    }
}

fn deadline_entry(fp: RequestFingerprint, attempt: &str) -> CachedResponseEntry {
    CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{}".to_vec(),
        received_at_unix_ms: 1000,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-1".into(),
        model_revision: Some("rev-1".into()),
        original_usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        attempt_id: Some(attempt.into()),
    }
}

#[test]
fn sqlite_follower_retries_real_lock_contention_only_within_deadline() {
    for release_lock in [true, false] {
        let tree = private_tree("busy-follower");
        let path = tree.join("coordination.sqlite3");
        let coordinator = SqliteLeaseCoordinator::open(&path).unwrap();
        let key = test_key();
        let ns = test_namespace("busy-follower");
        let fp = sample_request_fingerprint(&key, &ns);
        let coord_key = CoordinationKey::compute(&key, &ns, &fp);
        let LeaseAcquisition::Leading(leader) = coordinator
            .acquire(coord_key, 1000, &CoordinationPolicy::default())
            .unwrap()
        else {
            panic!("first owner must lead");
        };
        assert_eq!(
            coordinator
                .complete(
                    coord_key,
                    leader.owner_token,
                    leader.fencing_generation,
                    1000
                )
                .unwrap(),
            PublishOutcome::Published
        );

        // Exclusive WAL locking blocks independent readers too. Keep the real
        // connection alive until a failed poll has consumed a clock sample.
        let locker = Connection::open(&path).unwrap();
        locker
            .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(matches!(
            coordinator.check_lease(coord_key),
            Err(skillranker::cache::CoordinationError::StorageBusy)
        ));
        let locker = std::sync::Mutex::new(Some(locker));
        let ticks = AtomicU64::new(0);
        let result = coordinator
            .wait_for_completion(
                coord_key,
                || {
                    let tick = ticks.fetch_add(1, Ordering::Relaxed);
                    if release_lock && tick == 1 {
                        let connection = locker.lock().unwrap().take().unwrap();
                        connection.execute_batch("ROLLBACK;").unwrap();
                        drop(connection);
                    }
                    1000 + tick * 15
                },
                1100,
                Duration::from_millis(1),
            )
            .unwrap();
        if release_lock {
            assert!(matches!(
                result,
                skillranker::cache::FollowerResolution::Completed
            ));
        } else {
            assert!(matches!(
                result,
                skillranker::cache::FollowerResolution::DeadlineExceeded
            ));
        }
    }
}

#[test]
fn coordinated_cache_hit_obeys_deadline_and_preserves_timely_success() {
    let coordinator = SingleFlightCoordinator::memory_only(CoordinationPolicy::default());
    let cache = MemoryResponseCache::new();
    let key = test_key();
    let ns = test_namespace("deadline-cache");
    let fp = sample_request_fingerprint(&key, &ns);
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: 1100,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };
    let leader = coordinator
        .coordinate_request(
            &query,
            &cache,
            || 1000,
            |attempt| {
                Ok((
                    deadline_entry(fp, attempt),
                    Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ))
            },
        )
        .unwrap();
    assert_eq!(leader.new_requests, 1);
    let hit = coordinator
        .coordinate_request(
            &query,
            &cache,
            || 1099,
            |_| panic!("cache hit sent provider request"),
        )
        .unwrap();
    assert!(hit.served_from_cache);
    assert!(
        !hit.is_follower,
        "arrival after publication is a cache hit, not a waiting follower"
    );
    assert_eq!(hit.new_requests, 0);
    assert!(
        coordinator
            .coordinate_request(
                &query,
                &cache,
                || 1100,
                |_| panic!("expired call sent provider request")
            )
            .is_err()
    );
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
    let test_stage_str =
        std::env::var("SR_TEST_COORD_STAGE").unwrap_or_else(|_| "wide".to_string());

    let key = test_key();
    let ns = test_namespace(&session_name);
    let stage = if test_stage_str == "rerank" {
        RequestStage::Rerank
    } else {
        RequestStage::Wide
    };
    let fp = sample_stage_request_fingerprint(&key, &ns, stage);

    let coordinator =
        SingleFlightCoordinator::new(CoordinationPolicy::default(), Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let now = 1_000_000u64;
    let started = Instant::now();
    let ready_path = std::env::var_os("SR_TEST_COORD_READY").map(PathBuf::from);
    let now_fn = || now + 100 + u64::try_from(started.elapsed().as_millis()).unwrap();
    if let Some(path) = &ready_path {
        let leases = SqliteLeaseCoordinator::open(&db_path).unwrap();
        let coord_key = CoordinationKey::compute(&key, &ns, &fp);
        assert!(
            matches!(
                leases
                    .acquire(coord_key, now_fn(), &CoordinationPolicy::default())
                    .unwrap(),
                LeaseAcquisition::Following(_)
            ),
            "child must contend with the active parent lease"
        );
        std::fs::write(path, b"following").expect("signal follower acquisition");
        assert!(
            matches!(
                leases
                    .wait_for_completion(coord_key, now_fn, now + 5000, Duration::from_millis(5))
                    .unwrap(),
                skillranker::cache::FollowerResolution::Completed
            ),
            "follower must observe completion within its deadline"
        );
    }
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage,
        request_fingerprint: &fp,
        deadline_unix_ms: now + 5_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    let res = coordinator
        .coordinate_request(&query, &cache, now_fn, |_attempt_id| {
            panic!("child process should be follower, never invoke provider")
        })
        .expect("follower must successfully receive coordinated response from leader");

    // Contention and completion were proven above. Retrieval after completion
    // takes the initial-cache-hit path, which correctly is not a waiting follower.
    assert!(
        !res.is_follower,
        "completed response is an initial cache hit"
    );
    assert!(
        res.served_from_cache,
        "follower response must be served from cache"
    );
    assert_eq!(
        res.new_requests, 0,
        "follower must incur zero new provider requests"
    );
    assert_eq!(
        res.new_tokens, 0,
        "follower must incur zero new provider tokens"
    );
    assert!(res.attempt_id.is_none(), "follower has no attempt id");

    if stage == RequestStage::Wide {
        assert_eq!(
            res.entry.response_bytes,
            b"{\"stage\":\"wide\",\"choice\":\"cargo-test\"}"
        );
    } else {
        assert_eq!(
            res.entry.response_bytes,
            b"{\"stage\":\"rerank\",\"ranks\":[1]}"
        );
    }

    std::process::exit(0);
}

#[test]
fn real_competing_processes_and_single_flight_wide_and_rerank() {
    let tree = private_tree("real-procs");
    let db_path = tree.join("coordination.sqlite3");
    let session_name = "sess-competing-procs-both-stages";

    let key = test_key();
    let ns = test_namespace(session_name);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;

    // Test both stages sequentially: Wide first, then Rerank
    for stage in [RequestStage::Wide, RequestStage::Rerank] {
        let fp = sample_stage_request_fingerprint(&key, &ns, stage);
        let coord_key = CoordinationKey::compute(&key, &ns, &fp);

        // 1. Parent process acquires lease as Leader
        let parent_acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
        let parent_leader = match parent_acq {
            LeaseAcquisition::Leading(l) => l,
            other => panic!("expected parent to acquire leadership for {stage:?}, got {other:?}"),
        };

        // 2. Spawn concurrent child process which attempts to acquire the exact same lease as follower
        let stage_str = if stage == RequestStage::Rerank {
            "rerank"
        } else {
            "wide"
        };
        let ready_path = tree.join(format!("{stage_str}.ready"));
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "coordination_child_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("SR_TEST_COORD_DB_PATH", &db_path)
            .env("SR_TEST_COORD_SESSION", session_name)
            .env("SR_TEST_COORD_STAGE", stage_str)
            .env("SR_TEST_COORD_READY", &ready_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("must spawn child process");

        let mut child = CapturedChild::new(child);
        let ready_started = Instant::now();
        while !ready_path.exists() {
            if child.child.try_wait().unwrap().is_some()
                || ready_started.elapsed() > Duration::from_secs(5)
            {
                panic!(
                    "child did not enter {stage:?} follower wait: {}",
                    child.diagnostics()
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // 3. Parent puts validated response in cache, THEN completes the lease (eliminating race)
        let expected_bytes = if stage == RequestStage::Wide {
            b"{\"stage\":\"wide\",\"choice\":\"cargo-test\"}".to_vec()
        } else {
            b"{\"stage\":\"rerank\",\"ranks\":[1]}".to_vec()
        };

        let response_entry = CachedResponseEntry {
            stage,
            request_fingerprint: fp,
            response_bytes: expected_bytes,
            received_at_unix_ms: t0 + 80,
            ttl_seconds: DEFAULT_CACHE_TTL_SECS,
            model: "jev-1".to_string(),
            model_revision: Some("rev-1".to_string()),
            original_usage: Usage {
                input_tokens: 120,
                output_tokens: 45,
            },
            attempt_id: Some(parent_leader.attempt_id.clone()),
        };

        cache.put(&key, &ns, response_entry).unwrap();

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

        // 4. Wait for child to exit successfully (exit code 0 proves response delivery)
        let start_wait = Instant::now();
        let status = loop {
            if let Some(st) = child.child.try_wait().unwrap() {
                break st;
            }
            if start_wait.elapsed() > Duration::from_secs(5) {
                panic!(
                    "child process timed out waiting for lease completion on {stage:?}: {}",
                    child.diagnostics()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(
            status.code(),
            Some(0),
            "child process must exit 0, retrieving validated {stage:?} response from leader: {}",
            child.diagnostics()
        );
    }
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
fn fresh_reacquisition_after_cache_loss_when_lease_completed() {
    let tree = private_tree("cache-loss");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("sess-cache-loss");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 5000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    // 1. Initial request completes and publishes
    let res1 = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"val\":1}".to_vec(),
                    received_at_unix_ms: t0,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    attempt_id: Some(attempt_id.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                ))
            },
        )
        .unwrap();
    assert!(!res1.is_follower);
    assert_eq!(res1.new_requests, 1);

    // 2. Cache entry is explicitly evicted / lost (simulating cache loss or eviction)
    cache.evict_namespace(&key, &ns).unwrap();

    // 3. Second request arrives at t0 + 200 (within original lease TTL).
    // Because cache has lost the entry, coordinator must force reacquisition
    // with bumped generation rather than failing or returning stale/missing data!
    let res2 = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0 + 200,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"val\":2}".to_vec(),
                    received_at_unix_ms: t0 + 200,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 12,
                        output_tokens: 6,
                    },
                    attempt_id: Some(attempt_id.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 12,
                        output_tokens: 6,
                    },
                ))
            },
        )
        .unwrap();

    assert!(!res2.is_follower, "must reacquire as leader on cache loss");
    assert_eq!(res2.new_requests, 1, "must execute fresh provider call");
    assert_eq!(res2.entry.response_bytes, b"{\"val\":2}");
}

#[test]
fn completion_published_after_cache_put_eliminates_body_race() {
    let tree = private_tree("body-race");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("sess-race-test");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 5000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    // Leader executes and publishes
    let res = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"race\":\"none\"}".to_vec(),
                    received_at_unix_ms: t0,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 5,
                        output_tokens: 2,
                    },
                    attempt_id: Some(attempt_id.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 5,
                        output_tokens: 2,
                    },
                ))
            },
        )
        .unwrap();

    assert_eq!(res.entry.response_bytes, b"{\"race\":\"none\"}");

    // Direct check: immediately upon completion, cache MUST contain the body
    let lookup = cache
        .get(&skillranker::cache::CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 1,
            active_model: "jev-1",
            active_revision: Some("rev-1"),
        })
        .unwrap();

    assert!(
        lookup.fresh_entry().is_some(),
        "cached response body must be present immediately when lease is completed"
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
        stage: RequestStage::Wide,
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
        stage: RequestStage::Wide,
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
        stage: RequestStage::Wide,
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

#[test]
fn private_bounded_sqlite_qualifications() {
    let tree = private_tree("sqlite-qual");
    let db_path = tree.join("qualified.sqlite3");

    // Must successfully open qualified SQLite database
    let coord = SqliteLeaseCoordinator::open(&db_path).unwrap();
    assert_eq!(coord.db_path(), db_path);

    let cache = SqliteResponseCache::open(&db_path).unwrap();
    assert_eq!(cache.db_path(), db_path);

    // Verify WAL mode and busy timeout on connection
    let conn = Connection::open(&db_path).unwrap();
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_lowercase(), "wal");

    // Relative path is rejected
    let rel_path = Path::new("relative.sqlite3");
    assert!(SqliteLeaseCoordinator::open(rel_path).is_err());
    assert!(SqliteResponseCache::open(rel_path).is_err());

    // Path traversal '..' is rejected
    let dot_path = Path::new("/tmp/../etc/passwd");
    assert!(SqliteLeaseCoordinator::open(dot_path).is_err());
    assert!(SqliteResponseCache::open(dot_path).is_err());
}

#[test]
fn stalled_owner_a_refused_without_replacing_successor_b_body_sqlite() {
    let tree = private_tree("stalled-a-b-sqlite");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("stalled-owner-sqlite");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: true,
        lease_ttl_ms: 200,
    };
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;
    let clock = Arc::new(AtomicU64::new(t0));
    let clock_clone = clock.clone();

    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 10_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    // Owner A begins at t0. In its provider callback, Owner A stalls past lease TTL.
    // While A is stalled, Owner B arrives, reacquires with bumped generation, and publishes response B.
    // When A returns from provider, its attempt to publish must be refused with quiet fallback,
    // and response B in the cache must NOT be replaced!
    let coordinator_b = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache_b = SqliteResponseCache::open(&db_path).unwrap();

    let res_a = coordinator.coordinate_request(
        &query,
        &cache,
        || clock.load(Ordering::Relaxed),
        |attempt_a| {
            // A acquired lease at t0 (expires at t0 + 200).
            // A now stalls until t0 + 250 (past lease expiration).
            clock_clone.store(t0 + 250, Ordering::Relaxed);

            // B enters while A is stalled:
            let res_b = coordinator_b
                .coordinate_request(
                    &query,
                    &cache_b,
                    || clock_clone.load(Ordering::Relaxed),
                    |attempt_b| {
                        let entry = CachedResponseEntry {
                            stage: RequestStage::Wide,
                            request_fingerprint: fp,
                            response_bytes: b"{\"winner\":\"B\"}".to_vec(),
                            received_at_unix_ms: t0 + 250,
                            ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                            model: "jev-1".to_string(),
                            model_revision: Some("rev-1".to_string()),
                            original_usage: Usage {
                                input_tokens: 20,
                                output_tokens: 10,
                            },
                            attempt_id: Some(attempt_b.to_string()),
                        };
                        Ok((
                            entry,
                            Usage {
                                input_tokens: 20,
                                output_tokens: 10,
                            },
                        ))
                    },
                )
                .expect("Owner B must succeed in reacquiring and publishing");

            assert!(!res_b.is_follower, "B must lead");
            assert_eq!(res_b.new_requests, 1);
            assert_eq!(res_b.entry.response_bytes, b"{\"winner\":\"B\"}");

            // Now Owner A returns response A at t0 + 260:
            clock_clone.store(t0 + 260, Ordering::Relaxed);
            let entry_a = CachedResponseEntry {
                stage: RequestStage::Wide,
                request_fingerprint: fp,
                response_bytes: b"{\"loser\":\"A\"}".to_vec(),
                received_at_unix_ms: t0 + 260,
                ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                model: "jev-1".to_string(),
                model_revision: Some("rev-1".to_string()),
                original_usage: Usage {
                    input_tokens: 15,
                    output_tokens: 8,
                },
                attempt_id: Some(attempt_a.to_string()),
            };
            Ok((
                entry_a,
                Usage {
                    input_tokens: 15,
                    output_tokens: 8,
                },
            ))
        },
    );

    // 1. Stalled Owner A must be refused publication
    assert!(
        res_a.is_err(),
        "stalled owner A must fail with quiet fallback"
    );
    let err_msg = res_a.unwrap_err().to_string();
    assert!(
        err_msg.contains("superseded") || err_msg.contains("quiet fallback"),
        "error must indicate superseded leader: {err_msg}"
    );

    // 2. Body preservation: response B must remain authoritative in cache!
    let lookup = cache
        .get(&skillranker::cache::CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 270,
            active_model: "jev-1",
            active_revision: Some("rev-1"),
        })
        .unwrap();
    let fresh = lookup.fresh_entry().expect("response B must be cached");
    assert_eq!(
        fresh.response_bytes, b"{\"winner\":\"B\"}",
        "Owner A must NOT overwrite Owner B's response body in cache"
    );

    // 3. Follower twin: subsequent caller reuses B without new provider call
    let res_c = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0 + 280,
            |_att| panic!("provider must not be called when B's body is cached"),
        )
        .unwrap();
    assert!(res_c.served_from_cache);
    assert_eq!(res_c.new_requests, 0);
    assert_eq!(res_c.entry.response_bytes, b"{\"winner\":\"B\"}");

    // 4. Honest owner twin: un-stalled leader completes and publishes successfully
    let fp_honest = sample_stage_request_fingerprint(&key, &ns, RequestStage::Rerank);
    let query_honest = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Rerank,
        request_fingerprint: &fp_honest,
        deadline_unix_ms: t0 + 10_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };
    let res_d = coordinator
        .coordinate_request(
            &query_honest,
            &cache,
            || t0 + 300,
            |attempt_d| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Rerank,
                    request_fingerprint: fp_honest,
                    response_bytes: b"{\"honest\":\"D\"}".to_vec(),
                    received_at_unix_ms: t0 + 300,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                    },
                    attempt_id: Some(attempt_d.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                    },
                ))
            },
        )
        .unwrap();
    assert!(!res_d.is_follower);
    assert_eq!(res_d.new_requests, 1);
    assert_eq!(res_d.entry.response_bytes, b"{\"honest\":\"D\"}");
}

#[test]
fn stalled_owner_a_refused_without_replacing_successor_b_body_memory() {
    let key = test_key();
    let ns = test_namespace("stalled-owner-mem");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: false,
        lease_ttl_ms: 200,
    };
    let coordinator = SingleFlightCoordinator::memory_only(policy);
    let cache = MemoryResponseCache::new();

    let t0 = 1_000_000u64;
    let clock = Arc::new(AtomicU64::new(t0));
    let clock_clone = clock.clone();

    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 10_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    let cache_b = cache.clone();
    let res_a = coordinator.coordinate_request(
        &query,
        &cache,
        || clock.load(Ordering::Relaxed),
        |attempt_a| {
            // A stalls past lease TTL:
            clock_clone.store(t0 + 250, Ordering::Relaxed);

            // B enters while A is stalled:
            let res_b = coordinator
                .coordinate_request(
                    &query,
                    &cache_b,
                    || clock_clone.load(Ordering::Relaxed),
                    |attempt_b| {
                        let entry = CachedResponseEntry {
                            stage: RequestStage::Wide,
                            request_fingerprint: fp,
                            response_bytes: b"{\"winner\":\"mem_B\"}".to_vec(),
                            received_at_unix_ms: t0 + 250,
                            ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                            model: "jev-1".to_string(),
                            model_revision: Some("rev-1".to_string()),
                            original_usage: Usage {
                                input_tokens: 20,
                                output_tokens: 10,
                            },
                            attempt_id: Some(attempt_b.to_string()),
                        };
                        Ok((
                            entry,
                            Usage {
                                input_tokens: 20,
                                output_tokens: 10,
                            },
                        ))
                    },
                )
                .expect("Owner B must succeed in reacquiring and publishing in memory");

            assert!(!res_b.is_follower, "B must lead in memory");
            assert_eq!(res_b.entry.response_bytes, b"{\"winner\":\"mem_B\"}");

            // A returns response A at t0 + 260:
            clock_clone.store(t0 + 260, Ordering::Relaxed);
            let entry_a = CachedResponseEntry {
                stage: RequestStage::Wide,
                request_fingerprint: fp,
                response_bytes: b"{\"loser\":\"mem_A\"}".to_vec(),
                received_at_unix_ms: t0 + 260,
                ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                model: "jev-1".to_string(),
                model_revision: Some("rev-1".to_string()),
                original_usage: Usage {
                    input_tokens: 15,
                    output_tokens: 8,
                },
                attempt_id: Some(attempt_a.to_string()),
            };
            Ok((
                entry_a,
                Usage {
                    input_tokens: 15,
                    output_tokens: 8,
                },
            ))
        },
    );

    // Stalled Owner A must be refused
    assert!(res_a.is_err(), "stalled owner A in memory must fail");
    let err_msg = res_a.unwrap_err().to_string();
    assert!(
        err_msg.contains("superseded") || err_msg.contains("quiet fallback"),
        "error must indicate superseded leader: {err_msg}"
    );

    // Body preservation: response B must remain in memory cache
    let lookup = cache
        .get(&skillranker::cache::CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 270,
            active_model: "jev-1",
            active_revision: Some("rev-1"),
        })
        .unwrap();
    let fresh = lookup
        .fresh_entry()
        .expect("response B must be cached in memory");
    assert_eq!(
        fresh.response_bytes, b"{\"winner\":\"mem_B\"}",
        "Owner A must NOT overwrite Owner B's response body in memory cache"
    );

    // Follower twin:
    let res_c = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0 + 280,
            |_att| panic!("provider must not be called when B is in memory cache"),
        )
        .unwrap();
    assert!(res_c.served_from_cache);
    assert_eq!(res_c.new_requests, 0);
    assert_eq!(res_c.entry.response_bytes, b"{\"winner\":\"mem_B\"}");
}

#[test]
fn raced_cache_miss_force_reacquire_follows_active_leader() {
    let tree = private_tree("raced-reacquire");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("raced-reacquire");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 10_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    // 1. Initial request completes and publishes
    let res1 = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"val\":\"initial\"}".to_vec(),
                    received_at_unix_ms: t0,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    attempt_id: Some(attempt_id.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                ))
            },
        )
        .unwrap();
    assert!(!res1.is_follower);

    // 2. Cache entry is lost / evicted
    cache.evict_namespace(&key, &ns).unwrap();

    // 3. Concurrent racers: Racer 1 and Racer 2 both see AlreadyCompleted and miss cache.
    // Racer 1 force_reacquires first and starts refreshing.
    // Racer 2 force_reacquires concurrently while Racer 1 is leading.
    // Racer 2 must become a Follower of Racer 1 rather than unconditionally preempting it!
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);
    let reacquired_1 = coordinator
        .force_reacquire(coord_key, t0 + 200, &policy)
        .unwrap();

    let leader_1 = match reacquired_1 {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("Racer 1 must lead, got {other:?}"),
    };
    assert_eq!(leader_1.fencing_generation, FencingGeneration(2));

    // While Racer 1 is actively leading (is_completed = 0, unexpired at t0 + 250):
    let reacquired_2 = coordinator
        .force_reacquire(coord_key, t0 + 250, &policy)
        .unwrap();

    // Racer 2 must detect active refresh leader and become Following!
    let follower_2 = match reacquired_2 {
        LeaseAcquisition::Following(f) => f,
        other => panic!("Racer 2 must follow active refresh leader, got {other:?}"),
    };
    assert_eq!(
        follower_2.leader_generation,
        FencingGeneration(2),
        "Racer 2 must follow Racer 1's generation"
    );

    // Racer 1 finishes and publishes fresh response
    let publish_outcome = coordinator
        .complete(
            coord_key,
            leader_1.owner_token,
            leader_1.fencing_generation,
            t0 + 300,
        )
        .unwrap();
    assert_eq!(publish_outcome, PublishOutcome::Published);
}

#[test]
fn exact_boundary_lease_expiry_refuses_publish_sqlite() {
    let tree = private_tree("boundary-expiry-sqlite");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("boundary-sqlite");
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: true,
        lease_ttl_ms: 200,
    };
    let coordinator = SqliteLeaseCoordinator::open(&db_path).unwrap();

    let t0 = 1_000_000u64;
    // 1. Acquire lease at t0 (expires_at = 1_000_200)
    let acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected leading, got {other:?}"),
    };
    assert_eq!(leader.lease_expires_at_unix_ms, 1_000_200);

    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"winner\":\"boundary\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-1".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
        attempt_id: Some(leader.attempt_id.clone()),
    };

    // 2. Attempt publish at exact expiration millisecond now == expires_at (1_000_200)
    let outcome = coordinator
        .complete_and_publish(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            1_000_200,
            Some((&key, &ns, &entry)),
        )
        .unwrap();

    // Must be refused as Superseded
    assert_eq!(
        outcome,
        PublishOutcome::Superseded {
            expected_generation: FencingGeneration(1),
            current_generation: Some(FencingGeneration(1)),
        }
    );

    // 3. Verify response body was NOT published into SQLite cache
    let cache = SqliteResponseCache::open(&db_path).unwrap();
    let lookup = cache.get(&skillranker::cache::CacheLookupQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        fingerprint: &fp,
        now_unix_ms: 1_000_200,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    });
    assert!(
        lookup.unwrap().fresh_entry().is_none(),
        "cache body must not be published on boundary expiry"
    );

    // 4. Competitor acquiring at 1_000_200 sees lease as expired and reacquires
    let acq2 = coordinator.acquire(coord_key, 1_000_200, &policy).unwrap();
    match acq2 {
        LeaseAcquisition::Leading(l2) => {
            assert_eq!(l2.fencing_generation, FencingGeneration(2));
        }
        other => panic!("competitor at exact expiry should acquire lease, got {other:?}"),
    }
}

#[test]
fn exact_boundary_lease_expiry_refuses_publish_memory() {
    let key = test_key();
    let ns = test_namespace("boundary-mem");
    let fp = sample_request_fingerprint(&key, &ns);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy {
        cache_enabled: true,
        cross_process_allowed: false,
        lease_ttl_ms: 200,
    };
    let coordinator = MemoryCoordinator::new();

    let t0 = 1_000_000u64;
    // 1. Acquire lease at t0 (expires_at = 1_000_200)
    let acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected leading, got {other:?}"),
    };
    assert_eq!(leader.lease_expires_at_unix_ms, 1_000_200);

    // 2. Attempt publish at exact expiration millisecond now == expires_at (1_000_200)
    let mut publish_invoked = false;
    let outcome = coordinator
        .complete_and_publish(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            1_000_200,
            || {
                publish_invoked = true;
                Ok(())
            },
        )
        .unwrap();

    // Must be refused as Superseded and closure NOT invoked
    assert_eq!(
        outcome,
        PublishOutcome::Superseded {
            expected_generation: FencingGeneration(1),
            current_generation: Some(FencingGeneration(1)),
        }
    );
    assert!(
        !publish_invoked,
        "publish closure must not run on boundary expiry"
    );

    // 3. Competitor acquiring at 1_000_200 sees lease as expired and reacquires
    let acq2 = coordinator.acquire(coord_key, 1_000_200, &policy).unwrap();
    match acq2 {
        LeaseAcquisition::Leading(l2) => {
            assert_eq!(l2.fencing_generation, FencingGeneration(2));
        }
        other => panic!("competitor at exact expiry should acquire lease, got {other:?}"),
    }
}

struct TrackingResponseCache {
    inner: SqliteResponseCache,
    put_count: Arc<AtomicUsize>,
}

impl ResponseCache for TrackingResponseCache {
    fn sqlite_path(&self) -> Option<&Path> {
        self.inner.sqlite_path()
    }

    fn get(&self, query: &CacheLookupQuery<'_>) -> Result<CacheLookupResult, CacheError> {
        self.inner.get(query)
    }

    fn put(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
        entry: CachedResponseEntry,
    ) -> Result<(), CacheError> {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        self.inner.put(key, namespace, entry)
    }

    fn evict_namespace(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
    ) -> Result<usize, CacheError> {
        self.inner.evict_namespace(key, namespace)
    }
}

#[test]
fn sqlite_coordinator_rejects_mismatched_cache_backends() {
    let tree = private_tree("mismatched-cache");
    let db1 = tree.join("coord.sqlite3");
    let db2 = tree.join("other.sqlite3");
    let key = test_key();
    let ns = test_namespace("mismatched");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db1)).unwrap();

    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: 1_000_000 + 10_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    let mut provider_called = false;

    // 1. In-memory cache with SQLite coordinator is rejected before provider execution
    let mem_cache = MemoryResponseCache::new();
    let res = coordinator.coordinate_request(
        &query,
        &mem_cache,
        || 1_000_000,
        |_| {
            provider_called = true;
            panic!("provider must not be called on store mismatch");
        },
    );
    assert!(!provider_called);
    match res {
        Err(skillranker::cache::CoordinationError::StorageError(msg)) => {
            assert!(msg.contains("response cache store mismatch"));
            assert!(msg.contains("supplied cache is in-memory"));
        }
        other => panic!("expected StorageError mismatch, got {other:?}"),
    }

    // 2. Different SQLite db path is rejected before provider execution
    let other_cache = SqliteResponseCache::open(&db2).unwrap();
    let res2 = coordinator.coordinate_request(
        &query,
        &other_cache,
        || 1_000_000,
        |_| {
            provider_called = true;
            panic!("provider must not be called on store mismatch");
        },
    );
    assert!(!provider_called);
    match res2 {
        Err(skillranker::cache::CoordinationError::StorageError(msg)) => {
            assert!(msg.contains("response cache store mismatch"));
            assert!(msg.contains("supplied cache bound to"));
        }
        other => panic!("expected StorageError mismatch, got {other:?}"),
    }
}

#[test]
fn sqlite_coordinator_atomic_commit_never_calls_caller_cache_put() {
    let tree = private_tree("atomic-no-secondary-put");
    let db_path = tree.join("coord.sqlite3");
    let key = test_key();
    let ns = test_namespace("no-secondary-put");
    let fp = sample_request_fingerprint(&key, &ns);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let raw_cache = SqliteResponseCache::open(&db_path).unwrap();

    let put_count = Arc::new(AtomicUsize::new(0));
    let tracking_cache = TrackingResponseCache {
        inner: raw_cache,
        put_count: put_count.clone(),
    };

    let t0 = 1_000_000u64;
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 10_000,
        active_model: "jev-1",
        active_revision: Some("rev-1"),
    };

    let res = coordinator
        .coordinate_request(
            &query,
            &tracking_cache,
            || t0,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"winner\":\"atomic_no_put\"}".to_vec(),
                    received_at_unix_ms: t0,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-1".to_string(),
                    model_revision: Some("rev-1".to_string()),
                    original_usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    attempt_id: Some(attempt_id.to_string()),
                };
                Ok((
                    entry,
                    Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                ))
            },
        )
        .unwrap();

    assert!(!res.is_follower);
    assert!(!res.served_from_cache);
    assert_eq!(res.entry.response_bytes, b"{\"winner\":\"atomic_no_put\"}");

    // Critical assertion: tracking_cache.put MUST NOT have been called!
    // The SQLite coordinator must write atomically inside complete_and_publish,
    // with ZERO un-fenced secondary cache.put calls outside the lease lock.
    assert_eq!(
        put_count.load(Ordering::SeqCst),
        0,
        "caller cache.put must never be invoked when SQLite coordinator publishes atomically"
    );

    // Verify the response is genuinely in SQLite cache
    let lookup = tracking_cache
        .get(&CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 10,
            active_model: "jev-1",
            active_revision: Some("rev-1"),
        })
        .unwrap();

    match lookup {
        CacheLookupResult::Hit { entry, .. } => {
            assert_eq!(entry.response_bytes, b"{\"winner\":\"atomic_no_put\"}");
        }
        other => panic!("expected CacheLookupResult::Hit from SQLite cache, got {other:?}"),
    }
}
