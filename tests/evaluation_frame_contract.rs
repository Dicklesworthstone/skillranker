//! Integration tests for consented evaluation frames, streaming import,
//! label revision resolution, split isolation, and reconciliation (sr-roadmap-l1i.6.17).

use skillranker::evaluation::{
    CaseKey, EvaluationCaseRecord, EvaluationError, EvaluationSplit, JudgedLabel, LabelStatus,
    RelevanceClass, compute_metrics, join_evaluation_frame, parse_case_records_streaming,
    parse_labels_streaming, resolve_label_revisions, verify_split_isolation,
};
use skillranker::output::SCHEMA_VERSION;
use std::collections::BTreeMap;
use std::io::Cursor;

fn sample_case(
    frame: &str,
    family: &str,
    case: &str,
    replicate: u32,
    split: EvaluationSplit,
) -> EvaluationCaseRecord {
    EvaluationCaseRecord {
        schema_version: SCHEMA_VERSION,
        key: CaseKey::new(frame, family, case, replicate, "default"),
        split,
        prompt_summary: Some("Debug test failures".into()),
        roster_skills: vec![
            "rust-tester".into(),
            "rust-test-triage".into(),
            "agent-mail".into(),
        ],
        decision: "ranked".into(),
        suggested_skills: vec!["rust-test-triage".into(), "rust-tester".into()],
        fits: BTreeMap::from([
            ("rust-test-triage".into(), 0.92),
            ("rust-tester".into(), 0.85),
        ]),
        gate_score: Some(0.88),
        relevance_abstention: false,
        operational_failure: false,
    }
}

fn sample_label(case: &str, revision: u64, acceptable: &[&str]) -> JudgedLabel {
    JudgedLabel {
        schema_version: SCHEMA_VERSION,
        case_id: case.into(),
        revision,
        acceptable_skills: acceptable.iter().map(|s| s.to_string()).collect(),
        explicit_directive: None,
        no_skill_needed: acceptable.is_empty(),
        constraints: vec![],
        adjudicator: "expert-1".into(),
        created_at_unix_ms: 1726700000000,
        notes: None,
    }
}

#[test]
fn streaming_case_records_parses_valid_jsonl() {
    let case1 = sample_case("frame-1", "fam-1", "case-1", 0, EvaluationSplit::Train);
    let case2 = sample_case("frame-1", "fam-2", "case-2", 0, EvaluationSplit::Validation);

    let jsonl = format!(
        "{}\n{}\n",
        serde_json::to_string(&case1).unwrap(),
        serde_json::to_string(&case2).unwrap()
    );

    let records = parse_case_records_streaming(Cursor::new(jsonl.as_bytes())).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key.case_id, "case-1");
    assert_eq!(records[1].key.case_id, "case-2");
}

#[test]
fn streaming_case_records_rejects_duplicate_json_keys() {
    let case = sample_case("frame-1", "fam-1", "case-1", 0, EvaluationSplit::Train);
    let mut json_str = serde_json::to_string(&case).unwrap();
    json_str = json_str.replacen(
        "\"schema_version\":1,",
        "\"schema_version\":1,\"schema_version\":1,",
        1,
    );

    let err = parse_case_records_streaming(Cursor::new(json_str.as_bytes())).unwrap_err();
    assert!(matches!(err, EvaluationError::DuplicateKey(_)));
}

#[test]
fn streaming_case_records_rejects_deep_nesting() {
    let deep_json = format!(
        "{{\"schema_version\":1,\"key\":{{\"frame_id\":\"f\",\"family_id\":\"fam\",\"case_id\":\"c\",\"replicate\":0,\"policy_id\":\"p\"}},\"split\":\"train\",\"decision\":\"ranked\",\"data\":{}}}",
        "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]"
    );

    let err = parse_case_records_streaming(Cursor::new(deep_json.as_bytes())).unwrap_err();
    assert!(matches!(err, EvaluationError::ExcessiveDepth));
}

