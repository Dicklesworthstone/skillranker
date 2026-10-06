//! Tests for replay case validation, safe import, and policy comparison (sr-roadmap-l1i.6.14, sr-roadmap-l1i.6.16).

use serde_json::json;
use skillranker::output::{CliExit, GateStatus, RunStatus, SCHEMA_VERSION};
use skillranker::replay::{
    CandidateFitItem, CapturedCandidate, CapturedLoadedReference, CapturedLocalEvidence,
    CapturedRequest, CapturedScoringProfile, ChoiceDistributionItem, RecordedRerankChoice,
    RecordedResponses, RecordedWideChoice, ReplayCase, ReplayManifest, ReplayPolicy,
    execute_replay,
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
        frozen_inputs: None,
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
fn replay_rejects_identical_and_conflicting_duplicate_fit_definitions() {
    let baseline = sample_ranked_case();
    assert!(execute_replay(&baseline, None).is_ok());
    for fit in [0.90, 0.10] {
        let mut case = baseline.clone();
        case.recorded_responses
            .rerank
            .as_mut()
            .unwrap()
            .fits
            .push(CandidateFitItem {
                skill_id: "s_triage".into(),
                fit,
            });
        assert!(case.validate().is_err(), "duplicate fit {fit}");
        assert!(
            execute_replay(&case, None).is_err(),
            "in-memory duplicate fit {fit}"
        );
        assert!(ReplayCase::from_json_bytes(&serde_json::to_vec(&case).unwrap()).is_err());
    }
}

#[test]
fn replay_only_suppresses_a_matching_available_reference() {
    let baseline = sample_ranked_case();
    for (kind, hash_matches, availability, suppressed) in [
        ("workflow", true, "available", false),
        ("unknown", true, "available", false),
        ("reference", false, "available", false),
        ("reference", true, "unknown", false),
        ("reference", true, "available", true),
    ] {
        let mut case = baseline.clone();
        let candidate = &mut case.captured_request.candidate_options[0];
        candidate.usage_kind = kind.into();
        case.local_evidence
            .loaded_references
            .push(CapturedLoadedReference {
                skill_id: candidate.skill_id.clone(),
                content_hash: if hash_matches {
                    candidate.content_hash.clone()
                } else {
                    "9".repeat(64)
                },
                availability: availability.into(),
            });
        let outcome = execute_replay(&case, None).unwrap();
        let skills = outcome.document.as_value()["recomputed"]["skills"]
            .as_array()
            .unwrap();
        let present = skills.iter().any(|s| s["skill_id"] == "s_triage");
        assert_eq!(present, !suppressed, "{kind}/{hash_matches}/{availability}");
    }
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
fn replay_execution_refuses_an_unvalidated_in_memory_case() {
    // Built in memory, so it never passed through from_json_bytes. Without __none__
    // the beat-none rule would fail open if execution trusted it.
    let mut case = sample_ranked_case();
    let rerank = case.recorded_responses.rerank.as_mut().unwrap();
    rerank.distribution.retain(|d| d.option_id != "__none__");
    rerank.distribution[1].probability = 0.20;
    assert!(execute_replay(&case, None).is_err());
    // The honest twin still executes.
    assert!(execute_replay(&sample_ranked_case(), None).is_ok());
}

#[test]
fn replay_case_rejects_distribution_not_summing_to_one() {
    let mut case = sample_ranked_case();
    case.recorded_responses.wide.as_mut().unwrap().distribution[0].probability = 0.10; // Sum becomes 0.25, far from 1.0
    assert!(case.validate().is_err());
}

#[test]
fn legacy_replay_recomputes_ranked_without_claiming_exact_input_parity() {
    let case = sample_ranked_case();
    let outcome = execute_replay(&case, None).expect("replay execution");
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
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
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
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

#[test]
fn replay_missing_gate_is_unknown_and_cannot_be_replaced_by_none() {
    let mut case = sample_ranked_case();
    assert_eq!(
        execute_replay(&case, None).unwrap().run_status,
        RunStatus::Complete
    );
    case.recorded_responses.wide.as_mut().unwrap().gate_score = None;
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(outcome.run_status, RunStatus::Partial);
    assert!(outcome.recomputed_decision.is_none());
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_required"],
        2
    );
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_completed"],
        2
    );
    assert!(outcome.document.as_value().get("recomputed").is_none());
}

#[test]
fn replay_comparison_serializes_the_missing_stage_as_partial() {
    let mut case = sample_ranked_case();
    case.recorded_responses.wide.as_mut().unwrap().gate_score = Some(0.15);
    case.recorded_responses.rerank = None;
    case.manifest.stages_recorded = vec!["wide".into()];
    case.historical_decision = abstain_decision_fixture("low-need");
    let lowered = ReplayPolicy {
        gate_threshold: Some(0.1),
        ..Default::default()
    };
    let outcome =
        skillranker::replay::execute_replay_comparison(&case, None, Some(&lowered)).unwrap();
    let envelope = outcome.document.as_value();
    assert_eq!(outcome.run_status, RunStatus::Partial);
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
    assert_eq!(envelope["run_status"], "partial");
    assert_eq!(envelope["gate_status"], "not-established");
    assert_eq!(envelope["completeness"]["evidence_compatible"], false);
    assert_eq!(envelope["completeness"]["stages_required"], 2);
    assert_eq!(envelope["completeness"]["stages_completed"], 1);
    assert!(envelope.get("comparison").is_none());
}

#[test]
fn replay_json_roundtrip_preserves_gate_numeric_bits() {
    let mut case = sample_ranked_case();
    let gate = 0.09999999999999999_f64;
    case.recorded_responses.wide.as_mut().unwrap().gate_score = Some(gate);
    let bytes = serde_json::to_vec(&case).unwrap();
    let decoded = ReplayCase::from_json_bytes(&bytes).unwrap();
    assert_eq!(
        decoded
            .recorded_responses
            .wide
            .unwrap()
            .gate_score
            .unwrap()
            .to_bits(),
        gate.to_bits()
    );
}

#[test]
fn missing_wide_cannot_reproduce_a_ranked_inference() {
    let mut case = sample_ranked_case();
    assert_eq!(
        execute_replay(&case, None).unwrap().run_status,
        RunStatus::Complete
    );
    case.recorded_responses.wide = None;
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(outcome.run_status, RunStatus::Partial);
    assert!(outcome.recomputed_decision.is_none());
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_required"],
        2
    );
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_completed"],
        1
    );
    case.recorded_responses.rerank = None;
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_completed"],
        0
    );
}

