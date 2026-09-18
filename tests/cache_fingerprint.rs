//! Verification tests for protected cache namespace, request, and decision fingerprints.
//!
//! Validates:
//! 1. Key protection: CacheKey never leaks raw key bytes in debug/display representation.
//! 2. Determinism: Exact canonical inputs produce identical fingerprints.
//! 3. Stage & candidate sensitivity: Changing stage or candidate details changes the fingerprint.
//! 4. Question & model sensitivity: Changing questions, endpoint, model, or prompt/adapter versions changes the fingerprint.
//! 5. Namespace isolation: Different sessions or key generations produce distinct fingerprints (no cross-session leak).
//! 6. Decision sensitivity: Loaded references, exclusions, policy weights, prior snapshots, and snoozes alter the decision fingerprint.
//! 7. Non-finite weight rejection: NaN and Inf return FingerprintError::NonFiniteWeight.
//! 8. Event delivery key: Deterministic deduplication, distinguishing ambiguous vs unambiguous deliveries.

use skillranker::cache::{
    CacheKey, CacheNamespace, CandidateDigest, DecisionFingerprintInput, EventDeliveryInput,
    FingerprintError, LoadedReferenceDigest, RankingPolicySnapshot, RequestFingerprint,
    RequestFingerprintInput, RequestStage, SnoozeDigest, compute_decision_fingerprint,
    compute_delivery_key, compute_request_fingerprint,
};
use skillranker::identity::{
    AdapterId, AdapterVersion, BranchId, ContentHash, ContextEpoch, HarnessId, SessionId, SkillId,
    WorkspaceId,
};

fn test_key() -> CacheKey {
    CacheKey::from_bytes([42u8; 32])
}

fn sample_namespace() -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_workspace(WorkspaceId::new("/workspaces/demo").unwrap())
        .with_session(SessionId::new("sess-123").unwrap())
        .with_branch(BranchId::new("feature/ranking").unwrap())
        .with_context_epoch(ContextEpoch::new("epoch-1").unwrap())
        .with_adapter(
            AdapterId::new("claude_code").unwrap(),
            AdapterVersion::new("0.8.0").unwrap(),
        )
}

fn sample_candidates() -> Vec<CandidateDigest> {
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
    ]
}

fn sample_policy() -> RankingPolicySnapshot {
    RankingPolicySnapshot {
        w_fit: 1.0,
        w_prior: 0.0,
        w_phase: 0.0,
        gate_threshold: 0.30,
        fit_threshold: 0.30,
        top_k: 5,
        max_shortlist_m: 8,
    }
}

#[test]
fn key_protection_and_generation() {
    let generated = CacheKey::generate().expect("key generation from OS CSPRNG must succeed");
    let repr = format!("{generated:?}");
    assert!(
        repr.contains("CacheKey(<secret-key>)"),
        "debug output must not leak raw key: {repr}"
    );
    assert!(
        !repr.contains("42"),
        "key bytes must not appear in debug representation"
    );

    let manual = CacheKey::from_bytes([0x7a; 32]);
    let manual_repr = format!("{manual:?}");
    assert_eq!(manual_repr, "CacheKey(<secret-key>)");
}

#[test]
fn request_fingerprint_determinism_and_canonical_equivalence() {
    let key = test_key();
    let ns = sample_namespace();
    let candidates = sample_candidates();

    let input1 = RequestFingerprintInput {
        stage: RequestStage::Wide,
        canonical_redacted_state: b"{\"request\":\"fix bug in parser\"}",
        candidates: &candidates,
        questions_digest: [1u8; 32],
        endpoint_url: "https://api.typesafe.ai",
        model: "sys1-preview",
        prompt_version: "v1.2",
        adapter_version: "0.8.0",
        privacy_policy_version: "standard",
        excerpt_strategy: "head-tail",
    };

    let fp1 = compute_request_fingerprint(&key, &ns, &input1);
    let fp2 = compute_request_fingerprint(&key, &ns, &input1);

    assert_eq!(
        fp1, fp2,
        "identical inputs must produce identical request fingerprints"
    );
    assert_eq!(fp1.to_hex(), fp2.to_hex());
    assert_eq!(fp1.as_bytes(), fp2.as_bytes());
}

