//! Acceptance tests for per-stage validated response cache and truthful freshness (sr-roadmap-l1i.5.8).
//!
//! Validates:
//! 1. Per-stage isolation: Wide and Rerank stages are cached and looked up independently.
//! 2. Shortlist sensitivity: Gate and shortlist M changes alter the Stage 2 fingerprint and cause a cache miss.
//! 3. Truthful freshness: TTL starts at provider receipt; age is strictly elapsed wall-clock time.
//! 4. Non-renewal on read: Reading from cache does not extend TTL.
//! 5. Wall-clock rollback rejection: Negative age (clock rollback) is detected and treated as stale.
//! 6. Revision mismatch & unversioned alias: Model/revision changes invalidate cache; unversioned alias cannot pair cached wide with fresh rerank.
//! 7. Zero-cost accounting: Wholly cached runs incur 0 new requests and 0 new tokens.
//! 8. Stale inspection non-actionable: Expired entries report stale=true and is_actionable=false, never used as live hook fallbacks.
//! 9. Namespace eviction: Evicting a namespace removes only its entries without affecting other sessions.

use skillranker::cache::{
    CacheError, CacheKey, CacheLookupQuery, CacheLookupResult, CacheNamespace, CachedResponseEntry,
    CandidateDigest, DEFAULT_CACHE_TTL_SECS, ExecutionAccounting, FreshnessStatus,
    MemoryResponseCache, PipelineCacheProvenance, RequestFingerprint, RequestFingerprintInput,
    RequestStage, StageProvenance, compute_request_fingerprint, validate_stage_pair_coherence,
};
use skillranker::identity::{ContentHash, HarnessId, SessionId, SkillId};
use skillranker::jev::codec::Usage;

fn test_key() -> CacheKey {
    CacheKey::from_bytes([99u8; 32])
}

fn test_namespace(session_str: &str) -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_session(SessionId::new(session_str).unwrap())
}

fn lookup_query<'a>(
    key: &'a CacheKey,
    ns: &'a CacheNamespace,
    stage: RequestStage,
    fp: &'a RequestFingerprint,
    now: u64,
    model: &'a str,
    rev: Option<&'a str>,
) -> CacheLookupQuery<'a> {
    CacheLookupQuery {
        key,
        namespace: ns,
        stage,
        fingerprint: fp,
        now_unix_ms: now,
        active_model: model,
        active_revision: rev,
    }
}

fn sample_candidates_m3() -> Vec<CandidateDigest> {
    vec![
        CandidateDigest {
            skill_id: SkillId::new("cargo-test").unwrap(),
            content_hash: ContentHash::from_bytes(b"content-cargo-test"),
            excerpt_hash: Some(ContentHash::from_bytes(b"excerpt-cargo-test")),
        },
        CandidateDigest {
            skill_id: SkillId::new("rust-lint").unwrap(),
            content_hash: ContentHash::from_bytes(b"content-rust-lint"),
            excerpt_hash: None,
        },
        CandidateDigest {
            skill_id: SkillId::new("bench-check").unwrap(),
            content_hash: ContentHash::from_bytes(b"content-bench-check"),
            excerpt_hash: None,
        },
    ]
}

fn make_request_fingerprint(
    key: &CacheKey,
    ns: &CacheNamespace,
    stage: RequestStage,
    candidates: &[CandidateDigest],
) -> skillranker::cache::RequestFingerprint {
    compute_request_fingerprint(
        key,
        ns,
        &RequestFingerprintInput {
            stage,
            canonical_redacted_state: b"{\"request\":\"optimize memory allocation\"}",
            candidates,
            questions_digest: [10u8; 32],
            endpoint_url: "https://api.typesafe.ai",
            model: "jev-model",
            prompt_version: "1.0",
            adapter_version: "1.0",
            privacy_policy_version: "standard",
            excerpt_strategy: "default",
            paired_wide: None,
        },
    )
}

