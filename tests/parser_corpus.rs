//! tests/parser_corpus.rs
//!
//! Bounded parser, property, and fuzz regression campaigns for SkillRanker (P4).
//! Exercises all untrusted parser boundaries and pure invariants with bounded,
//! adversarial, and generated inputs:
//! - Configuration authority and layer precedence.
//! - Normalized context and native JSONL tail handling.
//! - Roster frontmatter, aliases, and size limits.
//! - Quill query escaping and term compilation bounds.
//! - Provider wire JSON, duplicate keys, option maps, and distribution sums.
//! - Explicit directive parsing and conflict resolution.
//! - Finite scoring, numerical stability, and softmax normalization.
//! - Keyed BLAKE3 cache fingerprints and namespace isolation.
//! - Bounded output document serialization and terminal text sanitization.
//! - Fixed-seed deterministic fuzz smoke campaign runner (1,500 iterations).

use asupersync::Budget;
use asupersync::runtime::RuntimeBuilder;
use serde_json::json;
use skillranker::cache::{
    CacheKey, CacheNamespace, CandidateDigest, DecisionFingerprintInput, RankingPolicySnapshot,
    RequestFingerprintInput, RequestStage, compute_decision_fingerprint,
    compute_request_fingerprint,
};
use skillranker::config::{ConfigSources, MAX_STRING_VALUE_BYTES, RawValue, ResolvedConfig};
use skillranker::context::jsonl::{CursorKind, SkipKind, parse_line, snapshot_jsonl};
use skillranker::identity::{
    AdapterId, AdapterVersion, BranchId, ContentHash, ContextEpoch, HarnessId, SessionId, SkillId,
    WorkspaceId,
};
use skillranker::jev::codec::{CodecError, MAX_REQUEST_BYTES, Request};
use skillranker::limits::*;
use skillranker::output::table::sanitize_terminal_text;
use skillranker::output::{
    ErrorKind, MAX_OUTPUT_BYTES, MAX_TEXT_BYTES, OutputDocument, sanitize_diagnostic_text,
};
use skillranker::roster::explicit::{
    ExplicitResolutionRequest, ExplicitResolutionResult, UnresolvedReason, parse_prompt_directives,
    resolve_explicit_requirements,
};
use skillranker::roster::resolution::ResolvedRoster;
use skillranker::roster::retrieval::{QueryInput, compile_query};
use skillranker::roster::{FrontmatterError, parse_skill_metadata};
use skillranker::runtime::ProcessInvocation;
use skillranker::scoring::{
    EPSILON, Input, ScoringError, W_FIT_MAX, W_PHASE_MAX, W_PRIOR_MAX, Weights, clip, log_odds,
    rank,
};

// ==============================================================================
// 1. Configuration and Project Authority Parser Fuzz & Boundaries
// ==============================================================================

#[test]
fn test_configuration_and_authority_parser_fuzz_boundaries() {
    // A. Valid layered configuration round trips with correct precedence
    let sources = ConfigSources {
        trusted_user: vec![
            ("ranking.top".to_string(), RawValue::Integer(5)),
            ("ranking.shortlist".to_string(), RawValue::Integer(8)),
        ],
        project: vec![("ranking.top".to_string(), RawValue::Integer(3))],
        ..ConfigSources::default()
    };
    let resolved = ResolvedConfig::resolve(sources, 1).expect("clean resolution");
    assert_eq!(
        resolved.effective().top(),
        3,
        "project layer overrides user layer"
    );
    assert_eq!(
        resolved.effective().shortlist(),
        8,
        "user layer value preserved"
    );

    // B. Project layer attempting to set forbidden authority settings must be rejected
    for forbidden_path in [
        "network.enabled",
        "typesafe.endpoint",
        "state.directory",
        "cache.directory",
        "retention.raw_transcripts",
    ] {
        let evil_sources = ConfigSources {
            project: vec![(forbidden_path.to_string(), RawValue::Bool(true))],
            ..ConfigSources::default()
        };
        let err = ResolvedConfig::resolve(evil_sources, 1).unwrap_err();
        assert!(
            !err.issues().is_empty(),
            "project layer must fail when setting {forbidden_path}"
        );
    }

    // C. Boundaries on numerical settings: invalid negative or zero top
    let invalid_sources = ConfigSources {
        trusted_user: vec![("ranking.top".to_string(), RawValue::Integer(0))],
        ..ConfigSources::default()
    };
    assert!(ResolvedConfig::resolve(invalid_sources, 1).is_err());

    // D. Excessive string length in configuration setting
    let huge_model = "m".repeat(MAX_STRING_VALUE_BYTES + 100);
    let huge_model_sources = ConfigSources {
        trusted_user: vec![("ranking.model".to_string(), RawValue::String(huge_model))],
        ..ConfigSources::default()
    };
    assert!(ResolvedConfig::resolve(huge_model_sources, 1).is_err());
}

