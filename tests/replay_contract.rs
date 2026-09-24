//! Tests for replay case validation, safe import, and policy comparison (sr-roadmap-l1i.6.14, sr-roadmap-l1i.6.16).

use serde_json::json;
use skillranker::output::{CliExit, GateStatus, RunStatus, SCHEMA_VERSION};
use skillranker::replay::{
    CandidateFitItem, CapturedCandidate, CapturedLocalEvidence, CapturedRequest,
    CapturedScoringProfile, ChoiceDistributionItem, RecordedRerankChoice, RecordedResponses,
    RecordedWideChoice, ReplayCase, ReplayManifest, ReplayPolicy, execute_replay,
};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

fn temp_replay_dir(name: &str) -> PathBuf {
    let dir = Path::new("/tmp").join(format!(
        "sr-replay-test-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)
        .unwrap();
    dir
}

fn ranked_decision_fixture() -> serde_json::Value {
    let mut v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/output-ranked.v1.json")).unwrap();
    v["skills"][0]["skill_id"] = json!("s_triage");
    v["skills"][0]["name"] = json!("rust-test-triage");
    v["skills"][0]["invocation_name"] = json!("rust-test-triage");
    v["skills"][1]["skill_id"] = json!("s_review");
    v["skills"][1]["name"] = json!("rust-code-review");
    v["skills"][1]["invocation_name"] = json!("rust-code-review");
    v
}

fn abstain_decision_fixture(reason: &str) -> serde_json::Value {
    let mut v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/output-abstain.v1.json")).unwrap();
    v["reason"] = json!(reason);
    v
}

fn sample_ranked_case() -> ReplayCase {
    ReplayCase {
        schema_version: SCHEMA_VERSION,
        case_id: "case-001-rust-audit".into(),
        created_at_unix_ms: 1726700000000,
        manifest: ReplayManifest {
            evidence_origin: "recorded".into(),
            adapter: "claude_code".into(),
            model: Some("claude-3-7-sonnet".into()),
            stages_recorded: vec!["wide".into(), "rerank".into()],
            prompt_summary: Some("Triage failing rust tests".into()),
        },
        captured_request: CapturedRequest {
            context_text: Some("We have an issue in our tests".into()),
            current_constraints: vec!["no release work".into()],
            candidate_options: vec![
                CapturedCandidate {
                    skill_id: "s_triage".into(),
                    invocation_name: "rust-test-triage".into(),
                    content_hash:
                        "0000000000000000000000000000000000000000000000000000000000000001".into(),
                    source: "workspace".into(),
                    usage_kind: "workflow".into(),
                    visibility: None,
                    description: Some("Triage failing rust tests".into()),
                    excerpt: None,
                },
                CapturedCandidate {
                    skill_id: "s_review".into(),
                    invocation_name: "rust-code-review".into(),
                    content_hash:
                        "0000000000000000000000000000000000000000000000000000000000000002".into(),
                    source: "workspace".into(),
                    usage_kind: "workflow".into(),
                    visibility: None,
                    description: Some("Review rust code".into()),
                    excerpt: None,
                },
            ],
        },
        recorded_responses: RecordedResponses {
            wide: Some(RecordedWideChoice {
                choice: "s_triage".into(),
                choices_probability: 0.85,
                gate_score: Some(0.85),
                distribution: vec![
                    ChoiceDistributionItem {
                        option_id: "s_triage".into(),
                        probability: 0.85,
                    },
                    ChoiceDistributionItem {
                        option_id: "s_review".into(),
                        probability: 0.10,
                    },
                    ChoiceDistributionItem {
                        option_id: "__none__".into(),
                        probability: 0.05,
                    },
                ],
            }),
            rerank: Some(RecordedRerankChoice {
                choice: "s_triage".into(),
                choices_probability: 0.80,
                stated_confidence: Some(0.81),
                fits: vec![
                    CandidateFitItem {
                        skill_id: "s_triage".into(),
                        fit: 0.90,
                    },
                    CandidateFitItem {
                        skill_id: "s_review".into(),
                        fit: 0.40,
                    },
                ],
                distribution: vec![
                    ChoiceDistributionItem {
                        option_id: "s_triage".into(),
                        probability: 0.80,
                    },
                    ChoiceDistributionItem {
                        option_id: "s_review".into(),
                        probability: 0.15,
                    },
                    ChoiceDistributionItem {
                        option_id: "__none__".into(),
                        probability: 0.05,
                    },
                ],
            }),
        },
        local_evidence: CapturedLocalEvidence {
            as_of_unix_ms: 1726700000000,
            active_snoozes: Vec::new(),
            loaded_references: Vec::new(),
            scoring_profile: CapturedScoringProfile {
                gate_threshold: 0.30,
                fit_threshold: 0.30,
                w_fit: 1.0,
                w_prior: 0.0,
                w_phase: 0.0,
                top_k: 5,
            },
        },
        historical_decision: ranked_decision_fixture(),
    }
}

#[test]
fn replay_case_validates_and_roundtrips() {
    let case = sample_ranked_case();
    let json_bytes = serde_json::to_vec(&case).unwrap();
    let parsed = ReplayCase::from_json_bytes(&json_bytes).expect("valid case must parse");
    assert_eq!(parsed.case_id, case.case_id);
    assert_eq!(parsed.manifest.evidence_origin, "recorded");
}

#[test]
fn replay_case_rejects_duplicate_json_keys() {
    let case = sample_ranked_case();
    let json_str = serde_json::to_string(&case).unwrap();
    let duplicate_json = json_str.replacen(
        "\"schema_version\":1,",
        "\"schema_version\":1,\"schema_version\":1,",
        1,
    );
    assert!(ReplayCase::from_json_bytes(duplicate_json.as_bytes()).is_err());
}

#[test]
fn replay_case_rejects_none_as_candidate_skill_id() {
    let mut case = sample_ranked_case();
    case.captured_request.candidate_options[0].skill_id = "__none__".into();
    assert!(case.validate().is_err());
}

#[test]
fn replay_case_rejects_option_map_mismatch() {
    let mut case = sample_ranked_case();
    // Recorded rerank refers to a skill not in the captured candidate set
    case.recorded_responses.rerank.as_mut().unwrap().choice = "unknown_skill".into();
    assert!(case.validate().is_err());
}

#[test]
fn replay_rejects_recorded_answers_the_live_codec_would_refuse() {
    assert!(sample_ranked_case().validate().is_ok(), "the honest twin");
    // Without __none__, its probability read as zero and every candidate
    // beat it: the beat-none rule failed open.
    let mut case = sample_ranked_case();
    let rerank = case.recorded_responses.rerank.as_mut().unwrap();
    rerank.distribution.retain(|d| d.option_id != "__none__");
    rerank.distribution[1].probability = 0.20;
    assert!(case.validate().is_err(), "rerank without __none__");
    // The wide question offers every candidate.
    let mut case = sample_ranked_case();
    let wide = case.recorded_responses.wide.as_mut().unwrap();
    wide.distribution.retain(|d| d.option_id != "s_review");
    wide.distribution[0].probability = 0.95;
    assert!(case.validate().is_err(), "wide answer missing a candidate");
    // The recorded choice must be a maximum-probability option.
    let mut case = sample_ranked_case();
    case.recorded_responses.rerank.as_mut().unwrap().choice = "s_review".into();
    assert!(case.validate().is_err(), "choice below the maximum");
    // Fits and distribution must name one shortlist.
    let mut case = sample_ranked_case();
    case.recorded_responses.rerank.as_mut().unwrap().fits.pop();
    assert!(case.validate().is_err(), "fits for a different shortlist");
}

#[test]
fn replay_case_rejects_distribution_not_summing_to_one() {
    let mut case = sample_ranked_case();
    case.recorded_responses.wide.as_mut().unwrap().distribution[0].probability = 0.10; // Sum becomes 0.25, far from 1.0
    assert!(case.validate().is_err());
}

#[test]
fn replay_executes_identical_ranked_decision() {
    let case = sample_ranked_case();
    let outcome = execute_replay(&case, None).expect("replay execution");
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.gate_status, GateStatus::Passed);
    assert_eq!(outcome.historical_decision, "ranked");
    assert_eq!(outcome.recomputed_decision.as_deref(), Some("ranked"));

    // Verify OutputDocument contract
    assert_eq!(outcome.document.as_value()["schema_version"], 1);
    assert_eq!(outcome.document.as_value()["actionable"], false);
    assert_eq!(outcome.document.exit_code(), CliExit::Success);
}

#[test]
fn replay_recomputes_with_policy_overrides() {
    let case = sample_ranked_case();
    // Policy override: higher fit threshold of 0.95 (which s_triage at 0.90 fails)
    let policy = ReplayPolicy {
        fit_threshold: Some(0.95),
        ..Default::default()
    };
    let outcome = execute_replay(&case, Some(&policy)).expect("replay execution");
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.historical_decision, "ranked");
    // With fit threshold 0.95, no candidate qualifies -> abstains
    assert_eq!(outcome.recomputed_decision.as_deref(), Some("abstain"));
}