#[test]
fn per_stage_isolation() {
    let key = test_key();
    let ns = test_namespace("sess-stage-iso");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();

    let wide_fp = make_request_fingerprint(&key, &ns, RequestStage::Wide, &candidates);
    let rerank_fp = make_request_fingerprint(&key, &ns, RequestStage::Rerank, &candidates);

    assert_ne!(
        wide_fp, rerank_fp,
        "wide and rerank fingerprints must be distinct"
    );

    let t0 = 1_000_000u64;
    let wide_entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: wide_fp,
        response_bytes: b"{\"answers\":{\"which\":{\"choice\":\"cargo-test\"}}}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 150,
            output_tokens: 50,
        },
        attempt_id: Some("att-w-1".to_string()),
    };

    cache
        .put(&key, &ns, wide_entry)
        .expect("put wide must succeed");

    // Wide lookup succeeds
    let wide_res = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &wide_fp,
            t0 + 5_000,
            "jev-model",
            Some("rev-1"),
        ))
        .expect("get wide must succeed");
    assert!(
        matches!(wide_res, CacheLookupResult::Hit { .. }),
        "wide must hit"
    );

    // Rerank lookup under wide fingerprint or rerank fingerprint is a Miss
    let rerank_res = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Rerank,
            &rerank_fp,
            t0 + 5_000,
            "jev-model",
            Some("rev-1"),
        ))
        .expect("get rerank must succeed");
    assert!(
        matches!(rerank_res, CacheLookupResult::Miss),
        "rerank must be a miss when only wide is cached"
    );

    let cross_stage_res = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Rerank,
            &wide_fp,
            t0 + 5_000,
            "jev-model",
            Some("rev-1"),
        ))
        .expect("cross-stage get must succeed");
    assert!(
        matches!(cross_stage_res, CacheLookupResult::Miss),
        "cannot retrieve wide entry under rerank stage"
    );
}

#[test]
fn shortlist_sensitivity_causes_rerank_miss_when_candidates_change() {
    let key = test_key();
    let ns = test_namespace("sess-shortlist-sens");
    let cache = MemoryResponseCache::new();

    let candidates_m3 = sample_candidates_m3();
    let candidates_m2 = &candidates_m3[..2]; // M changed from 3 to 2

    let rerank_fp_m3 = make_request_fingerprint(&key, &ns, RequestStage::Rerank, &candidates_m3);
    let rerank_fp_m2 = make_request_fingerprint(&key, &ns, RequestStage::Rerank, candidates_m2);

    assert_ne!(
        rerank_fp_m3, rerank_fp_m2,
        "changing shortlist M must change rerank fingerprint"
    );

    let t0 = 1_000_000u64;
    let rerank_entry_m3 = CachedResponseEntry {
        stage: RequestStage::Rerank,
        request_fingerprint: rerank_fp_m3,
        response_bytes: b"{\"rerank\":\"result-m3\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 300,
            output_tokens: 80,
        },
        attempt_id: Some("att-r-1".to_string()),
    };

    cache.put(&key, &ns, rerank_entry_m3).unwrap();

    // M=3 hits
    let hit_m3 = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Rerank,
            &rerank_fp_m3,
            t0 + 1_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    assert!(matches!(hit_m3, CacheLookupResult::Hit { .. }));

    // M=2 is a clean cache miss (cannot reuse alien candidate shortlist)
    let miss_m2 = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Rerank,
            &rerank_fp_m2,
            t0 + 1_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    assert!(matches!(miss_m2, CacheLookupResult::Miss));
}

#[test]
fn truthful_freshness_and_no_ttl_renewal_on_read() {
    let key = test_key();
    let ns = test_namespace("sess-freshness");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();
    let fp = make_request_fingerprint(&key, &ns, RequestStage::Wide, &candidates);

    let t0 = 10_000_000u64; // t0 in ms
    let ttl_secs = 600u32; // 10 minutes = 600,000 ms

    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"wide\":\"valid\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: ttl_secs,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
        },
        attempt_id: None,
    };
    cache.put(&key, &ns, entry).unwrap();

    // Read at t0 + 100s (100,000 ms)
    let read_100s = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            t0 + 100_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    match read_100s {
        CacheLookupResult::Hit {
            age_ms,
            remaining_ttl_ms,
            ..
        } => {
            assert_eq!(age_ms, 100_000);
            assert_eq!(remaining_ttl_ms, 500_000);
        }
        other => panic!("expected Hit at 100s, got {other:?}"),
    }

    // Read at t0 + 500s (500,000 ms) - verifies read at 100s did NOT renew TTL
    let read_500s = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            t0 + 500_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    match read_500s {
        CacheLookupResult::Hit {
            age_ms,
            remaining_ttl_ms,
            ..
        } => {
            assert_eq!(age_ms, 500_000);
            assert_eq!(remaining_ttl_ms, 100_000);
        }
        other => panic!("expected Hit at 500s, got {other:?}"),
    }

    // Read at t0 + 601s (601,000 ms) - expired!
    let read_601s = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            t0 + 601_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    match read_601s {
        CacheLookupResult::Stale { status, .. } => {
            assert_eq!(status, FreshnessStatus::Expired { age_ms: 601_000 });
        }
        other => panic!("expected Stale(Expired) at 601s, got {other:?}"),
    }
}

