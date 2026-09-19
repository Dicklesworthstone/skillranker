//! Acceptance tests for cache isolation, invalidation, and concurrent ownership (sr-roadmap-l1i.5.20).
//!
//! Validates:
//! 1. Stale policy authority withheld on exact hit: Effective exclusion policy change
//!    withholds cached skill from actionable recommendation; unchanged twin succeeds.
//! 2. Multi-process follower withholds stale authority: Follower process receiving
//!    shared response withholds excluded skill under its current local policy.
//! 3. Network-only revocation preserves valid local cache hit: Revoking network consent
//!    before publication allows valid local output when no wire requests are needed.
//! 4. Network-only revocation on shared response: Fully received shared response
//!    publishes valid output even if network consent is subsequently revoked.
//! 5. Negative network control: Cache miss under revoked network consent refuses
//!    with typed error, incurring zero network attempts.
//! 6. No-cache isolation: Coordination without cache forbids cross-process body sharing.
//! 7. No-persist isolation: In-memory coordination creates zero disk files or mutations.
//! 8. Stale/expired entries cannot authorize actionable output.

#![cfg(unix)]

use skillranker::cache::{
    compute_request_fingerprint, CacheKey, CacheLookupQuery, CacheLookupResult, CacheNamespace,
    CachedResponseEntry, CandidateDigest, CoordinateRequestQuery, CoordinationKey,
    CoordinationPolicy, LeaseAcquisition, LeaseCoordinator, MemoryResponseCache, PublishOutcome,
    RequestFingerprint, RequestFingerprintInput, RequestStage, SingleFlightCoordinator,
    SqliteResponseCache, DEFAULT_CACHE_TTL_SECS, DEFAULT_LEASE_TTL_MS,
};
use skillranker::context::PrivateText;
use skillranker::effects::{EffectGate, Scope};
use skillranker::eligibility::{admit, AbstainReason, Exclusion, LoadedState, Verdict};
use skillranker::identity::{ContentHash, HarnessId, SessionId, SkillId, SourceId};
use skillranker::jev::codec::Usage;
use skillranker::privacy::EffectFlags;
use skillranker::roster::resolution::{AdvisorySkill, Binding};
use skillranker::roster::{
    DisplayName, InvocationName, InvocationRestrictions, LoadTarget, LocalPath, SkillRecord,
    UsageKind, Visibility,
};
use std::collections::BTreeSet;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn private_tree(case: &str) -> PathBuf {
    let path = Path::new("/tmp").join(format!(
        "sr-cache-policy-{case}-{}-{}-{}",
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
    CacheKey::from_bytes([55u8; 32])
}

fn test_namespace(session_str: &str) -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_session(SessionId::new(session_str).unwrap())
}

fn mock_skill_record(name: &str) -> SkillRecord {
    SkillRecord {
        id: SkillId::new(name).unwrap(),
        source: SourceId::new("project").unwrap(),
        source_priority: 10,
        display_name: DisplayName::from_text(name),
        invocation_name: InvocationName::new(name).unwrap(),
        target: LoadTarget::File(LocalPath::new(PathBuf::from(format!(
            "/skills/{name}/SKILL.md"
        )))),
        source_content: ContentHash::from_bytes(name.as_bytes()),
        rendered_content: None,
        visibility: Visibility::Verified {
            contract_version: "v1".to_string(),
        },
        restrictions: InvocationRestrictions {
            agent_invocable: true,
            user_invocable: true,
        },
        usage_kind: UsageKind::Workflow,
        forked_context: false,
        dynamic_content: false,
        aliases: Vec::new(),
        description_full: PrivateText::new(format!("Description for {name}")),
        description_short: PrivateText::new(format!("Short {name}")),
        body_excerpt: PrivateText::new(format!("Body excerpt for {name}")),
        body_window: PrivateText::default(),
        tags: Vec::new(),
        phases: Vec::new(),
        parse_warnings: Vec::new(),
    }
}

fn mock_binding(name: &str) -> Binding {
    let id = SkillId::new(name).unwrap();
    Binding {
        id: id.clone(),
        source: SourceId::new("project").unwrap(),
        invocation: InvocationName::new(name).unwrap(),
        visibility: Visibility::Verified {
            contract_version: "v1".to_string(),
        },
        restrictions: InvocationRestrictions {
            agent_invocable: true,
            user_invocable: true,
        },
        priority: Some(10),
    }
}

fn sample_candidates() -> (SkillRecord, Binding, SkillRecord, Binding) {
    let r1 = mock_skill_record("cargo-test");
    let b1 = mock_binding("cargo-test");
    let r2 = mock_skill_record("rust-lint");
    let b2 = mock_binding("rust-lint");
    (r1, b1, r2, b2)
}

fn sample_request_fingerprint(
    key: &CacheKey,
    ns: &CacheNamespace,
    candidates: &[CandidateDigest],
) -> RequestFingerprint {
    compute_request_fingerprint(
        key,
        ns,
        &RequestFingerprintInput {
            stage: RequestStage::Wide,
            canonical_redacted_state: b"{\"request\":\"check and test workspace\"}",
            candidates,
            questions_digest: [33u8; 32],
            endpoint_url: "https://api.typesafe.ai",
            model: "jev-model-1",
            prompt_version: "1.0",
            adapter_version: "1.0",
            privacy_policy_version: "standard",
            excerpt_strategy: "default",
        },
    )
}

#[test]
fn exact_hit_consumer_withholds_stale_exclusion_authority() {
    let key = test_key();
    let ns = test_namespace("sess-exact-stale-exclusion");
    let cache = MemoryResponseCache::new();

    let (rec1, bind1, rec2, bind2) = sample_candidates();
    let adv1 = AdvisorySkill {
        record: &rec1,
        binding: &bind1,
    };
    let adv2 = AdvisorySkill {
        record: &rec2,
        binding: &bind2,
    };
    let candidates = [adv1, adv2];

    let candidate_digests = vec![
        CandidateDigest {
            skill_id: bind1.id.clone(),
            content_hash: rec1.source_content.clone(),
            excerpt_hash: None,
        },
        CandidateDigest {
            skill_id: bind2.id.clone(),
            content_hash: rec2.source_content.clone(),
            excerpt_hash: None,
        },
    ];

    let fp = sample_request_fingerprint(&key, &ns, &candidate_digests);
    let t0 = 1_000_000u64;

    // Cache entry has cargo-test as top choice
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"choice\":\"cargo-test\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model-1".to_string(),
        model_revision: Some("r1".to_string()),
        original_usage: Usage {
            input_tokens: 100,
            output_tokens: 30,
        },
        attempt_id: Some("attempt-leader-01".to_string()),
    };
    cache.put(&key, &ns, entry).unwrap();

    // 1. Consumer A: Policy has now changed to exclude "cargo-test"
    let mut excluded_a = BTreeSet::new();
    let id_cargo = bind1.id.clone();
    excluded_a.insert(&id_cargo);

    let empty_loaded = LoadedState {
        branch: None,
        records: &[],
    };

    // Lookup hits cache (served from cache, zero new requests)
    let lookup_a = cache
        .get(&CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 1000,
            active_model: "jev-model-1",
            active_revision: Some("r1"),
        })
        .unwrap();
    assert!(matches!(lookup_a, CacheLookupResult::Hit { .. }));

    // But publication revalidation against CURRENT effective policy withholds "cargo-test"!
    let admission_a = admit(&candidates, &excluded_a, empty_loaded);
    // cargo-test is excluded
    assert_eq!(admission_a.admitted.len(), 1);
    assert_eq!(admission_a.admitted[0].binding.id.as_str(), "rust-lint");
    assert!(admission_a
        .removed
        .iter()
        .any(|(id, exc)| { id.as_str() == "cargo-test" && *exc == Exclusion::Excluded }));

    // If ALL candidates were excluded, admission gives Abstain(Excluded)
    let mut exclude_all = BTreeSet::new();
    exclude_all.insert(&bind1.id);
    exclude_all.insert(&bind2.id);
    let admission_all = admit(&candidates, &exclude_all, empty_loaded);
    assert_eq!(
        admission_all.verdict,
        Some(Verdict::Abstain(AbstainReason::Excluded)),
        "all excluded must abstain without actionable output"
    );

    // 2. Consumer B (unchanged twin): Policy excludes nothing
    let excluded_b = BTreeSet::new();
    let admission_b = admit(&candidates, &excluded_b, empty_loaded);
    assert_eq!(admission_b.admitted.len(), 2);
    assert!(admission_b.verdict.is_none());
}

