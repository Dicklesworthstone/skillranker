//! Batch execution must use actual replay inputs, never manufacture observations
//! from an oracle. A successful offline replay is not a relevance quality gate.

use serde_json::{Value, json};
use skillranker::evaluation::batch::{
    BatchConfig, CaseExecutionStatus, EvidenceOrigin, execute_evaluation_batch,
};
use skillranker::output::{GateStatus, OutputKind, RunStatus};
use skillranker::replay::ReplayCase;
use skillranker::runtime::EntryClock;
use std::io::Cursor;

fn replay_case() -> Value {
    let historical: Value =
        serde_json::from_str(include_str!("fixtures/output-ranked.v1.json")).unwrap();
    let candidates: Vec<_> = historical["skills"].as_array().unwrap().iter().map(|skill| json!({
        "skill_id": skill["skill_id"], "invocation_name": skill["invocation_name"],
        "content_hash": skill["content_hash"], "source": "workspace", "usage_kind": "workflow"
    })).collect();
    let value = json!({
        "schema_version": 1, "case_id": "recorded-case", "created_at_unix_ms": 1726700000000_u64,
        "manifest": {"evidence_origin": "recorded", "adapter": "claude_code", "stages_recorded": ["wide", "rerank"]},
        "captured_request": {"candidate_options": candidates},
        "recorded_responses": {
            "wide": {"choice": "s_01", "choices_probability": 0.6, "gate_score": 0.8,
                "distribution": [{"option_id":"s_01","probability":0.6},{"option_id":"s_02","probability":0.3},{"option_id":"__none__","probability":0.1}]},
            "rerank": {"choice": "s_01", "choices_probability": 0.6, "stated_confidence": 0.8,
                "fits": [{"skill_id":"s_01","fit":0.8},{"skill_id":"s_02","fit":0.5}],
                "distribution": [{"option_id":"s_01","probability":0.6},{"option_id":"s_02","probability":0.3},{"option_id":"__none__","probability":0.1}]}
        },
        "local_evidence": {"as_of_unix_ms":1726700000000_u64,"active_snoozes":[],"loaded_references":[],
            "scoring_profile":{"gate_threshold":0.3,"fit_threshold":0.3,"w_fit":1.0,"w_prior":0.0,"w_phase":0.0,"top_k":5}},
        "historical_decision": historical
    });
    ReplayCase::from_value(value.clone()).expect("fixture must be valid replay evidence");
    value
}

fn run(
    value: Value,
    config: &BatchConfig,
) -> skillranker::evaluation::batch::EvaluationBatchReport {
    execute_evaluation_batch(
        Cursor::new(serde_json::to_vec(&value).unwrap()),
        config,
        &EntryClock::capture().unwrap(),
    )
    .unwrap()
}

#[test]
fn offline_replay_batch_zero_network_completes_and_validates_document() {
    let report = run(replay_case(), &BatchConfig::default());
    assert_eq!(report.run_status, RunStatus::Complete);
    assert_eq!(report.gate_status, GateStatus::NotEstablished);
    assert_eq!(report.evidence_origin, "recorded");
    assert_eq!(report.accounting.http_attempts, 0);
    assert_eq!(report.accounting.requests, 0);
    assert_eq!(report.completeness.cases_requested, 1);
    assert_eq!(report.completeness.cases_completed, 1);
    assert_eq!(report.completeness.stages_completed, 2);
    assert!(report.completeness.evidence_compatible);
    assert!(
        matches!(&report.cases[0].status, CaseExecutionStatus::Completed { decision, loss: None, normalized_loss: None, .. } if decision == "ranked")
    );
    assert_eq!(
        report.to_document().unwrap().kind(),
        OutputKind::Artifact(skillranker::output::ArtifactKind::Report)
    );
}

#[test]
fn online_mode_without_network_consent_is_refused_with_privacy_error() {
    let config = BatchConfig {
        online: true,
        max_requests: Some(10),
        ..Default::default()
    };
    let err = execute_evaluation_batch(Cursor::new(b""), &config, &EntryClock::capture().unwrap())
        .unwrap_err();
    assert!(err.to_string().contains("requires --allow-network"));
}