#[test]
fn wall_clock_rollback_detected_and_rejected() {
    let key = test_key();
    let ns = test_namespace("sess-clock-rollback");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();
    let fp = make_request_fingerprint(&key, &ns, RequestStage::Wide, &candidates);

    let t0 = 20_000_000u64;
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"wide\":\"valid\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: 600,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 50,
            output_tokens: 10,
        },
        attempt_id: None,
    };
    cache.put(&key, &ns, entry).unwrap();

    // Wall-clock rollback: current time is 19_999_000 ms (< t0 = 20_000_000 ms)
    let rollback_time = 19_999_000u64;
    let res = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            rollback_time,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    match res {
        CacheLookupResult::Stale { status, .. } => {
            assert_eq!(
                status,
                FreshnessStatus::ClockRollback {
                    received_at_unix_ms: t0,
                    current_unix_ms: rollback_time,
                }
            );
        }
        other => panic!("expected Stale(ClockRollback), got {other:?}"),
    }
}

#[test]
fn revision_mismatch_and_unversioned_alias_rules() {
    let key = test_key();
    let ns = test_namespace("sess-rev-mismatch");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();
    let fp = make_request_fingerprint(&key, &ns, RequestStage::Wide, &candidates);

    let t0 = 1_000_000u64;
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"wide\":\"rev1-output\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: 600,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 10,
            output_tokens: 10,
        },
        attempt_id: None,
    };
    cache.put(&key, &ns, entry).unwrap();

    // 1. Model revision changed to "rev-2" -> RevisionMismatch!
    let res_rev2 = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            t0 + 1_000,
            "jev-model",
            Some("rev-2"),
        ))
        .unwrap();
    assert!(matches!(
        res_rev2,
        CacheLookupResult::Stale {
            status: FreshnessStatus::RevisionMismatch,
            ..
        }
    ));

    // 2. Model name changed to "jev-v2" -> RevisionMismatch!
    let res_model2 = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            t0 + 1_000,
            "jev-v2",
            Some("rev-1"),
        ))
        .unwrap();
    assert!(matches!(
        res_model2,
        CacheLookupResult::Stale {
            status: FreshnessStatus::RevisionMismatch,
            ..
        }
    ));

    // 3. Stage pairing: Under unversioned model alias, mixing cached wide with fresh rerank is forbidden
    let pair_unversioned = validate_stage_pair_coherence(
        StageProvenance::Cached,
        StageProvenance::Fresh,
        true, // is_unversioned_alias
    );
    assert_eq!(
        pair_unversioned,
        Err(CacheError::UnversionedPairMismatch),
        "unversioned alias must reject mixing cached wide and fresh rerank"
    );

    // Under pinned revision, mixing is permitted as PartiallyCached
    let pair_versioned = validate_stage_pair_coherence(
        StageProvenance::Cached,
        StageProvenance::Fresh,
        false, // is_unversioned_alias = false (pinned)
    );
    assert_eq!(
        pair_versioned,
        Ok(PipelineCacheProvenance::PartiallyCached {
            wide: StageProvenance::Cached,
            rerank: StageProvenance::Fresh,
        })
    );

    // Both cached -> WhollyCached regardless of alias
    let both_cached =
        validate_stage_pair_coherence(StageProvenance::Cached, StageProvenance::Cached, true);
    assert_eq!(both_cached, Ok(PipelineCacheProvenance::WhollyCached));
}

#[test]
fn zero_cost_accounting_for_wholly_cached_executions() {
    let wide_usage = Usage {
        input_tokens: 1200,
        output_tokens: 80,
    };
    let rerank_usage = Usage {
        input_tokens: 800,
        output_tokens: 120,
    };

    // 1. Wholly cached pass -> 0 new requests, 0 new tokens!
    let cached_acct = ExecutionAccounting::compute(
        PipelineCacheProvenance::WhollyCached,
        wide_usage,
        Some(rerank_usage),
    );
    assert_eq!(cached_acct.new_requests, 0);
    assert_eq!(cached_acct.new_tokens, 0);
    assert!(cached_acct.served_from_cache);
    // Original usage is preserved for diagnostic transparency
    assert_eq!(cached_acct.original_usage.input_tokens, 2000);
    assert_eq!(cached_acct.original_usage.output_tokens, 200);
    assert_eq!(cached_acct.original_usage.total_tokens(), 2200);

    // 2. Wholly fresh pass -> 2 new requests, full token billing
    let fresh_acct = ExecutionAccounting::compute(
        PipelineCacheProvenance::WhollyFresh,
        wide_usage,
        Some(rerank_usage),
    );
    assert_eq!(fresh_acct.new_requests, 2);
    assert_eq!(fresh_acct.new_tokens, 2200);
    assert!(!fresh_acct.served_from_cache);

    // 3. Partially cached: Wide cached, Rerank fresh -> 1 new request, only rerank tokens
    let partial_acct = ExecutionAccounting::compute(
        PipelineCacheProvenance::PartiallyCached {
            wide: StageProvenance::Cached,
            rerank: StageProvenance::Fresh,
        },
        wide_usage,
        Some(rerank_usage),
    );
    assert_eq!(partial_acct.new_requests, 1);
    assert_eq!(partial_acct.new_tokens, rerank_usage.total_tokens());
    assert!(!partial_acct.served_from_cache);
}