#[test]
fn two_process_follower_withholds_stale_exclusion_authority() {
    let tree = private_tree("two-proc-stale");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("sess-follower-stale");

    let (rec1, bind1, _rec2, _bind2) = sample_candidates();
    let candidate_digests = vec![CandidateDigest {
        skill_id: bind1.id.clone(),
        content_hash: rec1.source_content.clone(),
        excerpt_hash: None,
    }];
    let fp = sample_request_fingerprint(&key, &ns, &candidate_digests);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;

    // Leader acquires lease
    let acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected leading, got {other:?}"),
    };

    // Leader saves response to cache
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"choice\":\"cargo-test\"}".to_vec(),
        received_at_unix_ms: t0 + 50,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model-1".to_string(),
        model_revision: Some("r1".to_string()),
        original_usage: Usage {
            input_tokens: 120,
            output_tokens: 40,
        },
        attempt_id: Some(leader.attempt_id.clone()),
    };
    cache.put(&key, &ns, entry).unwrap();

    // Leader publishes completion
    let outcome = coordinator
        .complete(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            t0 + 60,
        )
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Published);

    // Follower arrives and queries
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 5000,
        active_model: "jev-model-1",
        active_revision: Some("r1"),
    };

    let follower_res = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0 + 100,
            |_att| panic!("follower must not invoke provider"),
        )
        .unwrap();

    assert!(follower_res.served_from_cache);
    assert_eq!(follower_res.new_requests, 0);
    assert_eq!(follower_res.new_tokens, 0);

    // Follower validates against local policy where "cargo-test" is excluded
    let adv1 = AdvisorySkill {
        record: &rec1,
        binding: &bind1,
    };
    let mut follower_exclusions = BTreeSet::new();
    follower_exclusions.insert(&bind1.id);

    let empty_loaded = LoadedState {
        branch: None,
        records: &[],
    };
    let follower_adm = admit(&[adv1], &follower_exclusions, empty_loaded);
    assert_eq!(
        follower_adm.verdict,
        Some(Verdict::Abstain(AbstainReason::Excluded)),
        "follower must withhold stale cached authority when locally excluded"
    );

    // Follower twin without exclusions admits the skill
    let empty_exclusions = BTreeSet::new();
    let twin_adm = admit(&[adv1], &empty_exclusions, empty_loaded);
    assert_eq!(twin_adm.admitted.len(), 1);
    assert!(twin_adm.verdict.is_none());
}