#[test]
fn request_fingerprint_stage_and_field_sensitivity() {
    let key = test_key();
    let ns = sample_namespace();
    let candidates = sample_candidates();

    let base = RequestFingerprintInput {
        stage: RequestStage::Wide,
        canonical_redacted_state: b"{\"request\":\"fix bug in parser\"}",
        candidates: &candidates,
        questions_digest: [1u8; 32],
        endpoint_url: "https://api.typesafe.ai",
        model: "sys1-preview",
        prompt_version: "v1.2",
        adapter_version: "0.8.0",
        privacy_policy_version: "standard",
        excerpt_strategy: "head-tail",
    };

    let base_fp = compute_request_fingerprint(&key, &ns, &base);

    // 1. Changing stage (Wide vs Rerank)
    let stage_diff = RequestFingerprintInput {
        stage: RequestStage::Rerank,
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &stage_diff),
        "stage change must change request fingerprint"
    );

    // 2. Changing redacted request state
    let state_diff = RequestFingerprintInput {
        canonical_redacted_state: b"{\"request\":\"different request\"}",
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &state_diff),
        "redacted state change must change request fingerprint"
    );

    // 3. Changing candidate items or ordering
    let mut reordered_candidates = candidates.clone();
    reordered_candidates.reverse();
    let cand_diff = RequestFingerprintInput {
        candidates: &reordered_candidates,
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &cand_diff),
        "candidate ordering must change request fingerprint"
    );

    // 4. Changing candidate content hash
    let mut modified_cand = candidates.clone();
    modified_cand[0].content_hash = ContentHash::from_bytes(b"modified-content");
    let cand_content_diff = RequestFingerprintInput {
        candidates: &modified_cand,
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &cand_content_diff),
        "candidate content hash must change request fingerprint"
    );

    // 5. Changing questions digest
    let q_diff = RequestFingerprintInput {
        questions_digest: [2u8; 32],
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &q_diff),
        "questions digest must change request fingerprint"
    );

    // 6. Changing endpoint URL
    let ep_diff = RequestFingerprintInput {
        endpoint_url: "https://custom.endpoint.internal",
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &ep_diff),
        "endpoint change must change request fingerprint"
    );

    // 7. Changing model
    let model_diff = RequestFingerprintInput {
        model: "sys1-large",
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &model_diff),
        "model change must change request fingerprint"
    );

    // 8. Changing prompt or adapter version
    let prompt_diff = RequestFingerprintInput {
        prompt_version: "v1.3",
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &prompt_diff),
        "prompt version change must change request fingerprint"
    );

    // 9. Changing privacy policy profile
    let priv_diff = RequestFingerprintInput {
        privacy_policy_version: "minimal",
        ..base.clone()
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &priv_diff),
        "privacy profile change must change request fingerprint"
    );

    // 10. Changing excerpt strategy
    let strat_diff = RequestFingerprintInput {
        excerpt_strategy: "full-paragraph",
        ..base
    };
    assert_ne!(
        base_fp,
        compute_request_fingerprint(&key, &ns, &strat_diff),
        "excerpt strategy change must change request fingerprint"
    );
}

#[test]
fn namespace_isolation_across_sessions_and_key_generations() {
    let key = test_key();
    let ns1 = sample_namespace();
    let candidates = sample_candidates();

    let input = RequestFingerprintInput {
        stage: RequestStage::Wide,
        canonical_redacted_state: b"{\"request\":\"identical text\"}",
        candidates: &candidates,
        questions_digest: [1u8; 32],
        endpoint_url: "https://api.typesafe.ai",
        model: "sys1-preview",
        prompt_version: "v1.2",
        adapter_version: "0.8.0",
        privacy_policy_version: "standard",
        excerpt_strategy: "head-tail",
    };

    let fp1 = compute_request_fingerprint(&key, &ns1, &input);

    // Different session ID: same text MUST NOT share cache!
    let ns_sess2 = sample_namespace().with_session(SessionId::new("sess-456").unwrap());
    let fp_sess2 = compute_request_fingerprint(&key, &ns_sess2, &input);
    assert_ne!(
        fp1, fp_sess2,
        "different sessions must never share request fingerprint"
    );

    // Key generation advance: invalidates namespace
    let mut ns_gen2 = sample_namespace();
    ns_gen2.key_generation = 2;
    let fp_gen2 = compute_request_fingerprint(&key, &ns_gen2, &input);
    assert_ne!(
        fp1, fp_gen2,
        "key generation rotation must invalidate cache namespace"
    );

    // Different context epoch
    let ns_epoch2 = sample_namespace().with_context_epoch(ContextEpoch::new("epoch-2").unwrap());
    assert_ne!(
        fp1,
        compute_request_fingerprint(&key, &ns_epoch2, &input),
        "context epoch change must change request fingerprint"
    );

    // Different branch
    let ns_branch2 = sample_namespace().with_branch(BranchId::new("feature/branch-b").unwrap());
    assert_ne!(
        fp1,
        compute_request_fingerprint(&key, &ns_branch2, &input),
        "branch change must change request fingerprint"
    );
}