#[test]
fn stale_inspection_non_actionable_and_no_hook_fallback() {
    let key = test_key();
    let ns = test_namespace("sess-stale-insp");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();
    let fp = make_request_fingerprint(&key, &ns, RequestStage::Wide, &candidates);

    let t0 = 1_000_000u64;
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"wide\":\"expired-data\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: 600,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
        attempt_id: None,
    };
    cache.put(&key, &ns, entry).unwrap();

    // Query at t0 + 700s (expired)
    let stale_lookup = cache
        .get(&lookup_query(
            &key,
            &ns,
            RequestStage::Wide,
            &fp,
            t0 + 700_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();

    // Live hook requires fresh_entry() -> None!
    assert!(
        stale_lookup.fresh_entry().is_none(),
        "live hook must never use stale entry as fallback"
    );

    // Inspection view
    let insp = stale_lookup.to_inspection_view();
    assert!(
        insp.is_stale,
        "inspection must clearly disclose stale: true"
    );
    assert!(
        !insp.is_actionable,
        "inspection must strictly mark is_actionable: false"
    );
    assert_eq!(insp.age_ms, Some(700_000));
    assert!(insp.entry.is_some());
}

#[test]
fn namespace_isolation_and_eviction() {
    let key = test_key();
    let ns_a = test_namespace("sess-alpha");
    let ns_b = test_namespace("sess-beta");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();

    let fp_a = make_request_fingerprint(&key, &ns_a, RequestStage::Wide, &candidates);
    let fp_b = make_request_fingerprint(&key, &ns_b, RequestStage::Wide, &candidates);

    let t0 = 1_000_000u64;
    let entry_a = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp_a,
        response_bytes: b"{\"session\":\"alpha\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: 600,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 10,
            output_tokens: 10,
        },
        attempt_id: None,
    };
    cache.put(&key, &ns_a, entry_a).unwrap();

    // Session B cannot see Session A's entry
    let res_b = cache
        .get(&lookup_query(
            &key,
            &ns_b,
            RequestStage::Wide,
            &fp_b,
            t0 + 1_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    assert!(
        matches!(res_b, CacheLookupResult::Miss),
        "session B must miss session A entry"
    );

    // Evict session A
    let evicted = cache.evict_namespace(&key, &ns_a).unwrap();
    assert_eq!(evicted, 1);

    // Session A is now a miss
    let res_a_after = cache
        .get(&lookup_query(
            &key,
            &ns_a,
            RequestStage::Wide,
            &fp_a,
            t0 + 1_000,
            "jev-model",
            Some("rev-1"),
        ))
        .unwrap();
    assert!(
        matches!(res_a_after, CacheLookupResult::Miss),
        "evicted namespace must be a miss"
    );
}

#[test]
fn a_stored_ttl_never_extends_freshness_past_ten_minutes() {
    let key = test_key();
    let ns = test_namespace("sess-ttl-cap");
    let cache = MemoryResponseCache::new();
    let candidates = sample_candidates_m3();
    let fp = make_request_fingerprint(&key, &ns, RequestStage::Wide, &candidates);
    let t0 = 30_000_000u64;
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"{\"wide\":\"valid\"}".to_vec(),
        received_at_unix_ms: t0,
        ttl_seconds: 3_600,
        model: "jev-model".to_string(),
        model_revision: Some("rev-1".to_string()),
        original_usage: Usage {
            input_tokens: 50,
            output_tokens: 10,
        },
        attempt_id: None,
    };
    cache.put(&key, &ns, entry).unwrap();
    let at = |ms: u64| {
        cache
            .get(&lookup_query(
                &key,
                &ns,
                RequestStage::Wide,
                &fp,
                ms,
                "jev-model",
                Some("rev-1"),
            ))
            .unwrap()
    };
    // Nine minutes after receipt: still fresh, with at most the capped remainder.
    match at(t0 + 9 * 60 * 1000) {
        CacheLookupResult::Hit {
            remaining_ttl_ms, ..
        } => assert_eq!(remaining_ttl_ms, 60 * 1000),
        other => panic!("expected a fresh hit, got {other:?}"),
    }
    // Eleven minutes: expired, although the stored TTL claimed an hour.
    assert!(matches!(
        at(t0 + 11 * 60 * 1000),
        CacheLookupResult::Stale {
            status: FreshnessStatus::Expired { .. },
            ..
        }
    ));
}