#[test]
fn streaming_labels_parses_valid_jsonl() {
    let l1 = sample_label("case-1", 1, &["rust-test-triage"]);
    let l2 = sample_label("case-2", 1, &["agent-mail"]);

    let jsonl = format!(
        "{}\n{}\n",
        serde_json::to_string(&l1).unwrap(),
        serde_json::to_string(&l2).unwrap()
    );

    let labels = parse_labels_streaming(Cursor::new(jsonl.as_bytes())).unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0].case_id, "case-1");
    assert_eq!(labels[1].case_id, "case-2");
}

#[test]
fn resolve_label_revisions_selects_highest_revision_monotonically() {
    let l_v1 = sample_label("case-1", 1, &["rust-tester"]);
    let l_v2 = sample_label("case-1", 2, &["rust-test-triage", "rust-tester"]);
    let l_v3 = sample_label("case-1", 3, &["rust-test-triage"]);

    // Shuffled input order
    let labels = vec![l_v2, l_v1, l_v3.clone()];
    let resolved = resolve_label_revisions(&labels).unwrap();

    assert_eq!(resolved.len(), 1);
    let active = resolved.get("case-1").unwrap();
    assert_eq!(active.revision, 3);
    assert_eq!(active.acceptable_skills, l_v3.acceptable_skills);
}

#[test]
fn resolve_label_revisions_rejects_conflicting_duplicate_revisions() {
    let l_v1_a = sample_label("case-1", 1, &["rust-tester"]);
    let mut l_v1_b = sample_label("case-1", 1, &["agent-mail"]);
    l_v1_b.adjudicator = "expert-2".into();

    let labels = vec![l_v1_a, l_v1_b];
    let err = resolve_label_revisions(&labels).unwrap_err();
    assert!(matches!(err, EvaluationError::CardinalityViolation(_)));
}

#[test]
fn split_isolation_prevents_task_family_contamination() {
    let c1 = sample_case(
        "frame-1",
        "family-alpha",
        "case-1",
        0,
        EvaluationSplit::Train,
    );
    let c2 = sample_case(
        "frame-1",
        "family-alpha",
        "case-2",
        0,
        EvaluationSplit::Holdout,
    );

    let err = verify_split_isolation(&[c1, c2]).unwrap_err();
    assert!(matches!(err, EvaluationError::SplitContamination(_)));
    assert!(err.to_string().contains("family-alpha"));
}

#[test]
fn join_frame_reconciles_pre_and_post_counts_exactly() {
    let c1 = sample_case("frame-1", "fam-1", "case-1", 0, EvaluationSplit::Train);
    let c1_rep1 = sample_case("frame-1", "fam-1", "case-1", 1, EvaluationSplit::Train);
    let c2 = sample_case("frame-1", "fam-2", "case-2", 0, EvaluationSplit::Train);

    let l1 = sample_label("case-1", 1, &["rust-test-triage"]);
    let l2 = sample_label("case-2", 1, &["rust-tester"]);

    let cases = vec![c1, c1_rep1, c2];
    let labels = vec![l1, l2];

    let (resolved, manifest) = join_evaluation_frame(&cases, &labels).unwrap();

    // Verify reconciliation
    assert_eq!(manifest.pre_join_case_count, 3);
    assert_eq!(manifest.post_join_case_count, 3);
    assert!(manifest.reconciled);
    assert_eq!(manifest.unmatched_cases_count, 0);
    assert_eq!(manifest.null_key_cases_count, 0);
    assert_eq!(manifest.resolved_labels_count, 2);
    assert_eq!(resolved.len(), 3);

    // Many-to-one join: both replicates of case-1 join with l1
    assert!(matches!(
        resolved[0].label_status,
        LabelStatus::Resolved { .. }
    ));
    assert!(matches!(
        resolved[1].label_status,
        LabelStatus::Resolved { .. }
    ));
    assert_eq!(resolved[0].relevance_class, RelevanceClass::TruePositive);
    assert_eq!(resolved[1].relevance_class, RelevanceClass::TruePositive);
}