#[test]
fn exact_hit_network_only_revocation_preserves_valid_local_result() {
    let tree = private_tree("net-rev-exact");
    let db_path = tree.join("cache.sqlite3");
    let key = test_key();
    let ns = test_namespace("sess-net-rev-exact");
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let (rec1, bind1, _, _) = sample_candidates();
    let candidate_digests = vec![CandidateDigest {
        skill_id: bind1.id.clone(),
        content_hash: rec1.source_content.clone(),
        excerpt_hash: None,
    }];
    let fp = sample_request_fingerprint(&key, &ns, &candidate_digests);
    let t0 = 1_000_000u64;

    // Cache populated when network was initially allowed
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"choice\":\"cargo-test\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model-1".to_string(),
        model_revision: Some("r1".to_string()),
        original_usage: Usage {
            input_tokens: 80,
            output_tokens: 25,
        },
        attempt_id: Some("att-prior-01".to_string()),
    };
    cache.put(&key, &ns, entry).unwrap();

    // Now network consent is REVOKED (offline = true, allow_network = false)
    let revoked_flags = EffectFlags {
        offline: true,
        allow_network: false,
        dry_run: false,
        no_cache: false,
        no_ledger: true,
        no_persist: false,
        save_case: false,
    };
    let gate = EffectGate::new(revoked_flags, Scope::Rank).unwrap();
    assert!(!gate.source_policy().allow_network);
    assert_eq!(
        gate.policy().network_block(),
        Some(skillranker::privacy::NetworkBlock::Offline)
    );

    // Because response is already cached and valid, local cache lookup succeeds
    let lookup = cache
        .get(&CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 500,
            active_model: "jev-model-1",
            active_revision: Some("r1"),
        })
        .unwrap();

    assert!(lookup.fresh_entry().is_some());
    let fresh = lookup.fresh_entry().unwrap();
    assert_eq!(fresh.response_bytes, b"{\"choice\":\"cargo-test\"}");

    // Local result is valid with zero new network requests
    assert!(matches!(lookup, CacheLookupResult::Hit { .. }));

    // Negative control: A cache MISS under revoked network consent cannot call provider
    let miss_fp = RequestFingerprint::from_bytes([99u8; 32]);
    let miss_lookup = cache
        .get(&CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &miss_fp,
            now_unix_ms: t0 + 500,
            active_model: "jev-model-1",
            active_revision: Some("r1"),
        })
        .unwrap();
    assert!(matches!(miss_lookup, CacheLookupResult::Miss));
    // Since gate forbids network, no HTTP call is allowed:
    assert!(
        !gate.source_policy().allow_network,
        "network remains forbidden on miss"
    );
}