#[test]
fn decision_fingerprint_sensitivity() {
    let key = test_key();
    let ns = sample_namespace();
    let candidates = sample_candidates();

    let req_input = RequestFingerprintInput {
        stage: RequestStage::Wide,
        canonical_redacted_state: b"{\"request\":\"deploy app\"}",
        candidates: &candidates,
        questions_digest: [1u8; 32],
        endpoint_url: "https://api.typesafe.ai",
        model: "sys1-preview",
        prompt_version: "v1.2",
        adapter_version: "0.8.0",
        privacy_policy_version: "standard",
        excerpt_strategy: "head-tail",
    };
    let req_fp = compute_request_fingerprint(&key, &ns, &req_input);

    let loaded = vec![LoadedReferenceDigest {
        skill_id: SkillId::new("cargo-test").unwrap(),
        content_hash: ContentHash::from_bytes(b"content-cargo-test"),
    }];
    let exclusions = vec![SkillId::new("dangerous-rm").unwrap()];
    let policy = sample_policy();
    let prior_snap = Some(ContentHash::from_bytes(b"prior-snapshot-content"));
    let snoozes = vec![SnoozeDigest {
        skill_id: SkillId::new("annoying-tip").unwrap(),
        until_unix_ms: 5_000_000,
    }];

    let dec_input_base = DecisionFingerprintInput {
        request_fingerprint: req_fp,
        loaded_references: &loaded,
        explicit_exclusions: &exclusions,
        ranking_policy: policy,
        prior_snapshot_id: prior_snap.clone(),
        effective_snoozes: &snoozes,
        visibility_metadata: "claude-code-personal-and-project",
    };

    let dec_fp_base = compute_decision_fingerprint(&key, &ns, &dec_input_base)
        .expect("valid decision fingerprint");

    // 1. Changing loaded references
    let dec_input_no_loaded = DecisionFingerprintInput {
        loaded_references: &[],
        ..dec_input_base.clone()
    };
    assert_ne!(
        dec_fp_base,
        compute_decision_fingerprint(&key, &ns, &dec_input_no_loaded).unwrap(),
        "loaded references change must change decision fingerprint"
    );

    // 2. Changing explicit exclusions
    let dec_input_no_excl = DecisionFingerprintInput {
        explicit_exclusions: &[],
        ..dec_input_base.clone()
    };
    assert_ne!(
        dec_fp_base,
        compute_decision_fingerprint(&key, &ns, &dec_input_no_excl).unwrap(),
        "exclusion change must change decision fingerprint"
    );

    // 3. Changing policy weights
    let mut modified_policy = policy;
    modified_policy.w_fit = 2.0;
    let dec_input_policy = DecisionFingerprintInput {
        ranking_policy: modified_policy,
        ..dec_input_base.clone()
    };
    assert_ne!(
        dec_fp_base,
        compute_decision_fingerprint(&key, &ns, &dec_input_policy).unwrap(),
        "policy weight change must change decision fingerprint"
    );

    // 4. Changing prior snapshot
    let dec_input_no_prior = DecisionFingerprintInput {
        prior_snapshot_id: None,
        ..dec_input_base.clone()
    };
    assert_ne!(
        dec_fp_base,
        compute_decision_fingerprint(&key, &ns, &dec_input_no_prior).unwrap(),
        "prior snapshot change must change decision fingerprint"
    );

    // 5. Changing snoozes
    let dec_input_no_snooze = DecisionFingerprintInput {
        effective_snoozes: &[],
        ..dec_input_base.clone()
    };
    assert_ne!(
        dec_fp_base,
        compute_decision_fingerprint(&key, &ns, &dec_input_no_snooze).unwrap(),
        "snooze change must change decision fingerprint"
    );

    // 6. Changing visibility metadata
    let dec_input_vis = DecisionFingerprintInput {
        visibility_metadata: "project-only",
        ..dec_input_base
    };
    assert_ne!(
        dec_fp_base,
        compute_decision_fingerprint(&key, &ns, &dec_input_vis).unwrap(),
        "visibility metadata change must change decision fingerprint"
    );
}

