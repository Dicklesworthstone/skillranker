//! Independent regression checks for sr-mxn2: executing a batch is not proof
//! of relevance, and an input oracle is not an observed ranking decision.

use serde_json::json;
use skillranker::evaluation::batch::{BatchConfig, CaseExecutionStatus, execute_evaluation_batch};
use skillranker::output::GateStatus;
use skillranker::runtime::EntryClock;
use std::io::Cursor;

#[test]
fn a_synthetic_oracle_cannot_be_laundered_into_recorded_quality() {
    let clock = EntryClock::capture().unwrap();
    let input = json!({
        "schema_version": 1,
        "case_id": "oracle-only",
        "case_kind": "positive_advisory",
        "oracle": {"expected_correct_top_one": "unexecuted-skill"}
    });
    let result = execute_evaluation_batch(
        Cursor::new(serde_json::to_vec(&input).unwrap()),
        &BatchConfig::default(),
        &clock,
    );
    // Rejecting this non-replay schema is valid. If retained diagnostically,
    // it must not acquire observations or independent relevance labels.
    if let Ok(report) = result {
        assert_ne!(report.gate_status, GateStatus::Passed);
        assert_eq!(report.loss_summary.mean_loss, None);
        assert_eq!(report.accounting.http_attempts, 0);
    }
}

#[test]
fn an_unjudged_recorded_suggestion_has_no_correctness_loss() {
    let clock = EntryClock::capture().unwrap();
    let input = json!({
        "schema_version": 1,
        "key": {
            "frame_id": "frame", "family_id": "family", "case_id": "case",
            "replicate": 0, "policy_id": "policy"
        },
        "split": "holdout",
        "roster_skills": ["candidate"],
        "decision": "ranked",
        "suggested_skills": ["candidate"]
    });
    serde_json::from_value::<skillranker::evaluation::EvaluationCaseRecord>(input.clone())
        .expect("negative case must be a valid unjudged evaluation record");
    let result = execute_evaluation_batch(
        Cursor::new(serde_json::to_vec(&input).unwrap()),
        &BatchConfig::default(),
        &clock,
    );
    if let Ok(report) = result {
        assert_ne!(report.gate_status, GateStatus::Passed);
        assert_eq!(report.loss_summary.mean_loss, None);
        for case in report.cases {
            if let CaseExecutionStatus::Completed {
                loss,
                normalized_loss,
                ..
            } = case.status
            {
                assert_eq!(loss, None, "a suggestion is not its own correctness label");
                assert_eq!(normalized_loss, None);
            }
        }
    }
}

#[test]
fn an_unknown_object_is_not_a_successful_abstention() {
    let clock = EntryClock::capture().unwrap();
    assert!(
        execute_evaluation_batch(Cursor::new(b"{}\n"), &BatchConfig::default(), &clock,).is_err()
    );
}

#[test]
fn duplicate_case_definitions_do_not_enlarge_the_cohort() {
    let clock = EntryClock::capture().unwrap();
    let line = r#"{"schema_version":1,"case_id":"same","case_kind":"positive_advisory","oracle":{"expected_correct_top_one":"candidate"}}"#;
    assert!(
        execute_evaluation_batch(
            Cursor::new(format!("{line}\n{line}\n")),
            &BatchConfig::default(),
            &clock,
        )
        .is_err()
    );
}