#[test]
fn online_mode_without_explicit_request_cap_is_refused_with_usage_error() {
    let config = BatchConfig {
        online: true,
        allow_network: true,
        ..Default::default()
    };
    let err = execute_evaluation_batch(Cursor::new(b""), &config, &EntryClock::capture().unwrap())
        .unwrap_err();
    assert!(err.to_string().contains("--max-requests cap"));
}

#[test]
fn online_replay_is_refused_because_live_batches_rank_labeled_cases() {
    let config = BatchConfig {
        online: true,
        allow_network: true,
        max_requests: Some(10),
        ..Default::default()
    };
    let err = execute_evaluation_batch(Cursor::new(b""), &config, &EntryClock::capture().unwrap())
        .unwrap_err();
    // A replay dataset has no labeled requests to rank fresh; online mode must
    // never fall back to reporting an offline replay as a live evaluation.
    assert!(
        err.to_string().contains("replay datasets run offline"),
        "{err}"
    );
}

#[test]
fn missing_required_recorded_stage_reports_not_estimable_without_silent_dropping() {
    for stage in ["wide", "rerank"] {
        let mut case = replay_case();
        case["recorded_responses"]
            .as_object_mut()
            .unwrap()
            .remove(stage);
        let report = run(case, &BatchConfig::default());
        assert_eq!(report.run_status, RunStatus::Partial);
        assert_eq!(report.gate_status, GateStatus::NotEstablished);
        assert_eq!(report.completeness.cases_requested, 1);
        assert_eq!(report.completeness.cases_completed, 0);
        assert_eq!(report.loss_summary.not_estimable_cases, 1);
        assert_eq!(report.loss_summary.attempted_cases, 0);
        assert_eq!(report.loss_summary.mean_loss, None);
        assert!(!report.completeness.evidence_compatible);
        assert!(matches!(
            report.cases[0].status,
            CaseExecutionStatus::NotEstimable { .. }
        ));
    }
}

#[test]
fn zero_batch_deadline_is_not_unlimited() {
    let config = BatchConfig {
        max_runtime_ms: 0,
        ..Default::default()
    };
    let err = execute_evaluation_batch(Cursor::new(b""), &config, &EntryClock::capture().unwrap())
        .unwrap_err();
    assert!(err.to_string().contains("batch_runtime must be positive"));
}

#[test]
fn replayed_decisions_are_not_their_own_correctness_labels() {
    let report = run(replay_case(), &BatchConfig::default());
    assert_eq!(report.loss_summary.attempted_cases, 0);
    assert_eq!(report.loss_summary.mean_loss, None);
    assert_eq!(report.loss_summary.mean_normalized_loss, None);
    assert_eq!(report.gate_status, GateStatus::NotEstablished);
}

#[test]
fn synthetic_provenance_cannot_be_overridden_by_default_recorded_config() {
    let mut case = replay_case();
    case["manifest"]["evidence_origin"] = json!("synthetic");
    let report = run(case, &BatchConfig::default());
    assert_eq!(report.evidence_origin, "synthetic");
    assert_eq!(report.gate_status, GateStatus::NotApplicable);
    assert_eq!(report.run_status, RunStatus::Complete);
}