// ==============================================================================
// 2. Normalized Context and JSONL Tail Handling Boundaries
// ==============================================================================

#[test]
fn test_normalized_context_and_jsonl_tail_fuzz_boundaries() {
    let temp_dir = std::env::temp_dir().join(format!("sr-fuzz-jsonl-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("test_session.jsonl");

    // A. Complete records followed by an incomplete tail line
    let rec1 = json!({
        "type": "user",
        "uuid": "evt-1",
        "sessionId": "session-1",
        "message": {"content": [{"type": "text", "text": "First user turn"}]},
        "timestamp": "2026-09-19T10:00:00Z"
    });
    let rec2 = json!({
        "type": "assistant",
        "uuid": "evt-2",
        "sessionId": "session-1",
        "message": {"content": [{"type": "text", "text": "Assistant reply"}]},
        "timestamp": "2026-09-19T10:00:05Z"
    });
    let incomplete_tail = "{\"type\": \"user\", \"uuid\": \"evt-3\", \"ses";

    let content = format!("{}\n{}\n{}", rec1, rec2, incomplete_tail);
    std::fs::write(&file_path, content.as_bytes()).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snapshot = snapshot_jsonl(&invocation, &cx, &file_path, None, CursorKind::Ranking)
        .expect("snapshot succeeds");

    // The snapshot must process complete records and cleanly defer the incomplete tail without panic
    assert_eq!(snapshot.events.len(), 2);
    assert!(snapshot.incomplete_tail, "trailing bytes must be deferred");
    assert_eq!(snapshot.skipped.len(), 0);

    // B. Middle corrupted record is recorded as SkippedRecord, and reading continues
    let bad_record = "{\"type\": \"invalid-unclosed-json";
    let rec3 = json!({
        "type": "user",
        "uuid": "evt-4",
        "sessionId": "session-1",
        "message": {"content": [{"type": "text", "text": "Turn after corruption"}]},
        "timestamp": "2026-09-19T10:00:10Z"
    });
    let content_with_bad = format!("{}\n{}\n{}\n", rec1, bad_record, rec3);
    std::fs::write(&file_path, content_with_bad.as_bytes()).unwrap();

    let snapshot2 = snapshot_jsonl(&invocation, &cx, &file_path, None, CursorKind::Ranking)
        .expect("snapshot succeeds");
    assert_eq!(snapshot2.events.len(), 2);
    assert_eq!(snapshot2.skipped.len(), 1);
    assert!(matches!(snapshot2.skipped[0].kind, SkipKind::Corrupt));

    // C. Single record exceeding ONE_TRANSCRIPT_RECORD_BYTES is skipped
    let huge_line = format!(
        "{{\"type\": \"user\", \"text\": \"{}\"}}\n",
        "x".repeat(ONE_TRANSCRIPT_RECORD_BYTES.max() + 10)
    );
    let content_with_huge = format!("{}\n{}", rec1, huge_line);
    std::fs::write(&file_path, content_with_huge.as_bytes()).unwrap();

    let snapshot3 = snapshot_jsonl(&invocation, &cx, &file_path, None, CursorKind::Ranking)
        .expect("snapshot succeeds");
    assert_eq!(snapshot3.events.len(), 1);
    assert!(
        snapshot3
            .skipped
            .iter()
            .any(|s| matches!(s.kind, SkipKind::Oversize))
    );

    let _ = invocation.shutdown();
    let _ = std::fs::remove_dir_all(&temp_dir);
}

// ==============================================================================
// 3. Roster Frontmatter & Alias Adversarial Corpus
// ==============================================================================

#[test]
fn test_roster_frontmatter_and_alias_adversarial_corpus() {
    // A. Frontmatter size limit: 16 KiB
    let huge_frontmatter = format!(
        "---\nname: skill_huge\ndescription: {}\n---\nBody",
        "a".repeat(16 * KIB + 10)
    );
    let err = parse_skill_metadata(huge_frontmatter.as_bytes()).unwrap_err();
    assert!(matches!(err, FrontmatterError::FrontmatterTooLarge(_)));

    // B. Synonymous aliases collision produces DuplicateKey
    let alias_duplicate =
        "---\nname: skill_alias\nuser-invocable: true\nuser_invocable: true\n---\nBody";
    let err = parse_skill_metadata(alias_duplicate.as_bytes()).unwrap_err();
    assert_eq!(err, FrontmatterError::DuplicateKey);

    let alias_duplicate2 = "---\nname: skill_alias2\ndisable-model-invocation: true\ndisable_model_invocation: false\n---\nBody";
    let err = parse_skill_metadata(alias_duplicate2.as_bytes()).unwrap_err();
    assert_eq!(err, FrontmatterError::DuplicateKey);

    // C. Missing frontmatter falls back to heading / paragraph
    let markdown_no_fm =
        "# My Markdown Skill\n\nThis is a fallback description paragraph for the skill.\n";
    let parsed = parse_skill_metadata(markdown_no_fm.as_bytes()).expect("fallback succeeds");
    assert_eq!(parsed.name.as_deref(), Some("My Markdown Skill"));
    assert_eq!(
        parsed.description,
        "This is a fallback description paragraph for the skill."
    );
    assert!(parsed.user_invocable);
    assert!(parsed.agent_invocable);

    // D. Malformed YAML without closing delimiter returns clean error
    let unclosed_fm = "---\nname: broken\ndescription: missing closing delimiter\n";
    assert_eq!(
        parse_skill_metadata(unclosed_fm.as_bytes()).unwrap_err(),
        FrontmatterError::UnclosedFrontmatter
    );

    // E. Empty frontmatter
    let empty_fm = "---\n---\n# Title\nDescription\n";
    let parsed_empty =
        parse_skill_metadata(empty_fm.as_bytes()).expect("empty frontmatter falls back");
    assert_eq!(parsed_empty.name.as_deref(), Some("Title"));
}

// ==============================================================================
// 4. Quill Query Escaping and Term Bounds
// ==============================================================================

#[test]
fn test_quill_query_escaping_and_term_bounds() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::new());

    // A. Special Lucene / Quill characters must be properly escaped in literal tokens
    let special_input = "test + - && || ! ( ) { } [ ] ^ \" ~ * ? : \\ / token";
    let input = QueryInput {
        latest_request: special_input,
        active_task: "",
        recent_errors: "",
    };

    let compiled = compile_query(&cx, input).expect("compilation succeeds");
    let query_lit = compiled.query.expect("query terms exist");
    assert!(query_lit.as_str().contains("test"));
    assert!(query_lit.as_str().contains("token"));

    // B. Completely empty or whitespace-only input returns None for query (retrieval-empty)
    let empty_input = QueryInput {
        latest_request: "   \t\n   ",
        active_task: "",
        recent_errors: "",
    };
    let compiled_empty = compile_query(&cx, empty_input).expect("compilation succeeds");
    assert!(
        compiled_empty.query.is_none(),
        "whitespace-only input must be retrieval-empty"
    );

    // C. Massive repeated terms are deduplicated within disjunction limit
    let repeated = "duplicate ".repeat(500);
    let repeated_input = QueryInput {
        latest_request: &repeated,
        active_task: "",
        recent_errors: "",
    };
    let compiled_rep = compile_query(&cx, repeated_input).expect("compilation succeeds");
    assert!(
        compiled_rep
            .query
            .expect("query terms exist")
            .as_str()
            .len()
            <= 2048,
        "query must stay strictly bounded"
    );
}

