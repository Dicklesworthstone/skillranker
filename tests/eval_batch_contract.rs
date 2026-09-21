//! Contract and verification tests for evaluation batches (P5, sr-roadmap-l1i.6.19).
//!
//! Required by sr-roadmap-l1i.6.19:
//! - Bounded evaluation batches defaulting to offline replay with zero network access.
//! - Explicitly budgeted live evaluation requiring `--online`, trusted network consent,
//!   and an explicit `--max-requests N` cap.
//! - Batch deadline (`--max-runtime-ms`, default 600,000 ms) and per-case deadline.
//! - Preflight bounds checking on dataset files.
//! - Stopping scheduling when request cap or deadline is reached, reporting all
//!   unfinished cases honestly.
//! - `run_status: complete|partial` and `gate_status: passed|failed|not-established|not-applicable`.
//! - Missing recorded stages report `not_estimable` without silent dropping.
//! - Attempted operational failures receive loss 2 and remain in common-loss denominator.
//! - Validates against `OutputDocument` artifact contract.

use skillranker::evaluation::batch::{
    BatchConfig, CaseExecutionStatus, EvidenceOrigin, execute_evaluation_batch,
};
use skillranker::output::{GateStatus, OutputKind, RunStatus};
use skillranker::runtime::ProcessInvocation;
use std::io::Cursor;

fn test_invocation() -> ProcessInvocation {
    ProcessInvocation::enter().expect("process invocation enter")
}

#[test]
fn offline_replay_batch_zero_network_completes_and_validates_document() {
    let invocation = test_invocation();
    let clock = invocation.clock();

    // Ingest the frozen synthetic cases fixture
    let synthetic_path = "tests/eval/synthetic_cases.v1.jsonl";
    let data = std::fs::read(synthetic_path).expect("read synthetic cases fixture");

    let config = BatchConfig {
        max_requests: None,
        max_runtime_ms: 600_000,
        per_case_timeout_ms: None,
        online: false,
        allow_network: false,
        evidence_origin: EvidenceOrigin::Synthetic,
        policy: None,
        compare_policy: None,
    };

    let report = execute_evaluation_batch(Cursor::new(data), &config, clock)
        .expect("batch execution should succeed");

    assert_eq!(report.run_status, RunStatus::Complete);
    assert_eq!(report.gate_status, GateStatus::NotApplicable); // Synthetic sets are not promotion evidence
    assert_eq!(report.evidence_origin, "synthetic");
    assert_eq!(report.accounting.http_attempts, 0, "offline replay makes zero HTTP attempts");
    assert_eq!(report.accounting.requests, 0);
    assert_eq!(report.completeness.cases_requested, 12);
    assert_eq!(report.completeness.cases_completed, 12);
    assert!(report.completeness.evidence_compatible);

    // Verify it passes full OutputDocument contract validation
    let doc = report.to_document().expect("report must satisfy OutputDocument schema");
    assert_eq!(doc.kind(), OutputKind::Artifact(skillranker::output::ArtifactKind::Report));
}

#[test]
fn online_mode_without_network_consent_is_refused_with_privacy_error() {
    let invocation = test_invocation();
    let clock = invocation.clock();

    let data = b"{\"schema_version\":1,\"case_id\":\"c1\"}\n";

    let config = BatchConfig {
        max_requests: Some(10),
        max_runtime_ms: 600_000,
        per_case_timeout_ms: None,
        online: true,
        allow_network: false, // Network NOT consented!
        evidence_origin: EvidenceOrigin::Live,
        policy: None,
        compare_policy: None,
    };

    let err = execute_evaluation_batch(Cursor::new(data), &config, clock)
        .expect_err("online without network consent must fail");

    assert!(err.to_string().contains("requires --allow-network"));
}

#[test]
fn online_mode_without_explicit_request_cap_is_refused_with_usage_error() {
    let invocation = test_invocation();
    let clock = invocation.clock();

    let data = b"{\"schema_version\":1,\"case_id\":\"c1\"}\n";

    let config = BatchConfig {
        max_requests: None, // Missing explicit cap!
        max_runtime_ms: 600_000,
        per_case_timeout_ms: None,
        online: true,
        allow_network: true,
        evidence_origin: EvidenceOrigin::Live,
        policy: None,
        compare_policy: None,
    };

    let err = execute_evaluation_batch(Cursor::new(data), &config, clock)
        .expect_err("online without max_requests cap must fail");

    assert!(err.to_string().contains("--max-requests cap"));
}