#[test]
fn join_frame_preserves_unmatched_and_null_keys_without_dropping() {
    let c_valid = sample_case("frame-1", "fam-1", "case-1", 0, EvaluationSplit::Train);
    let c_unmatched = sample_case("frame-1", "fam-2", "case-2", 0, EvaluationSplit::Train);
    let mut c_null = sample_case("frame-1", "fam-3", "case-3", 0, EvaluationSplit::Train);
    c_null.key.case_id = "".into(); // null key

    let l1 = sample_label("case-1", 1, &["rust-test-triage"]);

    let cases = vec![c_valid, c_unmatched, c_null];
    let labels = vec![l1];

    let (resolved, manifest) = join_evaluation_frame(&cases, &labels).unwrap();

    assert_eq!(manifest.pre_join_case_count, 3);
    assert_eq!(manifest.post_join_case_count, 3);
    assert!(manifest.reconciled);
    assert_eq!(manifest.unmatched_cases_count, 1);
    assert_eq!(manifest.null_key_cases_count, 1);

    // Verify status preservation
    assert!(matches!(
        resolved[0].label_status,
        LabelStatus::Resolved { .. }
    ));
    assert!(matches!(resolved[1].label_status, LabelStatus::Unmatched));
    assert!(matches!(resolved[2].label_status, LabelStatus::NullKey));

    assert_eq!(resolved[1].relevance_class, RelevanceClass::Unjudged);
    assert_eq!(resolved[2].relevance_class, RelevanceClass::Unjudged);
}

#[test]
fn evaluation_metrics_computes_honest_rates_and_denominators() {
    // 1. True positive
    let mut c1 = sample_case("frame-1", "fam-1", "c1", 0, EvaluationSplit::Train);
    c1.suggested_skills = vec!["rust-test-triage".into()];
    let l1 = sample_label("c1", 1, &["rust-test-triage"]);

    // 2. False positive
    let mut c2 = sample_case("frame-1", "fam-2", "c2", 0, EvaluationSplit::Train);
    c2.suggested_skills = vec!["agent-mail".into(), "rust-test-triage".into()];
    let l2 = sample_label("c2", 1, &["rust-test-triage"]);

    // 3. True abstain (no skill needed, correctly abstained)
    let mut c3 = sample_case("frame-1", "fam-3", "c3", 0, EvaluationSplit::Train);
    c3.decision = "abstain".into();
    c3.suggested_skills = vec![];
    c3.relevance_abstention = true;
    let l3 = sample_label("c3", 1, &[]);

    // 4. Needless suggestion (no skill needed, but emitted suggestion)
    let mut c4 = sample_case("frame-1", "fam-4", "c4", 0, EvaluationSplit::Train);
    c4.suggested_skills = vec!["rust-test-triage".into()];
    let l4 = sample_label("c4", 1, &[]);

    // 5. False abstain (skill was needed, but abstained)
    let mut c5 = sample_case("frame-1", "fam-5", "c5", 0, EvaluationSplit::Train);
    c5.decision = "abstain".into();
    c5.suggested_skills = vec![];
    c5.relevance_abstention = true;
    let l5 = sample_label("c5", 1, &["rust-tester"]);

    // 6. Operational failure (network/timeout)
    let mut c6 = sample_case("frame-1", "fam-6", "c6", 0, EvaluationSplit::Train);
    c6.decision = "unavailable".into();
    c6.operational_failure = true;
    let l6 = sample_label("c6", 1, &["rust-tester"]);

    // 7. Unmatched / unjudged case
    let c7 = sample_case("frame-1", "fam-7", "c7", 0, EvaluationSplit::Train);

    let cases = vec![c1, c2, c3, c4, c5, c6, c7];
    let labels = vec![l1, l2, l3, l4, l5, l6];

    let (resolved, _) = join_evaluation_frame(&cases, &labels).unwrap();
    let metrics = compute_metrics(&resolved);

    assert_eq!(metrics.total_cases, 7);
    assert_eq!(metrics.judged_cases, 6);
    assert_eq!(metrics.unjudged_cases, 1);
    assert_eq!(metrics.operational_failures, 1);
    assert_eq!(metrics.advisory_cases, 5); // c1, c2, c3, c4, c5
    assert_eq!(metrics.positive_advisory_cases, 3); // c1 (TP), c2 (FP), c5 (FA)
    assert_eq!(metrics.no_match_advisory_cases, 2); // c3 (TA), c4 (Needless)

    // Candidate coverage rate on positive advisory cases: c1 and c2 covered, c5 not covered -> 2/3 ≈ 0.667
    assert!((metrics.candidate_coverage_rate.unwrap() - (2.0 / 3.0)).abs() < 1e-4);

    // Top-1 precision on emitted advisory suggestions:
    // Emitted suggestions: c1 (TP), c2 (FP), c4 (Needless) = 3 total. TP = 1. -> 1/3 ≈ 0.333
    assert!((metrics.top1_precision.unwrap() - (1.0 / 3.0)).abs() < 1e-4);

    // Positive suggestion rate on positive advisory cases: 1/3
    assert!((metrics.positive_suggestion_rate.unwrap() - (1.0 / 3.0)).abs() < 1e-4);

    // Needless suggestion rate on no-match advisory cases: 1 / 2 = 0.50
    assert_eq!(metrics.needless_suggestion_rate, Some(0.50));

    // False abstention rate on positive advisory cases: 1 / 3 ≈ 0.333
    assert!((metrics.false_abstention_rate.unwrap() - (1.0 / 3.0)).abs() < 1e-4);
}

