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

#[test]
fn replay_missing_fits_are_unknown_even_at_zero_threshold() {
    let mut case = sample_ranked_case();
    assert_eq!(
        execute_replay(&case, None).unwrap().run_status,
        RunStatus::Complete
    );
    case.recorded_responses
        .rerank
        .as_mut()
        .unwrap()
        .fits
        .clear();
    for threshold in [0.0, 0.3] {
        let outcome = execute_replay(
            &case,
            Some(&ReplayPolicy {
                fit_threshold: Some(threshold),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(
            outcome.run_status,
            RunStatus::Partial,
            "missing fits at {threshold}"
        );
        assert!(outcome.recomputed_decision.is_none());
        assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
    }
}

#[test]
fn replay_policy_rejects_each_invalid_weight_on_its_own() {
    for bytes in [
        br#"{"w_fit":4.01}"#.as_slice(),
        br#"{"w_prior":0.51}"#.as_slice(),
        br#"{"w_phase":1.01}"#.as_slice(),
    ] {
        assert!(ReplayPolicy::from_json_bytes(bytes).is_err());
    }
    for bytes in [
        br#"{"w_fit":4.0}"#.as_slice(),
        br#"{"w_prior":0.5}"#.as_slice(),
        br#"{"w_phase":1.0}"#.as_slice(),
    ] {
        assert!(ReplayPolicy::from_json_bytes(bytes).is_ok());
    }
}

#[test]
fn replay_scoring_uses_live_shortlist_order_instead_of_capture_order() {
    use skillranker::identity::SkillId;
    use skillranker::scoring::{Input, Weights, rank};
    let mut case = sample_ranked_case();
    case.manifest.evidence_origin = "synthetic".into();
    for field in ["total", "eligible", "wide_candidates", "shortlist"] {
        case.historical_decision["roster"][field] = json!(16);
    }
    let template = case.captured_request.candidate_options[0].clone();
    case.captured_request.candidate_options = (0..16)
        .map(|index| {
            let mut candidate = template.clone();
            candidate.skill_id = format!("s_score{index:02}");
            candidate.invocation_name = format!("score-{index:02}");
            candidate
        })
        .collect();
    let ids: Vec<_> = case
        .captured_request
        .candidate_options
        .iter()
        .map(|c| SkillId::new(&c.skill_id).unwrap())
        .collect();
    let raw: Vec<_> = (0..16).map(|i| 1.0 / (i + 1) as f64).collect();
    let total: f64 = raw.iter().sum();
    let probabilities: Vec<_> = raw.iter().map(|v| v * 0.999 / total).collect();
    let fits: Vec<_> = (0..16)
        .map(|i| 0.31 + (i * 7 % 23) as f64 * 0.028)
        .collect();
    let wide = case.recorded_responses.wide.as_mut().unwrap();
    wide.choice = ids[0].as_str().into();
    wide.choices_probability = 16.0 * 0.99 / 136.0;
    wide.distribution = ids
        .iter()
        .enumerate()
        .map(|(i, id)| ChoiceDistributionItem {
            option_id: id.as_str().into(),
            probability: (16 - i) as f64 * 0.99 / 136.0,
        })
        .chain(std::iter::once(ChoiceDistributionItem {
            option_id: "__none__".into(),
            probability: 0.01,
        }))
        .collect();
    let rerank = case.recorded_responses.rerank.as_mut().unwrap();
    rerank.choice = ids[0].as_str().into();
    rerank.choices_probability = probabilities[0];
    rerank.distribution = ids
        .iter()
        .enumerate()
        .map(|(i, id)| ChoiceDistributionItem {
            option_id: id.as_str().into(),
            probability: probabilities[i],
        })
        .chain(std::iter::once(ChoiceDistributionItem {
            option_id: "__none__".into(),
            probability: 0.001,
        }))
        .collect();
    rerank.fits = ids
        .iter()
        .enumerate()
        .map(|(i, id)| CandidateFitItem {
            skill_id: id.as_str().into(),
            fit: fits[i],
        })
        .collect();
    let inputs: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Input {
            id,
            rerank: probabilities[i],
            fit: fits[i],
            prior_delta: 0.0,
            phase_match: 0.0,
        })
        .collect();
    let expected = rank(&inputs, Weights::DEFAULT, 5).unwrap();
    // This permutation changed scores by one ULP in the actual installed CLI.
    let candidates = case.captured_request.candidate_options.clone();
    for order in [
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        vec![15, 3, 2, 12, 8, 9, 13, 11, 7, 10, 1, 4, 6, 0, 5, 14],
    ] {
        case.captured_request.candidate_options =
            order.iter().map(|&i| candidates[i].clone()).collect();
        let outcome = execute_replay(&case, None).unwrap();
        let recomputed = &outcome.document.as_value()["recomputed"];
        assert_eq!(
            recomputed["omitted_rank_mass"].as_f64().unwrap().to_bits(),
            expected.omitted_mass.to_bits()
        );
        let actual_skills = recomputed["skills"].as_array().unwrap();
        assert_eq!(actual_skills.len(), expected.returned.len());
        for (actual, score) in actual_skills.iter().zip(&expected.returned) {
            assert_eq!(actual["skill_id"], ids[score.index].as_str());
            assert_eq!(
                actual["rank_score"].as_f64().unwrap().to_bits(),
                score.rank_score.to_bits()
            );
        }
    }
}

#[test]
fn replay_low_fit_reason_ignores_locally_ineligible_candidates() {
    let mut case = sample_ranked_case();
    case.local_evidence.active_snoozes.push("s_triage".into());
    case.recorded_responses.rerank.as_mut().unwrap().fits[1].fit = 0.1;
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(
        outcome.document.as_value()["recomputed"]["reason"],
        "low-fit"
    );
    // Unsnoozing the actually offered useful candidate preserves the success case.
    case.local_evidence.active_snoozes.clear();
    assert_eq!(
        execute_replay(&case, None)
            .unwrap()
            .recomputed_decision
            .as_deref(),
        Some("ranked")
    );
}

#[test]
fn replay_low_fit_reason_does_not_count_unoffered_zero_fits() {
    let mut case = sample_ranked_case();
    let rerank = case.recorded_responses.rerank.as_mut().unwrap();
    rerank.fits.remove(1);
    rerank.fits[0].fit = 0.1;
    rerank.distribution.remove(1);
    rerank.distribution[0].probability = 0.95;
    rerank.choices_probability = 0.95;
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(
        outcome.document.as_value()["recomputed"]["reason"],
        "low-fit"
    );
    let outcome = execute_replay(
        &case,
        Some(&ReplayPolicy {
            fit_threshold: Some(0.0),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(outcome.recomputed_decision.as_deref(), Some("ranked"));
    assert_eq!(
        outcome.document.as_value()["recomputed"]["skills"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn replay_preserves_declared_roster_beyond_the_captured_wide_subset() {
    for overflow in [false, true] {
        let mut case = sample_ranked_case();
        if overflow {
            case.historical_decision["roster"]["total"] = json!(300);
            case.historical_decision["roster"]["eligible"] = json!(280);
            case.historical_decision["roster"]["retrieval"] = json!("quill-bm25");
        }
        let roster = case.historical_decision["roster"].clone();
        let ranked = execute_replay(&case, None).unwrap();
        assert_eq!(ranked.recomputed_decision.as_deref(), Some("ranked"));
        assert_eq!(ranked.document.as_value()["recomputed"]["roster"], roster);
        assert_eq!(ranked.gate_status, GateStatus::NotEstablished);
        assert_eq!(
            ranked.document.as_value()["input_completeness"]["visible_roster"],
            false
        );
        for policy in [
            ReplayPolicy {
                fit_threshold: Some(1.0),
                ..Default::default()
            },
            ReplayPolicy {
                gate_threshold: Some(1.0),
                ..Default::default()
            },
        ] {
            let abstained = execute_replay(&case, Some(&policy)).unwrap();
            assert_eq!(abstained.recomputed_decision.as_deref(), Some("abstain"));
            assert_eq!(
                abstained.document.as_value()["recomputed"]["roster"],
                roster
            );
            assert_eq!(abstained.gate_status, GateStatus::NotEstablished);
        }
    }
}