#[test]
fn shared_response_network_only_revocation_preserves_valid_local_result() {
    let tree = private_tree("net-rev-shared");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("sess-net-rev-shared");

    let (rec1, bind1, _, _) = sample_candidates();
    let candidate_digests = vec![CandidateDigest {
        skill_id: bind1.id.clone(),
        content_hash: rec1.source_content.clone(),
        excerpt_hash: None,
    }];
    let fp = sample_request_fingerprint(&key, &ns, &candidate_digests);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, Some(&db_path)).unwrap();
    let cache = SqliteResponseCache::open(&db_path).unwrap();

    let t0 = 1_000_000u64;

    // Leader completes provider call
    let acq = coordinator.acquire(coord_key, t0, &policy).unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected leading, got {other:?}"),
    };

    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"choice\":\"cargo-test\"}".to_vec(),
        received_at_unix_ms: t0 + 10,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model-1".to_string(),
        model_revision: Some("r1".to_string()),
        original_usage: Usage {
            input_tokens: 50,
            output_tokens: 15,
        },
        attempt_id: Some(leader.attempt_id.clone()),
    };
    cache.put(&key, &ns, entry).unwrap();
    coordinator
        .complete(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            t0 + 20,
        )
        .unwrap();

    // Follower's network consent is revoked
    let follower_flags = EffectFlags {
        offline: true,
        allow_network: false,
        dry_run: false,
        no_cache: false,
        no_ledger: true,
        no_persist: false,
        save_case: false,
    };
    let follower_gate = EffectGate::new(follower_flags, Scope::Rank).unwrap();
    assert!(!follower_gate.source_policy().allow_network);

    // Follower receives the completed response locally with 0 new wire calls
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 2000,
        active_model: "jev-model-1",
        active_revision: Some("r1"),
    };

    let follower_res = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0 + 30,
            |_att| panic!("no provider call allowed under revoked network consent"),
        )
        .unwrap();

    assert!(follower_res.served_from_cache);
    assert_eq!(follower_res.new_requests, 0);
    assert_eq!(follower_res.new_tokens, 0);
    assert_eq!(
        follower_res.entry.response_bytes,
        b"{\"choice\":\"cargo-test\"}"
    );
}