#[test]
fn replay_rejects_uncaptured_prior_policy() {
    let case = sample_ranked_case();
    // Turning on prior when not captured must be refused as incompatible policy
    let policy = ReplayPolicy {
        w_prior: Some(0.2),
        ..Default::default()
    };
    assert!(execute_replay(&case, Some(&policy)).is_err());
}

#[test]
fn replay_low_gate_abstention_without_rerank_is_complete() {
    let mut case = sample_ranked_case();
    // Low gate score, historical decision was abstain, no rerank response recorded
    case.recorded_responses.wide.as_mut().unwrap().gate_score = Some(0.15);
    case.recorded_responses.rerank = None;
    case.historical_decision = abstain_decision_fixture("low-fit");

    // Replay with default policy: correctly reproduces the low-gate abstention
    let outcome = execute_replay(&case, None).expect("replay execution");
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.gate_status, GateStatus::Passed);
    assert_eq!(outcome.recomputed_decision.as_deref(), Some("abstain"));

    // Now attempt a policy that lowers the gate threshold to 0.10:
    // This requires a rerank response that was not recorded, so it must report partial and not-established!
    let policy = ReplayPolicy {
        gate_threshold: Some(0.10),
        ..Default::default()
    };
    let outcome = execute_replay(&case, Some(&policy)).expect("replay execution");
    assert_eq!(outcome.run_status, RunStatus::Partial);
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
    assert!(outcome.recomputed_decision.is_none());
    assert!(
        outcome
            .explanation
            .unwrap()
            .contains("missing recorded rerank")
    );
}

#[test]
fn replay_synthetic_case_uses_not_applicable_gate() {
    let mut case = sample_ranked_case();
    case.manifest.evidence_origin = "synthetic".into();
    let outcome = execute_replay(&case, None).expect("replay execution");
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.gate_status, GateStatus::NotApplicable);
}

#[test]
fn replay_save_and_load_enforces_owner_only_and_no_clobber() {
    let dir = temp_replay_dir("export-contract");
    let target = dir.join("case.json");
    let case = sample_ranked_case();

    // First save succeeds
    case.save_to_file(&target)
        .expect("initial save must succeed");
    assert!(target.exists());

    // Second save must fail due to atomic no-clobber guarantee
    assert!(case.save_to_file(&target).is_err());

    // Load from file verifies permissions and data
    let loaded = ReplayCase::load_from_file(&target).expect("load from file");
    assert_eq!(loaded.case_id, case.case_id);
}