#[test]
fn non_finite_policy_weights_rejected() {
    let key = test_key();
    let ns = sample_namespace();
    let req_fp = RequestFingerprint::from_bytes([99u8; 32]);

    let mut nan_policy = sample_policy();
    nan_policy.w_fit = f64::NAN;

    let input_nan = DecisionFingerprintInput {
        request_fingerprint: req_fp,
        loaded_references: &[],
        explicit_exclusions: &[],
        ranking_policy: nan_policy,
        prior_snapshot_id: None,
        effective_snoozes: &[],
        visibility_metadata: "",
    };

    assert_eq!(
        compute_decision_fingerprint(&key, &ns, &input_nan),
        Err(FingerprintError::NonFiniteWeight),
        "NaN weight must be rejected"
    );

    let mut inf_policy = sample_policy();
    inf_policy.gate_threshold = f64::INFINITY;
    let input_inf = DecisionFingerprintInput {
        ranking_policy: inf_policy,
        ..input_nan
    };

    assert_eq!(
        compute_decision_fingerprint(&key, &ns, &input_inf),
        Err(FingerprintError::NonFiniteWeight),
        "Infinity threshold must be rejected"
    );
}

#[test]
fn event_delivery_duplicate_detection() {
    let key = test_key();
    let sess = SessionId::new("sess-hook-1").unwrap();
    let branch = BranchId::new("main").unwrap();
    let prompt_hash = ContentHash::from_bytes(b"prompt: build and test");

    let delivery1 = EventDeliveryInput {
        delivery_id: Some("del-xyz-789"),
        session_id: &sess,
        branch_id: Some(&branch),
        event_type: "UserPromptSubmit",
        transcript_generation: 1,
        cursor_offset: 250,
        prompt_fingerprint: &prompt_hash,
    };

    let key1 = compute_delivery_key(&key, &delivery1);
    let key1_dup = compute_delivery_key(&key, &delivery1);
    assert_eq!(key1, key1_dup);
    assert!(
        !key1.is_ambiguous(),
        "explicit delivery ID is never ambiguous"
    );

    // Different cursor offset -> distinct delivery
    let delivery2 = EventDeliveryInput {
        cursor_offset: 500,
        ..delivery1.clone()
    };
    let key2 = compute_delivery_key(&key, &delivery2);
    assert_ne!(key1, key2);

    // Unversioned delivery without delivery ID and at offset 0 -> ambiguous!
    let delivery_ambiguous = EventDeliveryInput {
        delivery_id: None,
        session_id: &sess,
        branch_id: Some(&branch),
        event_type: "UserPromptSubmit",
        transcript_generation: 0,
        cursor_offset: 0,
        prompt_fingerprint: &prompt_hash,
    };
    let key_amb = compute_delivery_key(&key, &delivery_ambiguous);
    assert!(
        key_amb.is_ambiguous(),
        "unversioned delivery at generation 0 with no delivery ID must be marked ambiguous"
    );

    // Non-zero generation without delivery ID is NOT ambiguous because generation distinguishes it
    let delivery_non_amb = EventDeliveryInput {
        delivery_id: None,
        session_id: &sess,
        branch_id: Some(&branch),
        event_type: "UserPromptSubmit",
        transcript_generation: 1,
        cursor_offset: 0,
        prompt_fingerprint: &prompt_hash,
    };
    let key_non_amb = compute_delivery_key(&key, &delivery_non_amb);
    assert!(
        !key_non_amb.is_ambiguous(),
        "delivery with positive transcript generation has established coordinate identity"
    );
}