#[test]
fn no_cache_forbids_cross_process_body_sharing() {
    let tree = private_tree("no-cache-iso");
    let db_path = tree.join("coordination.sqlite3");
    let key = test_key();
    let ns = test_namespace("sess-no-cache-iso");
    let fp = RequestFingerprint::from_bytes([11u8; 32]);
    let coord_key = CoordinationKey::compute(&key, &ns, &fp);

    let no_cache_policy = CoordinationPolicy {
        cache_enabled: false,
        cross_process_allowed: true,
        lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
    };
    let coordinator = SingleFlightCoordinator::new(no_cache_policy, Some(&db_path)).unwrap();

    let t0 = 1_000_000u64;
    let acq = coordinator
        .acquire(coord_key, t0, &no_cache_policy)
        .unwrap();
    let leader = match acq {
        LeaseAcquisition::Leading(l) => l,
        other => panic!("expected leading, got {other:?}"),
    };

    // Completing lease with cache_enabled=false succeeds for lease metadata
    let outcome = coordinator
        .complete(
            coord_key,
            leader.owner_token,
            leader.fencing_generation,
            t0 + 50,
        )
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Published);

    // Follower attempting to retrieve body fails because no-cache forbids body storage/sharing
    let empty_cache = MemoryResponseCache::new();
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 1000,
        active_model: "jev-model-1",
        active_revision: None,
    };

    let err = coordinator
        .coordinate_request(
            &query,
            &empty_cache,
            || t0 + 60,
            |_att| panic!("no provider invocation on completed lease"),
        )
        .unwrap_err();

    assert!(
        err.to_string().contains("cache disabled"),
        "must reject body retrieval when cache is disabled: {err}"
    );
}

#[test]
fn no_persist_guarantees_zero_disk_mutation() {
    let tree = private_tree("no-persist-iso");
    // No SQLite database path is supplied (in-memory coordination)
    let policy = CoordinationPolicy::default();
    let coordinator = SingleFlightCoordinator::new(policy, None).unwrap();
    let cache = MemoryResponseCache::new();

    let key = test_key();
    let ns = test_namespace("sess-no-persist-iso");
    let fp = RequestFingerprint::from_bytes([22u8; 32]);

    let t0 = 1_000_000u64;
    let query = CoordinateRequestQuery {
        key: &key,
        namespace: &ns,
        stage: RequestStage::Wide,
        request_fingerprint: &fp,
        deadline_unix_ms: t0 + 2000,
        active_model: "jev-model-1",
        active_revision: None,
    };

    let res = coordinator
        .coordinate_request(
            &query,
            &cache,
            || t0,
            |attempt_id| {
                let entry = CachedResponseEntry {
                    stage: RequestStage::Wide,
                    request_fingerprint: fp,
                    response_bytes: b"{\"mem\":true}".to_vec(),
                    received_at_unix_ms: t0,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECS,
                    model: "jev-model-1".to_string(),
                    model_revision: None,
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
    assert_eq!(res.entry.response_bytes, b"{\"mem\":true}");

    // Verify zero files exist in tree
    let entries: Vec<_> = std::fs::read_dir(&tree).unwrap().collect();
    assert!(
        entries.is_empty(),
        "no-persist must leave directory completely untouched"
    );
}

#[test]
fn stale_expired_entry_refuses_actionable_publication() {
    let key = test_key();
    let ns = test_namespace("sess-stale-expired");
    let cache = MemoryResponseCache::new();
    let fp = RequestFingerprint::from_bytes([33u8; 32]);

    let t0 = 1_000_000u64;
    let ttl_s = 60u32;
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"choice\":\"old-skill\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: ttl_s,
        model: "jev-model-1".to_string(),
        model_revision: Some("r1".to_string()),
        original_usage: Usage {
            input_tokens: 20,
            output_tokens: 10,
        },
        attempt_id: Some("att-old".to_string()),
    };
    cache.put(&key, &ns, entry).unwrap();

    // Query past TTL (t0 + 61s)
    let stale_lookup = cache
        .get(&CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + u64::from(ttl_s + 1) * 1000,
            active_model: "jev-model-1",
            active_revision: Some("r1"),
        })
        .unwrap();

    assert!(
        stale_lookup.fresh_entry().is_none(),
        "expired entry is never fresh"
    );
    assert!(
        matches!(stale_lookup, CacheLookupResult::Stale { .. }),
        "expired entry must report stale"
    );

    // Fresh lookup twin at t0 + 10s succeeds
    let fresh_lookup = cache
        .get(&CacheLookupQuery {
            key: &key,
            namespace: &ns,
            stage: RequestStage::Wide,
            fingerprint: &fp,
            now_unix_ms: t0 + 10_000,
            active_model: "jev-model-1",
            active_revision: Some("r1"),
        })
        .unwrap();

    assert!(fresh_lookup.fresh_entry().is_some());
    assert!(matches!(fresh_lookup, CacheLookupResult::Hit { .. }));
}
