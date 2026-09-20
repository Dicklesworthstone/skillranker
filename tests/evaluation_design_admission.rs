use skillranker::evaluation::design_weighted::*;
use skillranker::evaluation::stratified::*;
use skillranker::evaluation::{CaseKey, EvaluationCaseRecord, EvaluationSplit};
use std::collections::BTreeMap;

fn allocation(pi: Option<f64>, census: bool) -> BTreeMap<String, StratumAllocation> {
    BTreeMap::from([(
        "a".into(),
        StratumAllocation {
            stratum_key: "a".into(),
            population_size: if census { 2 } else { 10 },
            sample_size: 2,
            weight: 1.0,
            inclusion_probability: pi,
        },
    )])
}

fn losses() -> Vec<StratumCaseLoss> {
    vec![
        StratumCaseLoss::new("a", SampledCaseLoss::Observed(0.0)),
        StratumCaseLoss::new("a", SampledCaseLoss::Observed(1.0)),
    ]
}

#[test]
fn missing_non_census_probabilities_do_not_become_guarantees() {
    assert!(compute_design_weighted_loss(&allocation(None, false), &losses(), 0.05).is_err());
}

#[test]
fn supplied_probabilities_must_be_finite_positive_and_match_the_design() {
    for pi in [f64::NAN, f64::INFINITY, -0.1, 0.0, 1.1, 0.5] {
        assert!(
            compute_design_weighted_loss(&allocation(Some(pi), false), &losses(), 0.05).is_err(),
            "accepted {pi}"
        );
    }
    assert!(compute_design_weighted_loss(&allocation(Some(0.2), true), &losses(), 0.05).is_err());
}

#[test]
fn valid_probability_and_enumerated_census_preserve_estimates() {
    for (pi, census) in [(Some(0.2), false), (Some(1.0), true), (None, true)] {
        let report =
            compute_design_weighted_loss(&allocation(pi, census), &losses(), 0.05).unwrap();
        assert!(report.point_estimate_guaranteed);
        assert_eq!(report.r_hat_observed, Some(0.5));
        assert_eq!(
            report.min_inclusion_probability,
            Some(if census { 1.0 } else { 0.2 })
        );
        if census {
            assert_eq!(report.conservative_upper_bound, 0.5);
        }
    }
}

#[test]
fn diagnostic_manifest_cannot_be_promoted_by_inserting_probabilities() {
    let cases: Vec<_> = (0..4)
        .map(|i| EvaluationCaseRecord {
            schema_version: 1,
            key: CaseKey::new("frame", format!("family-{i}"), "case", 0, "policy"),
            split: EvaluationSplit::Holdout,
            prompt_summary: Some("synthetic".into()),
            roster_skills: vec!["skill".into()],
            decision: "abstain".into(),
            suggested_skills: vec![],
            fits: BTreeMap::new(),
            gate_score: None,
            relevance_abstention: true,
            operational_failure: false,
        })
        .collect();
    let mut manifest = draw_stratified_sample(
        &cases,
        EvaluationSplit::Holdout,
        2,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed: 42 },
        "policy",
        1,
    )
    .unwrap();
    let labels = manifest
        .selected_cases
        .iter()
        .map(|entry| (entry.case_key.clone(), SampledCaseLoss::Observed(0.0)))
        .collect();
    assert!(compute_design_weighted_loss_from_manifest(&manifest, &labels, 0.05).is_err());
    for stratum in manifest.strata.values_mut() {
        stratum.inclusion_probability = Some(0.5);
    }
    for entry in &mut manifest.selected_cases {
        entry.inclusion_probability = Some(0.5);
    }
    manifest.manifest_id = format!("man-{}", &manifest.compute_manifest_digest()[..16]);
    assert!(compute_design_weighted_loss_from_manifest(&manifest, &labels, 0.05).is_err());
    manifest.design_status = DesignStatus::StratifiedProbabilitySample;
    manifest.manifest_id = format!("man-{}", &manifest.compute_manifest_digest()[..16]);
    assert!(compute_design_weighted_loss_from_manifest(&manifest, &labels, 0.05).is_err());
}