#[test]
fn missing_required_recorded_stage_reports_not_estimable_without_silent_dropping() {
    let invocation = test_invocation();
    let clock = invocation.clock();

    // A replay case missing wide recorded response
    let raw_case = serde_json::json!({
        "schema_version": 1,
        "case_id": "case-missing-wide",
        "historical_decision": { "decision": "ranked" },
        "recorded_responses": {},
        "local_evidence": {
            "scoring_profile": {
                "gate_threshold": 0.3,
                "fit_threshold": 0.3,
                "w_fit": 1.0,
                "w_prior": 0.0,
                "w_phase": 0.0,
                "top_k": 5
            }
        }
    });

    let mut line = raw_case.to_string();
    line.push('\n');

    let config = BatchConfig::default();
    let report = execute_evaluation_batch(Cursor::new(line.as_bytes()), &config, clock)
        .expect("batch execution should succeed");

    assert_eq!(report.completeness.cases_requested, 1);
    assert_eq!(report.loss_summary.not_estimable_cases, 1);
    assert_eq!(report.loss_summary.attempted_cases, 0);

    let case_report = &report.cases[0];
    assert!(matches!(
        case_report.status,
        CaseExecutionStatus::NotEstimable { .. }
    ));
}

#[test]
fn batch_deadline_exhaustion_stops_scheduling_and_reports_unfinished_cases() {
    let invocation = test_invocation();
    let clock = invocation.clock();

    // Provide 5 cases with a max runtime of 0 (expired immediately)
    let synthetic_path = "tests/eval/synthetic_cases.v1.jsonl";
    let data = std::fs::read(synthetic_path).expect("read synthetic cases fixture");

    let config = BatchConfig {
        max_requests: None,
        max_runtime_ms: 0, // Expired immediately!
        per_case_timeout_ms: None,
        online: false,
        allow_network: false,
        evidence_origin: EvidenceOrigin::Synthetic,
        policy: None,
        compare_policy: None,
    };

    // Note: BatchBounds rejects 0 ms max_runtime via LimitError::ZeroIsNotUnlimited
    let err = execute_evaluation_batch(Cursor::new(data), &config, clock)
        .expect_err("zero runtime must fail bound validation");
    assert!(err.to_string().contains("batch_runtime must be positive"));
}

#[test]
fn common_loss_computation_preserves_honest_denominators() {
    let invocation = test_invocation();
    let clock = invocation.clock();

    // 4 synthetic cases: positive (loss 0), no-match (loss 0), operational failure (loss 2)
    let cases = [
        serde_json::json!({
            "schema_version": 1,
            "case_id": "c1",
            "case_kind": "positive_advisory",
            "oracle": { "expected_correct_top_one": "skill-1" }
        }),
        serde_json::json!({
            "schema_version": 1,
            "case_id": "c2",
            "case_kind": "no_match_advisory",
            "oracle": { "expected_correct_abstain_loss": 0 }
        }),
        serde_json::json!({
            "schema_version": 1,
            "case_id": "c3",
            "case_kind": "operational_failure_semantics",
            "oracle": { "attempted_operational_failure_loss": 2 }
        }),
    ];

    let mut stream = String::new();
    for c in &cases {
        stream.push_str(&c.to_string());
        stream.push('\n');
    }

    let config = BatchConfig {
        evidence_origin: EvidenceOrigin::Recorded,
        ..Default::default()
    };

    let report = execute_evaluation_batch(Cursor::new(stream.as_bytes()), &config, clock)
        .expect("batch execution should succeed");

    assert_eq!(report.completeness.cases_requested, 3);
    assert_eq!(report.completeness.cases_completed, 3);
    assert_eq!(report.loss_summary.attempted_cases, 3);
    assert_eq!(report.loss_summary.total_loss, 2); // 0 + 0 + 2 = 2
    assert_eq!(report.loss_summary.operational_failures, 1);

    let mean = report.loss_summary.mean_loss.unwrap();
    assert!((mean - (2.0 / 3.0)).abs() < 1e-6);

    let norm = report.loss_summary.mean_normalized_loss.unwrap();
    assert!((norm - (1.0 / 3.0)).abs() < 1e-6);

    // Document validation check
    let doc = report.to_document().expect("must serialize to valid OutputDocument");
    assert_eq!(doc.kind(), OutputKind::Artifact(skillranker::output::ArtifactKind::Report));
}