#[test]
fn multiple_acceptable_skills_and_set_recall() {
    let mut c1 = sample_case("frame-1", "fam-1", "case-multi", 0, EvaluationSplit::Train);
    c1.suggested_skills = vec!["skill-a".into(), "skill-b".into()];

    // Label accepts 3 skills: a, b, c
    let l1 = sample_label("case-multi", 1, &["skill-a", "skill-b", "skill-c"]);

    let (resolved, _) = join_evaluation_frame(&[c1], &[l1]).unwrap();
    assert_eq!(resolved.len(), 1);
    let res = &resolved[0];

    assert_eq!(res.relevance_class, RelevanceClass::TruePositive);
    assert!(res.top1_match);
    assert!(res.candidate_coverage);
    // 2 out of 3 skills found in suggestions
    assert!((res.set_recall.unwrap() - (2.0 / 3.0)).abs() < 1e-4);
}

#[test]
fn explicit_request_separated_from_advisory_metrics() {
    let mut c_exp = sample_case("frame-1", "fam-1", "case-exp", 0, EvaluationSplit::Train);
    c_exp.decision = "explicit".into();
    c_exp.suggested_skills = vec!["beads-br".into()];

    let mut l_exp = sample_label("case-exp", 1, &["beads-br"]);
    l_exp.explicit_directive = Some("beads-br".into());

    let (resolved, _) = join_evaluation_frame(&[c_exp], &[l_exp]).unwrap();
    assert_eq!(resolved[0].relevance_class, RelevanceClass::ExplicitMatch);

    let metrics = compute_metrics(&resolved);
    assert_eq!(metrics.total_cases, 1);
    assert_eq!(metrics.judged_cases, 1);
    assert_eq!(metrics.explicit_cases, 1);
    assert_eq!(metrics.advisory_cases, 0);
    // Explicit cases do not pollute advisory precision or coverage
    assert_eq!(metrics.top1_precision, None);
    assert_eq!(metrics.candidate_coverage_rate, None);
}
