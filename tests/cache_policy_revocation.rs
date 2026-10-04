//! Cache policy regressions with production SQLite storage and pure eligibility.
//! MemoryResponseCache remains a useful pure cache fixture; the retired
//! SingleFlightCoordinator and its alternate response store are not reproduced.
//! Actual CLI sharing, offline output, provider attempts and live policy changes
//! are covered by real_rank_coordination and rank_acceptance.

use skillranker::cache::{
    CacheKey, CacheLookupQuery, CacheLookupResult, CacheNamespace, CachedResponseEntry,
    CandidateDigest, DEFAULT_CACHE_TTL_SECS, MemoryResponseCache, RequestFingerprint,
    RequestFingerprintInput, RequestStage, compute_request_fingerprint,
};
use skillranker::context::PrivateText;
use skillranker::effects::{EffectGate, Scope};
use skillranker::eligibility::{AbstainReason, Exclusion, LoadedState, Verdict, admit};
use skillranker::identity::{ContentHash, HarnessId, SessionId, SkillId, SourceId};
use skillranker::jev::codec::Usage;
use skillranker::privacy::EffectFlags;
use skillranker::roster::resolution::{AdvisorySkill, Binding};
use skillranker::roster::{
    DisplayName, InvocationName, InvocationRestrictions, LoadTarget, LocalPath, SkillRecord,
    UsageKind, Visibility,
};
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::{CACHE_FILE, CacheAccess, CacheLocation, CacheOpen, CacheStore};
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
            paired_wide: None,
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
    assert!(
        admission_a
            .removed
            .iter()
            .any(|(id, exc)| { id.as_str() == "cargo-test" && *exc == Exclusion::Excluded })
    );

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

fn qualified_store(path: &Path, gate: EffectGate) -> CacheOpen {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = gate
        .open_cache(
            &invocation,
            &cx,
            CacheAccess::Initialize,
            CacheLocation::Directory(path.to_owned()),
        )
        .unwrap();
    assert!(invocation.shutdown());
    result
}

fn ready(path: &Path, gate: EffectGate) -> CacheStore {
    let CacheOpen::Ready(store) = qualified_store(path, gate) else {
        panic!("cache unavailable");
    };
    *store
}

fn stored_entry(fp: RequestFingerprint) -> CachedResponseEntry {
    CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        // Storage fixture, not a validated provider answer or a ranking decision.
        response_bytes: b"synthetic-wide-storage-body".to_vec(),
        received_at_unix_ms: u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap(),
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: "jev-model-1".into(),
        model_revision: Some("r1".into()),
        original_usage: Usage {
            input_tokens: 80,
            output_tokens: 25,
        },
        attempt_id: Some("synthetic-prior-attempt".into()),
    }
}

fn seed(path: &Path, fp: RequestFingerprint) {
    let gate = EffectGate::new(EffectFlags::default(), Scope::Rank).unwrap();
    let store = ready(path, gate);
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let entry = stored_entry(fp);
    let now = entry.received_at_unix_ms;
    let store = store
        .record_response(&invocation, &cx, [7; 32], entry, now)
        .unwrap();
    assert!(invocation.shutdown());
    drop(store);
}

fn lookup(path: &Path, gate: EffectGate, fp: RequestFingerprint) -> Option<CachedResponseEntry> {
    let store = ready(path, gate);
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let (_, entry) = store
        .response(&invocation, &cx, [7; 32], RequestStage::Wide, fp)
        .unwrap();
    assert!(invocation.shutdown());
    entry
}

#[test]
fn qualified_store_consumers_withhold_stale_exclusion_authority() {
    let path = private_tree("qualified-exclusions");
    let fp = RequestFingerprint::from_bytes([45; 32]);
    seed(&path, fp);
    let gate = EffectGate::new(EffectFlags::default(), Scope::Rank).unwrap();
    let (record, binding, _, _) = sample_candidates();
    let candidate = AdvisorySkill {
        record: &record,
        binding: &binding,
    };
    let loaded = LoadedState {
        branch: None,
        records: &[],
    };
    let excluded = BTreeSet::from([&binding.id]);
    // Independently reopened consumers get the same response, while local
    // eligibility must be re-evaluated by each consumer. No model is called.
    let changed = lookup(&path, gate, fp).unwrap();
    let unchanged = lookup(&path, gate, fp).unwrap();
    assert_eq!(changed.response_bytes, unchanged.response_bytes);
    assert_eq!(changed.original_usage, unchanged.original_usage);
    assert_eq!(
        admit(&[candidate], &excluded, loaded).verdict,
        Some(Verdict::Abstain(AbstainReason::Excluded))
    );
    let eligible = admit(&[candidate], &BTreeSet::new(), loaded);
    assert_eq!(eligible.admitted.len(), 1);
    assert!(eligible.verdict.is_none());
}

#[test]
fn qualified_cache_network_revocation_preserves_exact_local_hit_and_exposes_miss() {
    let path = private_tree("qualified-offline");
    let fp = RequestFingerprint::from_bytes([46; 32]);
    seed(&path, fp);
    let revoked = EffectGate::new(
        EffectFlags {
            offline: true,
            no_ledger: true,
            ..EffectFlags::default()
        },
        Scope::Rank,
    )
    .unwrap();
    assert_eq!(
        revoked.policy().network_block(),
        Some(skillranker::privacy::NetworkBlock::Offline)
    );
    let fresh = lookup(&path, revoked, fp).unwrap();
    assert_eq!(fresh.response_bytes, stored_entry(fp).response_bytes);
    assert_eq!(fresh.original_usage, stored_entry(fp).original_usage);
    assert!(lookup(&path, revoked, RequestFingerprint::from_bytes([99; 32])).is_none());
    assert_eq!(
        revoked.policy().network_block(),
        Some(skillranker::privacy::NetworkBlock::Offline)
    );
    // This proves storage access under the effective offline gate. Complete
    // offline ranking and zero wire calls are exercised in rank_acceptance.
}

#[test]
fn disabled_cache_and_persistence_do_not_open_the_production_store() {
    for flags in [
        EffectFlags {
            no_cache: true,
            ..EffectFlags::default()
        },
        EffectFlags {
            no_persist: true,
            ..EffectFlags::default()
        },
        EffectFlags {
            dry_run: true,
            ..EffectFlags::default()
        },
    ] {
        let path = private_tree("disabled-production");
        let gate = EffectGate::new(flags, Scope::Rank).unwrap();
        assert!(matches!(qualified_store(&path, gate), CacheOpen::Disabled));
        assert!(std::fs::read_dir(&path).unwrap().next().is_none());
    }
    // Allowed twin must initialize a real, qualified cache at the same boundary.
    let path = private_tree("enabled-production");
    let gate = EffectGate::new(EffectFlags::default(), Scope::Rank).unwrap();
    assert!(matches!(qualified_store(&path, gate), CacheOpen::Ready(_)));
    assert!(path.join(CACHE_FILE).is_file());
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