#[test]
fn missing_wide_cannot_replay_an_inferred_abstention_as_local_metadata() {
    let mut case = sample_ranked_case();
    case.recorded_responses.wide.as_mut().unwrap().gate_score = Some(0.15);
    case.recorded_responses.rerank = None;
    case.manifest.stages_recorded = vec!["wide".into()];
    case.historical_decision = abstain_decision_fixture("low-need");
    case.historical_decision["needs_skill"] = json!(0.15);
    let positive = execute_replay(&case, None).unwrap();
    assert_eq!(positive.run_status, RunStatus::Complete);
    assert_eq!(positive.recomputed_decision.as_deref(), Some("abstain"));
    assert_eq!(
        positive.document.as_value()["completeness"]["stages_completed"],
        1
    );

    case.recorded_responses.wide = None;
    case.manifest.stages_recorded.clear();
    let missing = execute_replay(&case, None).unwrap();
    assert_eq!(missing.run_status, RunStatus::Partial);
    assert_eq!(missing.gate_status, GateStatus::NotEstablished);
    assert!(missing.recomputed_decision.is_none());
    assert_eq!(
        missing.document.as_value()["completeness"]["stages_required"],
        1
    );
    assert_eq!(
        missing.document.as_value()["completeness"]["stages_completed"],
        0
    );

    // A genuinely local abstention did not need an unrecorded provider answer.
    case.historical_decision = abstain_decision_fixture("local-exclusion");
    let local = execute_replay(&case, None).unwrap();
    assert_eq!(local.run_status, RunStatus::Complete);
    assert_eq!(local.gate_status, GateStatus::NotApplicable);
    assert_eq!(local.recomputed_decision.as_deref(), Some("abstain"));
    assert_eq!(
        local.document.as_value()["completeness"]["stages_required"],
        0
    );
}

#[test]
fn unavailable_replay_keeps_terminal_failure_and_unknown_usage() {
    let mut case = sample_ranked_case();
    case.recorded_responses.rerank = None;
    case.historical_decision = skillranker::output::OutputDocument::failure_with_details(
        skillranker::output::ErrorKind::Timeout,
        "provider did not finish",
        "retry deliberately",
        true,
    )
    .as_value()
    .clone();
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.gate_status, GateStatus::NotApplicable);
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_required"],
        0
    );
    assert_eq!(
        outcome.document.as_value()["completeness"]["stages_completed"],
        0
    );
    assert_eq!(
        outcome.document.as_value()["recomputed"],
        case.historical_decision
    );
    assert_eq!(
        outcome.document.as_value()["completeness"]["evidence_compatible"],
        false
    );
}

#[test]
fn new_frozen_semantics_cannot_hide_inside_legacy_schema() {
    let mut case = sample_ranked_case();
    assert!(case.validate().is_ok());
    case.frozen_inputs = Some(skillranker::replay::frozen::FrozenReplayInputs::new(
        "local",
    ));
    assert!(case.validate().is_err());
    case.frozen_inputs = None;
    case.schema_version = 3;
    assert!(case.validate().is_err());
}

#[test]
fn replay_import_diagnostics_never_echo_private_values_or_keys() {
    let canary = "sk-aB7cD8eF9gH0jK1mN2pQ3rS4tU5vW6xY";
    assert!(sample_ranked_case().validate().is_ok());
    let mut case = sample_ranked_case();
    case.manifest.evidence_origin = canary.into();
    let error = case.validate().unwrap_err();
    assert!(!error.to_string().contains(canary));
    assert!(!format!("{error:?}").contains(canary));
    let raw = format!("{{\"{canary}\":null,\"{canary}\":null}}");
    let error = ReplayCase::from_json_bytes(raw.as_bytes()).unwrap_err();
    assert!(!error.to_string().contains(canary));
    assert!(!format!("{error:?}").contains(canary));
    let policy = format!("{{\"{canary}\":true}}");
    let error = ReplayPolicy::from_json_bytes(policy.as_bytes()).unwrap_err();
    assert!(!error.to_string().contains(canary));
    assert!(!format!("{error:?}").contains(canary));
}

#[test]
fn replay_import_rejects_trailing_json_and_unknown_policy_fields() {
    let mut bytes = serde_json::to_vec(&sample_ranked_case()).unwrap();
    assert!(ReplayCase::from_json_bytes(&bytes).is_ok());
    bytes.extend_from_slice(b" {} ");
    assert!(ReplayCase::from_json_bytes(&bytes).is_err());
    assert!(ReplayPolicy::from_json_bytes(br#"{"gate_threshold":0.3}"#).is_ok());
    assert!(ReplayPolicy::from_json_bytes(br#"{"model":"different"}"#).is_err());
    assert!(ReplayPolicy::from_json_bytes(br#"{"gate_threshold":0.3}{}"#).is_err());
}