// ==============================================================================
// 5. Provider JSON Codec and Option Map Boundaries
// ==============================================================================

#[test]
fn test_provider_json_codec_and_option_map_boundaries() {
    // A. Request size cap: MAX_REQUEST_BYTES (96 KiB)
    let huge_query = "q".repeat(MAX_REQUEST_BYTES + 10);
    let huge_req_json = json!({
        "model": "jev-latest",
        "state": {"task": "test"},
        "questions": {
            "rank": {
                "type": "choice",
                "instructions": huge_query,
                "criteria": {"__none__": "none"}
            }
        }
    });
    let serialized = serde_json::to_vec(&huge_req_json).unwrap();
    assert!(serialized.len() > MAX_REQUEST_BYTES);
    let decode_err = match Request::from_json(&serialized) {
        Ok(_) => panic!("oversized request should fail decode"),
        Err(e) => e,
    };
    assert_eq!(decode_err, CodecError::TooLarge);

    // B. Option count boundary: 255 max choice options (254 real + 1 none)
    let mut criteria = serde_json::Map::new();
    for i in 0..254 {
        criteria.insert(format!("skill_{i}"), json!(format!("Desc {i}")));
    }
    criteria.insert("__none__".to_string(), json!("No skill applies"));
    assert_eq!(criteria.len(), 255);

    let valid_choice_req = json!({
        "model": "jev-latest",
        "state": {"task": "test"},
        "questions": {
            "rank": {
                "type": "choice",
                "instructions": "Which skill?",
                "criteria": criteria
            }
        }
    });
    let req_bytes = serde_json::to_vec(&valid_choice_req).unwrap();
    let req = Request::from_json(&req_bytes).expect("255 options is valid");
    assert_eq!(req.questions().len(), 1);

    // C. Duplicate keys in provider response must be rejected
    let duplicate_key_resp = br#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":10,"output_tokens":5},"model":"evil-second-model"}"#;
    let resp_err = match req.decode_response(duplicate_key_resp) {
        Ok(_) => panic!("duplicate key should fail decode"),
        Err(e) => e,
    };
    assert_eq!(resp_err, CodecError::InvalidJson);

    // D. Non-finite probabilities rejected
    let nan_resp = br#"{"model":"jev-latest","answers":{"rank":{"type":"choice","choice":"__none__","probabilities":{"__none__":NaN},"confidence":1.0}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
    let resp_err2 = match req.decode_response(nan_resp) {
        Ok(_) => panic!("NaN probability should fail decode"),
        Err(e) => e,
    };
    assert_eq!(resp_err2, CodecError::InvalidJson);

    // E. Option map mismatch: response omits requested options
    let missing_options_resp = json!({
        "model": "jev-latest",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "__none__",
                "probabilities": {
                    "__none__": 1.0
                },
                "confidence": 1.0
            }
        },
        "usage": {
            "input_tokens": 10,
            "output_tokens": 10
        }
    });
    let missing_bytes = serde_json::to_vec(&missing_options_resp).unwrap();
    let resp_err_opt = match req.decode_response(&missing_bytes) {
        Ok(_) => panic!("missing options should fail decode"),
        Err(e) => e,
    };
    assert_eq!(resp_err_opt, CodecError::OptionMismatch);

    // F. Probability sum out of tolerance (sum = 0.8 != 1.0)
    let single_opt_req_json = json!({
        "model": "jev-latest",
        "state": {"task": "test"},
        "questions": {
            "rank": {
                "type": "choice",
                "instructions": "Which skill?",
                "criteria": {"__none__": "none"}
            }
        }
    });
    let single_opt_req =
        Request::from_json(&serde_json::to_vec(&single_opt_req_json).unwrap()).unwrap();
    let bad_sum_resp = json!({
        "model": "jev-latest",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "__none__",
                "probabilities": {
                    "__none__": 0.8
                },
                "confidence": 0.8
            }
        },
        "usage": {
            "input_tokens": 10,
            "output_tokens": 10
        }
    });
    let bad_sum_bytes = serde_json::to_vec(&bad_sum_resp).unwrap();
    let resp_err3 = match single_opt_req.decode_response(&bad_sum_bytes) {
        Ok(_) => panic!("bad sum probability should fail decode"),
        Err(e) => e,
    };
    assert_eq!(resp_err3, CodecError::InvalidDistribution);
}