#[test]
fn duplicate_valid_replay_case_ids_are_rejected() {
    let line = replay_case().to_string();
    let err = execute_evaluation_batch(
        Cursor::new(format!("{line}\n{line}\n")),
        &BatchConfig::default(),
        &EntryClock::capture().unwrap(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("duplicate replay case ID"));
}

#[test]
fn low_gate_case_needs_only_the_wide_stage() {
    let mut case = replay_case();
    case["recorded_responses"]["wide"]["gate_score"] = json!(0.1);
    case["recorded_responses"]
        .as_object_mut()
        .unwrap()
        .remove("rerank");
    let report = run(case, &BatchConfig::default());
    assert_eq!(report.run_status, RunStatus::Complete);
    assert_eq!(report.completeness.stages_required, 1);
    assert_eq!(report.completeness.stages_completed, 1);
    assert!(
        matches!(&report.cases[0].status, CaseExecutionStatus::Completed { decision, .. } if decision == "abstain")
    );
}

#[test]
fn offline_mode_cannot_claim_live_origin() {
    let config = BatchConfig {
        evidence_origin: EvidenceOrigin::Live,
        ..Default::default()
    };
    assert!(
        execute_evaluation_batch(Cursor::new(b""), &config, &EntryClock::capture().unwrap())
            .is_err()
    );
}

fn live_case(id: &str) -> skillranker::evaluation::batch::LiveEvaluationCase {
    serde_json::from_value(json!({
        "schema_version": 1,
        "key": {"frame_id": "f", "family_id": format!("fam-{id}"), "case_id": id,
                "replicate": 0, "policy_id": "p"},
        "split": "holdout",
        "context": {"request": id}
    }))
    .unwrap()
}

fn live_labels(ids: &[&str]) -> Cursor<Vec<u8>> {
    let body: String = ids
        .iter()
        .map(|id| {
            json!({"schema_version": 1, "case_id": id, "revision": 1,
                   "acceptable_skills": ["s_alpha"], "adjudicator": "judge",
                   "created_at_unix_ms": 1u64})
            .to_string()
                + "\n"
        })
        .collect();
    Cursor::new(body.into_bytes())
}

#[test]
fn the_runtime_cap_stops_scheduling_halfway_and_reports_unfinished_cases() {
    use skillranker::evaluation::batch::{
        LiveBatchLimits, LiveRankOutcome, execute_live_frame_evaluation,
    };
    let ids = ["a", "b", "c"];
    let roster = std::collections::BTreeSet::from(["s_alpha".to_owned()]);
    let mut ranked = Vec::new();
    let report = execute_live_frame_evaluation(
        ids.iter().map(|id| live_case(id)).collect(),
        live_labels(&ids),
        None,
        &roster,
        LiveBatchLimits {
            max_requests: 100,
            max_runtime_ms: 40,
            attempts_per_case: 4,
            fit_threshold: 0.3,
        },
        &EntryClock::capture().unwrap(),
        0,
        |_| Ok(Some(json!({"disclosed_bytes": 10}))),
        |case| {
            ranked.push(case.key.case_id.clone());
            // Each ranking outlasts the whole batch deadline.
            std::thread::sleep(std::time::Duration::from_millis(60));
            LiveRankOutcome {
                decision: "ranked".into(),
                suggested_skills: vec!["s_alpha".into()],
                requests: 2,
                http_attempts: 2,
                ..LiveRankOutcome::default()
            }
        },
    )
    .unwrap();
    // The ranking under way finishes and counts; no later case is started.
    assert_eq!(ranked, ["a"]);
    assert_eq!(report.run_status, RunStatus::Partial);
    assert_eq!(report.accounting.http_attempts, 2);
    assert_eq!(report.loss_summary.attempted_cases, 1);
    assert_eq!(report.loss_summary.unfinished_cases, 2);
    assert_eq!(report.error.as_ref().unwrap().kind, "timeout");
    assert_eq!(report.evidence_origin, "live");
    let statuses: Vec<_> = report.cases.iter().map(|case| &case.status).collect();
    assert!(matches!(
        statuses[0],
        CaseExecutionStatus::Completed { loss: Some(0), .. }
    ));
    assert!(
        statuses[1..]
            .iter()
            .all(|status| matches!(status, CaseExecutionStatus::Unfinished { .. }))
    );
    // Honest counterpart: with room, every case runs and the run completes.
    let report = execute_live_frame_evaluation(
        ids.iter().map(|id| live_case(id)).collect(),
        live_labels(&ids),
        None,
        &roster,
        LiveBatchLimits {
            max_requests: 100,
            max_runtime_ms: 60_000,
            attempts_per_case: 4,
            fit_threshold: 0.3,
        },
        &EntryClock::capture().unwrap(),
        0,
        |_| Ok(None),
        |_| LiveRankOutcome {
            decision: "ranked".into(),
            suggested_skills: vec!["s_alpha".into()],
            http_attempts: 2,
            ..LiveRankOutcome::default()
        },
    )
    .unwrap();
    assert_eq!(report.run_status, RunStatus::Complete);
    assert_eq!(report.accounting.http_attempts, 6);
    assert!(report.error.is_none());
}

#[test]
fn a_refused_disclosure_preview_is_never_sent_and_the_preflight_is_frozen_first() {
    use skillranker::evaluation::batch::{
        LiveBatchLimits, LiveRankOutcome, execute_live_frame_evaluation,
    };
    let ids = ["a", "b", "c"];
    let roster = std::collections::BTreeSet::from(["s_alpha".to_owned()]);
    let run = || {
        let order = std::cell::RefCell::new(Vec::new());
        let report = execute_live_frame_evaluation(
            ids.iter().map(|id| live_case(id)).collect(),
            live_labels(&ids),
            None,
            &roster,
            LiveBatchLimits {
                max_requests: 100,
                max_runtime_ms: 60_000,
                attempts_per_case: 4,
                fit_threshold: 0.3,
            },
            &EntryClock::capture().unwrap(),
            0,
            |case| {
                order
                    .borrow_mut()
                    .push(format!("preview {}", case.key.case_id));
                match case.key.case_id.as_str() {
                    "b" => Err("unsupported-input".to_owned()),
                    _ => Ok(Some(json!({"disclosed_bytes": 100, "total_redactions": 1}))),
                }
            },
            |case| {
                order
                    .borrow_mut()
                    .push(format!("send {}", case.key.case_id));
                LiveRankOutcome {
                    decision: "ranked".into(),
                    suggested_skills: vec!["s_alpha".into()],
                    http_attempts: 2,
                    ..LiveRankOutcome::default()
                }
            },
        )
        .unwrap();
        (report, order.into_inner())
    };
    let (report, order) = run();
    // Every preview precedes every send, and the refused case is never sent.
    assert_eq!(
        order,
        ["preview a", "preview b", "preview c", "send a", "send c"]
    );
    let frozen = report.disclosure_preflight.as_ref().unwrap();
    assert_eq!(frozen.cases_checked, 3);
    assert_eq!(frozen.cases_refused, 1);
    assert_eq!(frozen.disclosed_bytes, 200);
    assert_eq!(frozen.total_redactions, 2);
    assert_eq!(frozen.receipts_digest.len(), 64);
    let refused = report.cases.iter().find(|c| c.case_id == "b").unwrap();
    assert!(matches!(
        &refused.status,
        CaseExecutionStatus::NotEstimable { reason } if reason.contains("unsupported-input")
    ));
    assert_eq!(report.run_status, RunStatus::Partial);
    assert_eq!(report.accounting.http_attempts, 4);
    // The same inputs freeze the same digest.
    assert_eq!(
        run().0.disclosure_preflight.unwrap().receipts_digest,
        frozen.receipts_digest
    );
}

#[test]
fn baselines_score_every_policy_on_the_same_judged_cohort_from_one_runs_answers() {
    use skillranker::evaluation::batch::{
        LiveBatchLimits, LiveRankOutcome, execute_live_frame_evaluation,
    };
    use skillranker::pipeline::{RerankEvidence, StageEvidence, WideEvidence};
    // Two positive cases (Y = {s_alpha}) and one no-match case (Y empty).
    let cases = ["pos-1", "pos-2", "none"];
    let labels: String = cases
        .iter()
        .map(|id| {
            let acceptable: Vec<&str> = if *id == "none" {
                vec![]
            } else {
                vec!["s_alpha"]
            };
            json!({"schema_version": 1, "case_id": id, "revision": 1,
                   "acceptable_skills": acceptable, "no_skill_needed": acceptable.is_empty(),
                   "adjudicator": "judge", "created_at_unix_ms": 1u64})
            .to_string()
                + "\n"
        })
        .collect();
    let roster = std::collections::BTreeSet::from(["s_alpha".to_owned(), "s_beta".to_owned()]);
    let stages = |shortlist: &[(&str, f64)], fits: &[(&str, f64)]| StageEvidence {
        admitted: vec!["s_alpha".into(), "s_beta".into()],
        quill_ranked: false,
        lexical: None,
        wide: Some(WideEvidence {
            needs_skill: 0.9,
            none_probability: 0.1,
            low_need: shortlist.is_empty(),
            shortlist: shortlist.iter().map(|(id, p)| ((*id).into(), *p)).collect(),
            // Whatever the gate said, both skills are the top candidates.
            intrinsic_shortlist: vec!["s_beta".into(), "s_alpha".into()],
        }),
        rerank: (!fits.is_empty()).then(|| RerankEvidence {
            none_probability: 0.1,
            candidates: fits
                .iter()
                .map(|(id, fit)| ((*id).into(), 0.5, *fit))
                .collect(),
        }),
    };
    let report = execute_live_frame_evaluation(
        cases.iter().map(|id| live_case(id)).collect(),
        Cursor::new(labels.into_bytes()),
        None,
        &roster,
        LiveBatchLimits {
            max_requests: 100,
            max_runtime_ms: 60_000,
            attempts_per_case: 4,
            fit_threshold: 0.3,
        },
        &EntryClock::capture().unwrap(),
        0,
        |_| Ok(None),
        |case| {
            // pos-1: wide prefers beta, fit prefers alpha, production emits alpha.
            // pos-2: the gate stops the run; everyone abstains.
            // none:  wide prefers beta, fit prefers beta, production abstains.
            let (mut evidence, suggested) = match case.key.case_id.as_str() {
                "pos-1" => (
                    stages(
                        &[("s_beta", 0.6), ("s_alpha", 0.3)],
                        &[("s_alpha", 0.9), ("s_beta", 0.2)],
                    ),
                    vec!["s_alpha".to_owned()],
                ),
                "pos-2" => (stages(&[], &[]), vec![]),
                _ => (stages(&[("s_beta", 0.7)], &[("s_beta", 0.8)]), vec![]),
            };
            // Quill ranks alpha first for pos-1, finds nothing for the no-match
            // case, and could not run for pos-2.
            evidence.lexical = match case.key.case_id.as_str() {
                "pos-1" => Some(vec!["s_alpha".to_owned(), "s_beta".to_owned()]),
                "pos-2" => None,
                _ => Some(Vec::new()),
            };
            LiveRankOutcome {
                decision: if suggested.is_empty() {
                    "abstain"
                } else {
                    "ranked"
                }
                .into(),
                suggested_skills: suggested,
                http_attempts: 2,
                elapsed_ms: match case.key.case_id.as_str() {
                    "pos-1" => 100,
                    "pos-2" => 200,
                    _ => 300,
                },
                evidence: Some(evidence),
                ..LiveRankOutcome::default()
            }
        },
    )
    .unwrap();
    let baselines = report.baselines.unwrap();
    assert_eq!(baselines.cases_without_evidence, 0);
    // Both positive cases admitted alpha; only pos-1 passed the gate, and its
    // shortlist holds alpha.
    assert_eq!(
        (
            baselines.coverage.admitted.successes,
            baselines.coverage.admitted.denominator
        ),
        (2, 2)
    );
    assert_eq!(
        (
            baselines.coverage.shortlist.successes,
            baselines.coverage.shortlist.denominator
        ),
        (1, 1)
    );
    assert_eq!(baselines.coverage.shortlist_gated_out, 1);
    // Irrespective of the gate, both positive cases kept alpha in contention.
    let intrinsic = &baselines.coverage.intrinsic_shortlist;
    assert_eq!((intrinsic.successes, intrinsic.denominator), (2, 2));
    let policy = |name: &str| {
        baselines
            .policies
            .iter()
            .find(|p| p.policy == name)
            .unwrap()
            .clone()
    };
    // choice-only: pos-1 -> beta (wrong, 2), pos-2 abstain (1), none -> beta (needless, 2).
    let choice = policy("choice-only");
    assert_eq!(choice.evaluated_cases, 3);
    assert_eq!(
        (
            choice.top1_precision.successes,
            choice.top1_precision.denominator
        ),
        (0, 2)
    );
    assert_eq!(choice.needless_suggestion_rate.successes, 1);
    assert_eq!(choice.false_abstention_rate.successes, 1);
    assert_eq!(choice.mean_loss, Some(5.0 / 3.0));
    // fit-only: pos-1 -> alpha (0), pos-2 abstain (1), none -> beta (2).
    let fit = policy("fit-only");
    assert_eq!(
        (
            fit.positive_suggestion_rate.successes,
            fit.positive_suggestion_rate.denominator
        ),
        (1, 2)
    );
    assert_eq!(fit.mean_loss, Some(1.0));
    // blend: pos-1 -> alpha (0), pos-2 abstain (1), none abstains (0).
    let blend = policy("blend");
    assert_eq!(
        (
            blend.top1_precision.successes,
            blend.top1_precision.denominator
        ),
        (1, 1)
    );
    assert_eq!(blend.needless_suggestion_rate.successes, 0);
    assert_eq!(blend.mean_loss, Some(1.0 / 3.0));
    assert!(blend.top1_precision.wilson_95.is_some());
    assert!(
        !baselines
            .not_computed
            .iter()
            .any(|note| note.starts_with("quill-only"))
    );
    // quill-only: pos-1 -> alpha (0), none -> abstain (0); pos-2 not evaluated.
    let quill = policy("quill-only");
    assert_eq!((quill.evaluated_cases, quill.not_evaluated_cases), (2, 1));
    assert_eq!(
        (
            quill.top1_precision.successes,
            quill.top1_precision.denominator
        ),
        (1, 1)
    );
    assert_eq!(quill.needless_suggestion_rate.successes, 0);
    assert_eq!(quill.mean_loss, Some(0.0));
    assert_eq!(policy("blend").not_evaluated_cases, 0);
    // cookbook-approx: pos-1's top three hold alpha (fit 0.9) and beta; their
    // rerank probabilities tie, broken by ID to alpha (0). pos-2 is gated (1);
    // none suggests beta with fit 0.8 (needless, 2).
    let cookbook = policy("cookbook-approx");
    assert_eq!(cookbook.evaluated_cases, 3);
    assert_eq!(cookbook.positive_suggestion_rate.successes, 1);
    assert_eq!(cookbook.needless_suggestion_rate.successes, 1);
    assert_eq!(cookbook.mean_loss, Some(1.0));
    // Reranked pairs: pos-1 alpha 0.9 (acceptable), pos-1 beta 0.2 (not), and
    // none beta 0.8 (not). Brier = (0.01 + 0.04 + 0.64) / 3 = 0.23.
    let calibration = baselines.fit_calibration.unwrap();
    assert_eq!(calibration.pairs, 3);
    assert!(
        (calibration.brier - 0.23).abs() < 1e-12,
        "{}",
        calibration.brier
    );
    let counts: Vec<(usize, usize)> = calibration
        .bins
        .iter()
        .map(|bin| (bin.pairs, bin.acceptable.successes))
        .collect();
    assert_eq!(counts, [(0, 0), (1, 0), (0, 0), (0, 0), (2, 1)]);
    // Only the blend publishes a list: pos-1's covers alpha, pos-2's is empty.
    let top_k = policy("blend").top_k_coverage.unwrap();
    assert_eq!((top_k.successes, top_k.denominator), (1, 2));
    assert!(policy("choice-only").top_k_coverage.is_none());
    // Nearest-rank percentiles over every executed ranking.
    let latency = baselines.latency_ms.unwrap();
    assert_eq!(
        (
            latency.cases,
            latency.p50,
            latency.p95,
            latency.p99,
            latency.max
        ),
        (3, 200, 300, 300, 300)
    );
}

#[test]
fn timeouts_cannot_shrink_the_loss_and_always_abstaining_is_not_a_good_score() {
    use skillranker::evaluation::batch::execute_labeled_frame_evaluation;
    let frame = |outcome: &dyn Fn(usize) -> (&'static str, Vec<&'static str>, bool)| {
        let mut records = String::new();
        let mut labels = String::new();
        for i in 0..100 {
            let (decision, suggested, failed) = outcome(i);
            records.push_str(
                &(json!({"schema_version": 1,
                    "key": {"frame_id": "f", "family_id": format!("fam-{i}"),
                            "case_id": format!("c{i}"), "replicate": 0, "policy_id": "p"},
                    "split": "holdout", "prompt_summary": "request", "decision": decision,
                    "suggested_skills": suggested, "relevance_abstention": decision == "abstain",
                    "operational_failure": failed})
                .to_string()
                    + "\n"),
            );
            labels.push_str(
                &(json!({"schema_version": 1, "case_id": format!("c{i}"), "revision": 1,
                        "acceptable_skills": ["s_right"], "adjudicator": "judge",
                        "created_at_unix_ms": 1u64})
                .to_string()
                    + "\n"),
            );
        }
        execute_labeled_frame_evaluation(
            Cursor::new(records.into_bytes()),
            Cursor::new(labels.into_bytes()),
            None,
            0,
        )
        .unwrap()
    };
    // 90 correct and 10 incorrect: mean loss (10 x 2) / 100 = 0.2.
    let wrong = frame(&|i| {
        if i < 90 {
            ("ranked", vec!["s_right"], false)
        } else {
            ("ranked", vec!["s_wrong"], false)
        }
    });
    assert_eq!(wrong.loss_summary.mean_loss, Some(0.2));
    // The same ten as timeouts keep their loss and stay in the denominator.
    let timed_out = frame(&|i| {
        if i < 90 {
            ("ranked", vec!["s_right"], false)
        } else {
            ("unavailable", vec![], true)
        }
    });
    assert_eq!(timed_out.loss_summary.mean_loss, Some(0.2));
    assert_eq!(timed_out.loss_summary.attempted_cases, 100);
    assert_eq!(timed_out.loss_summary.operational_failures, 10);
    // Always abstaining on positive cases is charged for every miss.
    let silent = frame(&|_| ("abstain", vec![], false));
    assert_eq!(silent.loss_summary.mean_loss, Some(1.0));
    assert!(silent.loss_summary.mean_loss > wrong.loss_summary.mean_loss);
}

#[test]
fn context_ablation_ranks_history_cases_twice_within_the_caps() {
    use skillranker::evaluation::batch::{
        LiveBatchLimits, LiveRankOutcome, execute_live_frame_evaluation,
    };
    // "hist-a" and "hist-b" carry history; "single" does not.
    let case = |id: &str, history: bool| -> skillranker::evaluation::batch::LiveEvaluationCase {
        let events = if history {
            json!([{"text": "earlier turn"}])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "schema_version": 1,
            "key": {"frame_id": "f", "family_id": format!("fam-{id}"), "case_id": id,
                    "replicate": 0, "policy_id": "p"},
            "split": "holdout",
            "context": {"events": events}
        }))
        .unwrap()
    };
    let ids = ["hist-a", "hist-b", "single"];
    let run = |max_requests: usize| {
        let calls = std::cell::RefCell::new(Vec::new());
        let report = execute_live_frame_evaluation(
            vec![
                case("hist-a", true),
                case("hist-b", true),
                case("single", false),
            ],
            live_labels(&ids),
            None,
            &std::collections::BTreeSet::from(["s_alpha".to_owned()]),
            LiveBatchLimits {
                max_requests,
                max_runtime_ms: 60_000,
                attempts_per_case: 4,
                fit_threshold: 0.3,
            },
            &EntryClock::capture().unwrap(),
            0,
            |_| Ok(None),
            |case| {
                let history = !case.context["events"].as_array().unwrap().is_empty();
                calls.borrow_mut().push(format!(
                    "{} {}",
                    case.key.case_id,
                    if history { "full" } else { "latest" }
                ));
                // With history the selector finds alpha; without it, it abstains.
                LiveRankOutcome {
                    decision: if history { "ranked" } else { "abstain" }.into(),
                    suggested_skills: if history {
                        vec!["s_alpha".into()]
                    } else {
                        vec![]
                    },
                    http_attempts: 2,
                    ..LiveRankOutcome::default()
                }
            },
        )
        .unwrap();
        (report, calls.into_inner())
    };
    let (report, calls) = run(100);
    assert_eq!(
        calls,
        [
            "hist-a full",
            "hist-b full",
            "single latest",
            "hist-a latest",
            "hist-b latest"
        ]
    );
    // Both arms count toward the batch's attempts.
    assert_eq!(report.accounting.http_attempts, 10);
    let ablation = report
        .baselines
        .as_ref()
        .unwrap()
        .context_ablation
        .clone()
        .unwrap();
    assert_eq!(ablation.cases, 2);
    assert_eq!(
        ablation.recent_context.positive_suggestion_rate.successes,
        2
    );
    assert_eq!(
        ablation.latest_request_only.false_abstention_rate.successes,
        2
    );
    assert_eq!(ablation.recent_context.mean_loss, Some(0.0));
    assert_eq!(ablation.latest_request_only.mean_loss, Some(1.0));
    assert!(
        !report
            .baselines
            .unwrap()
            .not_computed
            .iter()
            .any(|note| note.starts_with("context"))
    );
    // Main rankings come first; the second arms share what budget remains.
    let (report, calls) = run(11);
    assert_eq!(
        calls,
        [
            "hist-a full",
            "hist-b full",
            "single latest",
            "hist-a latest"
        ]
    );
    // Every main ranking ran; only a second arm was skipped.
    assert_eq!(report.run_status, skillranker::output::RunStatus::Complete);
    let ablation = report.baselines.unwrap().context_ablation.unwrap();
    assert_eq!((ablation.cases, ablation.skipped_for_budget), (1, 1));
}