// ==============================================================================
// 6. Explicit Directive Resolution Adversarial Inputs
// ==============================================================================

#[test]
fn test_explicit_directive_resolution_adversarial_inputs() {
    // A. Directives in quotes must be ignored
    let quoted_prompt = "The user asked: \"use skill: git_commit\" but do not run it.";
    let parsed = parse_prompt_directives(quoted_prompt);
    assert_eq!(parsed.len(), 0, "quoted directives must be ignored");

    // B. Directives in code blocks must be ignored
    let code_block_prompt = "```bash\nuse skill: deploy_prod\n```\nPlease ignore above.";
    let parsed_code = parse_prompt_directives(code_block_prompt);
    assert_eq!(
        parsed_code.len(),
        0,
        "directives inside code blocks must be ignored"
    );

    // C. Conflicting require and exclude directives must return ConflictingDirective
    let conflict_prompt = "use skill dangerous_skill; do not use skill dangerous_skill";
    let parsed_conflict = parse_prompt_directives(conflict_prompt);
    assert_eq!(parsed_conflict.len(), 2);

    let empty_roster = ResolvedRoster::default();

    let req = ExplicitResolutionRequest {
        cli_required_skills: Vec::new(),
        cli_excluded_skills: Vec::new(),
        context_skill_references: Vec::new(),
        context_excluded_skills: Vec::new(),
        user_prompt: Some(conflict_prompt.to_string()),
    };
    let res = resolve_explicit_requirements(&req, &empty_roster);
    match res {
        Ok(ExplicitResolutionResult::Unavailable { unresolved }) => {
            assert!(
                unresolved
                    .iter()
                    .any(|u| u.reason == UnresolvedReason::ConflictingDirective)
            );
        }
        other => panic!("expected unavailable due to conflict, got: {other:?}"),
    }

    // D. Missing explicit skill reference produces Missing without substitution
    let missing_prompt = "use skill nonexistent_skill_12345";
    let req2 = ExplicitResolutionRequest {
        cli_required_skills: Vec::new(),
        cli_excluded_skills: Vec::new(),
        context_skill_references: Vec::new(),
        context_excluded_skills: Vec::new(),
        user_prompt: Some(missing_prompt.to_string()),
    };
    let res2 = resolve_explicit_requirements(&req2, &empty_roster);
    match res2 {
        Ok(ExplicitResolutionResult::Unavailable { unresolved }) => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].target, "nonexistent_skill_12345");
            assert_eq!(unresolved[0].reason, UnresolvedReason::Missing);
        }
        other => panic!("expected missing, got: {other:?}"),
    }
}

// ==============================================================================
// 7. Scoring Normalization and Finite Arithmetic Fuzz
// ==============================================================================

#[test]
fn test_scoring_normalization_and_finite_arithmetic_fuzz() {
    let id_a = SkillId::new("skill_alpha").unwrap();
    let id_b = SkillId::new("skill_beta").unwrap();
    let id_c = SkillId::new("skill_gamma").unwrap();

    // A. Blend weights boundary validation
    assert!(Weights::new(0.0, 0.0, 0.0).is_ok());
    assert!(Weights::new(W_FIT_MAX, W_PRIOR_MAX, W_PHASE_MAX).is_ok());
    assert_eq!(
        Weights::new(W_FIT_MAX + 0.001, 0.0, 0.0).unwrap_err(),
        ScoringError::InvalidWeight
    );
    assert_eq!(
        Weights::new(0.0, W_PRIOR_MAX + 0.001, 0.0).unwrap_err(),
        ScoringError::InvalidWeight
    );
    assert_eq!(
        Weights::new(0.0, 0.0, W_PHASE_MAX + 0.001).unwrap_err(),
        ScoringError::InvalidWeight
    );

    // B. Extreme probability clipping: 0.0 and 1.0 never produce NaN or Inf in log_odds
    assert_eq!(clip(0.0), EPSILON);
    assert_eq!(clip(1.0), 1.0 - EPSILON);
    assert!(log_odds(0.0).is_finite());
    assert!(log_odds(1.0).is_finite());

    // C. Max-shifted softmax over widely divergent utilities: numerical stability
    let inputs = vec![
        Input {
            id: &id_a,
            rerank: 0.999999,
            fit: 0.999999,
            prior_delta: 0.5,
            phase_match: 1.0,
        },
        Input {
            id: &id_b,
            rerank: 0.000001,
            fit: 0.000001,
            prior_delta: -0.5,
            phase_match: 0.0,
        },
        Input {
            id: &id_c,
            rerank: 0.5,
            fit: 0.5,
            prior_delta: 0.0,
            phase_match: 0.5,
        },
    ];

    let weights = Weights::new(W_FIT_MAX, W_PRIOR_MAX, W_PHASE_MAX).unwrap();
    let ranking = rank(&inputs, weights, 2).expect("ranking succeeds");
    assert_eq!(ranking.returned.len(), 2);
    assert_eq!(ranking.eligible, 3);
    assert!(ranking.omitted_mass > 0.0);
    assert!(
        (ranking.returned[0].rank_score + ranking.returned[1].rank_score + ranking.omitted_mass
            - 1.0)
            .abs()
            < 1e-6
    );

    // Winner must be id_a
    assert_eq!(ranking.returned[0].index, 0);

    // D. Stable-ID tie breaking when utilities are exactly equal
    let tied_inputs = vec![
        Input {
            id: &id_b,
            rerank: 0.5,
            fit: 0.5,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
        Input {
            id: &id_a,
            rerank: 0.5,
            fit: 0.5,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
    ];
    let tied_ranking = rank(&tied_inputs, Weights::DEFAULT, 2).expect("ranking succeeds");
    assert_eq!(
        tied_ranking.returned[0].index, 1,
        "id_a (skill_alpha) must beat id_b (skill_beta) on tie"
    );
    assert_eq!(tied_ranking.returned[1].index, 0);
}

// ==============================================================================
// 8. Cache Fingerprint and Namespace Invariants
// ==============================================================================

#[test]
fn test_cache_fingerprint_and_namespace_invariants() {
    let key = CacheKey::from_bytes([0x42; 32]);

    // Namespace isolation across different sessions
    let ns_a = CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_workspace(WorkspaceId::new("/workspaces/demo").unwrap())
        .with_session(SessionId::new("session-A").unwrap())
        .with_branch(BranchId::new("main").unwrap())
        .with_context_epoch(ContextEpoch::new("epoch-1").unwrap())
        .with_adapter(
            AdapterId::new("claude_code").unwrap(),
            AdapterVersion::new("0.8.0").unwrap(),
        );

    let ns_b = CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_workspace(WorkspaceId::new("/workspaces/demo").unwrap())
        .with_session(SessionId::new("session-B").unwrap())
        .with_branch(BranchId::new("main").unwrap())
        .with_context_epoch(ContextEpoch::new("epoch-1").unwrap())
        .with_adapter(
            AdapterId::new("claude_code").unwrap(),
            AdapterVersion::new("0.8.0").unwrap(),
        );

    let candidates = vec![
        CandidateDigest {
            skill_id: SkillId::new("skill_alpha").unwrap(),
            content_hash: ContentHash::from_bytes(b"content-a"),
            excerpt_hash: None,
        },
        CandidateDigest {
            skill_id: SkillId::new("skill_beta").unwrap(),
            content_hash: ContentHash::from_bytes(b"content-b"),
            excerpt_hash: None,
        },
    ];

    let req_input_1 = RequestFingerprintInput {
        stage: RequestStage::Wide,
        canonical_redacted_state: b"canonical_redacted_request_bytes",
        candidates: &candidates,
        questions_digest: [1u8; 32],
        endpoint_url: "https://console.typesafe.ai",
        model: "jev-latest",
        prompt_version: "v1.2",
        adapter_version: "0.8.0",
        privacy_policy_version: "standard",
        excerpt_strategy: "head-tail",
    };

    let fp1 = compute_request_fingerprint(&key, &ns_a, &req_input_1);
    let fp2 = compute_request_fingerprint(&key, &ns_a, &req_input_1);
    assert_eq!(fp1, fp2);

    let policy = RankingPolicySnapshot {
        w_fit: 1.0,
        w_prior: 0.0,
        w_phase: 0.0,
        gate_threshold: 0.30,
        fit_threshold: 0.30,
        top_k: 5,
        max_shortlist_m: 8,
    };

    let dec_input = DecisionFingerprintInput {
        request_fingerprint: fp1,
        loaded_references: &[],
        explicit_exclusions: &[],
        ranking_policy: policy,
        prior_snapshot_id: None,
        effective_snoozes: &[],
        visibility_metadata: "claude_code:v1",
    };

    let dec_fp_a = compute_decision_fingerprint(&key, &ns_a, &dec_input).expect("decision fp A");
    let dec_fp_b = compute_decision_fingerprint(&key, &ns_b, &dec_input).expect("decision fp B");

    assert_ne!(
        dec_fp_a, dec_fp_b,
        "different sessions must never share decision fingerprint"
    );
}

// ==============================================================================
// 9. Bounded Output Documents and Control Character Sanitization
// ==============================================================================

#[test]
fn test_bounded_output_and_control_character_sanitization() {
    // A. Terminal sanitization of ANSI CSI, OSC, and control sequences
    let csi_injection = "Malicious\x1b[31;1m ANSI \x1b[0mPayload";
    let osc_injection = "Click \x1b]8;;https://evil.com\x07here\x1b]8;;\x07";
    let cursor_trick = "Before\r\x1b[2KOverwritten";
    let tabs_and_controls = "Skill\tName\x00\x08With\x07Controls";

    assert_eq!(
        sanitize_terminal_text(csi_injection),
        "Malicious ANSI Payload"
    );
    assert_eq!(sanitize_terminal_text(osc_injection), "Click here");
    assert_eq!(sanitize_terminal_text(cursor_trick), "Before Overwritten");
    assert_eq!(
        sanitize_terminal_text(tabs_and_controls),
        "Skill Name  With Controls"
    );

    // B. Diagnostic text truncation and fallback
    let safe_diag = sanitize_diagnostic_text("Safe error message", "fallback");
    assert_eq!(safe_diag, "Safe error message");

    let long_diag = "x".repeat(MAX_TEXT_BYTES + 50);
    let truncated_diag = sanitize_diagnostic_text(&long_diag, "fallback");
    assert!(truncated_diag.len() <= MAX_TEXT_BYTES);

    let empty_diag = sanitize_diagnostic_text("", "fallback message");
    assert_eq!(empty_diag, "fallback message");

    // C. Output document serialization bounding
    let doc = OutputDocument::failure(ErrorKind::InvalidUsage, false);
    let json_bytes = doc.to_json().expect("serialization succeeds");
    assert!(json_bytes.len() < MAX_OUTPUT_BYTES);
}

// ==============================================================================
// 10. Deterministic Fuzz Smoke Campaign Runner (1,500 Iterations)
// ==============================================================================

#[test]
fn test_deterministic_fuzz_smoke_campaign_runner() {
    // Fixed seed linear congruential generator for reproducible property fuzzing
    struct SimpleRng {
        state: u64,
    }
    impl SimpleRng {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }
        fn next_u32(&mut self) -> u32 {
            self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (self.state >> 32) as u32
        }
        fn next_f64(&mut self) -> f64 {
            (self.next_u32() as f64) / (u32::MAX as f64)
        }
    }

    let seeds = [0x5EED0001u64, 0x5EED0002u64, 0x5EED0003u64];
    let mut total_cases = 0;

    for &seed in &seeds {
        let mut rng = SimpleRng::new(seed);
        for i in 0..500 {
            total_cases += 1;
            let case_type = rng.next_u32() % 5;
            match case_type {
                0 => {
                    // Fuzz frontmatter parser with random byte slices
                    let len = (rng.next_u32() % 256) as usize;
                    let mut bytes = Vec::with_capacity(len + 8);
                    bytes.extend_from_slice(b"---\n");
                    for _ in 0..len {
                        bytes.push((rng.next_u32() % 256) as u8);
                    }
                    bytes.extend_from_slice(b"\n---\n");
                    let _ = parse_skill_metadata(&bytes); // must never panic
                }
                1 => {
                    // Fuzz directive parser with random text
                    let len = (rng.next_u32() % 128) as usize;
                    let text: String = (0..len)
                        .map(|_| {
                            let b = (rng.next_u32() % 128) as u8;
                            if b.is_ascii() { b as char } else { ' ' }
                        })
                        .collect();
                    let _ = parse_prompt_directives(&text); // must never panic
                }
                2 => {
                    // Fuzz scoring utility and ranking
                    let rerank = rng.next_f64();
                    let fit = rng.next_f64();
                    let prior = (rng.next_f64() - 0.5) * 2.0; // [-1.0, 1.0]
                    let phase = rng.next_f64();

                    let id1 = SkillId::new("skill_fuzz_1").unwrap();
                    let id2 = SkillId::new("skill_fuzz_2").unwrap();
                    let inputs = vec![
                        Input {
                            id: &id1,
                            rerank,
                            fit,
                            prior_delta: prior,
                            phase_match: phase,
                        },
                        Input {
                            id: &id2,
                            rerank: 1.0 - rerank,
                            fit: 1.0 - fit,
                            prior_delta: -prior,
                            phase_match: 1.0 - phase,
                        },
                    ];
                    let w = Weights::DEFAULT;
                    let res = rank(&inputs, w, 2);
                    assert!(
                        res.is_ok(),
                        "scoring must succeed for valid bounds: iteration {i}"
                    );
                    let r = res.unwrap();
                    assert_eq!(r.returned.len(), 2);
                    assert!(r.returned[0].rank_score.is_finite());
                    assert!(r.returned[1].rank_score.is_finite());
                }
                3 => {
                    // Fuzz terminal text sanitizer
                    let len = (rng.next_u32() % 256) as usize;
                    let text: String = (0..len)
                        .map(|_| match rng.next_u32() % 10 {
                            0 => '\x1b',
                            1 => '\r',
                            2 => '\n',
                            3 => '\t',
                            4 => '\x07',
                            5 => '\x08',
                            _ => ((rng.next_u32() % 95) + 32) as u8 as char,
                        })
                        .collect();
                    let sanitized = sanitize_terminal_text(&text);
                    assert!(!sanitized.contains('\x1b'));
                    assert!(!sanitized.contains('\x07'));
                    assert!(!sanitized.contains('\x08'));
                }
                _ => {
                    // Fuzz JSON parse line
                    let len = (rng.next_u32() % 128) as usize;
                    let mut bytes = Vec::with_capacity(len);
                    for _ in 0..len {
                        bytes.push((rng.next_u32() % 256) as u8);
                    }
                    let _ = parse_line(&bytes); // must never panic
                }
            }
        }
    }

    assert_eq!(
        total_cases, 1500,
        "expected exactly 1,500 campaign iterations executed"
    );
}
