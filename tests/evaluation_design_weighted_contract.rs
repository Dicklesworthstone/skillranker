use skillranker::evaluation::design_weighted::*;
use skillranker::evaluation::stratified::*;
use skillranker::evaluation::{CaseKey, EvaluationCaseRecord, EvaluationSplit};
use std::collections::BTreeMap;

#[test]
fn test_exact_900_100_frame_reproduction() {
    // Contract requirement:
    // "For example, a frame with 900 routine cases and 100 overflow cases, sampled 50 from each,
    // has weights 0.9 and 0.1. Observed error rates 2% and 20% yield 0.9×0.02 + 0.1×0.20 = 3.8%,
    // whereas the unweighted sample reports 11%."

    let mut strata = BTreeMap::new();
    strata.insert(
        "routine:complete".to_string(),
        StratumAllocation {
            stratum_key: "routine:complete".to_string(),
            population_size: 900,
            sample_size: 50,
            weight: 0.9,
            inclusion_probability: Some(50.0 / 900.0),
        },
    );
    strata.insert(
        "overflow:complete".to_string(),
        StratumAllocation {
            stratum_key: "overflow:complete".to_string(),
            population_size: 100,
            sample_size: 50,
            weight: 0.1,
            inclusion_probability: Some(50.0 / 100.0),
        },
    );

    let mut cases = Vec::new();
    // Routine: 1 error out of 50 = 2% error rate
    for i in 0..50 {
        let loss = if i == 0 { 1.0 } else { 0.0 };
        cases.push(StratumCaseLoss::new(
            "routine:complete",
            SampledCaseLoss::Observed(loss),
        ));
    }
    // Overflow: 10 errors out of 50 = 20% error rate
    for i in 0..50 {
        let loss = if i < 10 { 1.0 } else { 0.0 };
        cases.push(StratumCaseLoss::new(
            "overflow:complete",
            SampledCaseLoss::Observed(loss),
        ));
    }

    let report = compute_design_weighted_loss(&strata, &cases, 0.05).unwrap();

    let r_hat = report.r_hat_observed.unwrap();
    assert!(
        (r_hat - 0.038).abs() < 1e-9,
        "Expected exact 3.8% (0.038), got {r_hat}"
    );
    assert!((report.r_hat_lower - 0.038).abs() < 1e-9);
    assert!((report.r_hat_upper - 0.038).abs() < 1e-9);

    // Unweighted sample mean would be (1 + 10) / 100 = 0.11
    let unweighted_mean: f64 = cases.iter().map(|c| c.loss.lower_assignment()).sum::<f64>() / 100.0;
    assert!((unweighted_mean - 0.11).abs() < 1e-9);

    // Conservative upper bound must be greater than r_hat and <= 1.0
    assert!(report.conservative_upper_bound > r_hat);
    assert!(report.conservative_upper_bound <= 1.0);
    assert!(!report.weights_clipped);
    assert!(report.point_estimate_guaranteed);
}

#[test]
fn test_tiny_population_exact_enumeration_unbiasedness_and_coverage() {
    // Mathematical proof test:
    // Enumerate ALL possible stratified samples from a finite population to prove:
    // 1. E[\hat{R}] == \mu (exact unbiasedness of Horvitz-Thompson mean)
    // 2. Empirical coverage of U >= 1 - alpha (validity of Hoeffding without replacement bound)

    // Stratum 1: N_1 = 4, values = [0.0, 0.0, 1.0, 1.0] (mean = 0.5)
    // Stratum 2: N_2 = 3, values = [0.0, 1.0, 1.0] (mean = 2/3)
    // Total population N = 7
    // True frame mean \mu = (4 * 0.5 + 3 * (2.0 / 3.0)) / 7 = (2 + 2) / 7 = 4/7 ~ 0.57142857
    let s1_pop = [0.0, 0.0, 1.0, 1.0];
    let s2_pop = [0.0, 1.0, 1.0];
    let true_mean = 4.0 / 7.0;

    let n1 = 2;
    let n2 = 2;

    let mut strata = BTreeMap::new();
    strata.insert(
        "s1".to_string(),
        StratumAllocation {
            stratum_key: "s1".to_string(),
            population_size: 4,
            sample_size: n1,
            weight: 4.0 / 7.0,
            inclusion_probability: Some(2.0 / 4.0),
        },
    );
    strata.insert(
        "s2".to_string(),
        StratumAllocation {
            stratum_key: "s2".to_string(),
            population_size: 3,
            sample_size: n2,
            weight: 3.0 / 7.0,
            inclusion_probability: Some(2.0 / 3.0),
        },
    );

    // Combinations of 2 from 4: 6 combinations
    let s1_samples = [
        [s1_pop[0], s1_pop[1]],
        [s1_pop[0], s1_pop[2]],
        [s1_pop[0], s1_pop[3]],
        [s1_pop[1], s1_pop[2]],
        [s1_pop[1], s1_pop[3]],
        [s1_pop[2], s1_pop[3]],
    ];
    // Combinations of 2 from 3: 3 combinations
    let s2_samples = [
        [s2_pop[0], s2_pop[1]],
        [s2_pop[0], s2_pop[2]],
        [s2_pop[1], s2_pop[2]],
    ];

    let alpha = 0.10;
    let mut sum_r_hat = 0.0;
    let mut covered_count = 0;
    let total_samples = s1_samples.len() * s2_samples.len(); // 18 samples

    for s1 in &s1_samples {
        for s2 in &s2_samples {
            let cases = vec![
                StratumCaseLoss::new("s1", SampledCaseLoss::Observed(s1[0])),
                StratumCaseLoss::new("s1", SampledCaseLoss::Observed(s1[1])),
                StratumCaseLoss::new("s2", SampledCaseLoss::Observed(s2[0])),
                StratumCaseLoss::new("s2", SampledCaseLoss::Observed(s2[1])),
            ];

            let report = compute_design_weighted_loss(&strata, &cases, alpha).unwrap();
            let r_hat = report.r_hat_observed.unwrap();
            sum_r_hat += r_hat;

            if true_mean <= report.conservative_upper_bound {
                covered_count += 1;
            }
        }
    }

    let expected_r_hat = sum_r_hat / (total_samples as f64);
    assert!(
        (expected_r_hat - true_mean).abs() < 1e-12,
        "Horvitz-Thompson mean must be exactly unbiased: expected {true_mean}, got {expected_r_hat}"
    );

    let coverage = (covered_count as f64) / (total_samples as f64);
    assert!(
        coverage >= 1.0 - alpha,
        "Conservative upper bound coverage must be >= 1 - alpha ({}), got {}",
        1.0 - alpha,
        coverage
    );
}

#[test]
fn test_extreme_weights_and_no_clipping() {
    // Test extreme population disparity: 1,000,000 cases vs 10 cases
    let mut strata = BTreeMap::new();
    strata.insert(
        "large".to_string(),
        StratumAllocation {
            stratum_key: "large".to_string(),
            population_size: 1_000_000,
            sample_size: 100,
            weight: 1_000_000.0 / 1_000_010.0,
            inclusion_probability: Some(100.0 / 1_000_000.0),
        },
    );
    strata.insert(
        "tiny".to_string(),
        StratumAllocation {
            stratum_key: "tiny".to_string(),
            population_size: 10,
            sample_size: 5,
            weight: 10.0 / 1_000_010.0,
            inclusion_probability: Some(5.0 / 10.0),
        },
    );

    let mut cases = Vec::new();
    for _ in 0..100 {
        cases.push(StratumCaseLoss::new("large", SampledCaseLoss::Observed(0.01)));
    }
    for _ in 0..5 {
        cases.push(StratumCaseLoss::new("tiny", SampledCaseLoss::Observed(0.80)));
    }

    let report = compute_design_weighted_loss(&strata, &cases, 0.01).unwrap();
    assert!(!report.weights_clipped);
    assert!(report.min_inclusion_probability.unwrap() < 0.001);
    assert!(report.max_inclusion_probability.unwrap() >= 0.5);

    // Large stratum dominates: weight ~ 0.99999
    let expected = (1_000_000.0 / 1_000_010.0) * 0.01 + (10.0 / 1_000_010.0) * 0.80;
    assert!((report.r_hat_observed.unwrap() - expected).abs() < 1e-8);
}

#[test]
fn test_missing_labels_bounds_and_conservative_upper_assignment() {
    let mut strata = BTreeMap::new();
    strata.insert(
        "s1".to_string(),
        StratumAllocation {
            stratum_key: "s1".to_string(),
            population_size: 100,
            sample_size: 4,
            weight: 1.0,
            inclusion_probability: Some(4.0 / 100.0),
        },
    );

    // 2 observed (0.2, 0.4) and 2 missing
    let cases = vec![
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.2)),
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.4)),
        StratumCaseLoss::new("s1", SampledCaseLoss::Missing),
        StratumCaseLoss::new("s1", SampledCaseLoss::Missing),
    ];

    let report = compute_design_weighted_loss(&strata, &cases, 0.05).unwrap();

    assert_eq!(report.total_missing_labels, 2);
    assert!(!report.point_estimate_guaranteed);

    // Lower assignment (missing as 0.0): (0.2 + 0.4 + 0.0 + 0.0) / 4 = 0.15
    assert!((report.r_hat_lower - 0.15).abs() < 1e-9);
    // Upper assignment (missing as 1.0): (0.2 + 0.4 + 1.0 + 1.0) / 4 = 0.65
    assert!((report.r_hat_upper - 0.65).abs() < 1e-9);
    // Observed only: (0.2 + 0.4) / 2 = 0.30
    assert!((report.r_hat_observed.unwrap() - 0.30).abs() < 1e-9);

    // Conservative upper bound must use the upper assignment:
    // U = min(1.0, mean_upper + margin) > 0.65
    assert!(report.conservative_upper_bound > 0.65);
    assert!(report.conservative_upper_bound <= 1.0);
}

#[test]
fn test_full_census_zero_sampling_variance() {
    let mut strata = BTreeMap::new();
    strata.insert(
        "s1".to_string(),
        StratumAllocation {
            stratum_key: "s1".to_string(),
            population_size: 3,
            sample_size: 3,
            weight: 0.6,
            inclusion_probability: Some(1.0),
        },
    );
    strata.insert(
        "s2".to_string(),
        StratumAllocation {
            stratum_key: "s2".to_string(),
            population_size: 2,
            sample_size: 2,
            weight: 0.4,
            inclusion_probability: Some(1.0),
        },
    );

    let cases = vec![
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.1)),
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.2)),
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.3)),
        StratumCaseLoss::new("s2", SampledCaseLoss::Observed(0.5)),
        StratumCaseLoss::new("s2", SampledCaseLoss::Observed(0.5)),
    ];

    let report = compute_design_weighted_loss(&strata, &cases, 0.05).unwrap();

    let mean_s1 = (0.1 + 0.2 + 0.3) / 3.0; // 0.2
    let mean_s2 = (0.5 + 0.5) / 2.0; // 0.5
    let exact_mean = 0.6 * mean_s1 + 0.4 * mean_s2; // 0.12 + 0.20 = 0.32

    assert!((report.r_hat_observed.unwrap() - exact_mean).abs() < 1e-9);
    // In full census, upper bound equals the exact mean with zero sampling variance!
    assert!((report.conservative_upper_bound - exact_mean).abs() < 1e-9);
    assert!(report.strata["s1"].is_census);
    assert!(report.strata["s2"].is_census);
    assert!(report.point_estimate_guaranteed);
}

#[test]
fn test_multi_endpoint_alpha_allocation() {
    let alloc = MultiEndpointAlphaAllocation::equal_split(
        0.06,
        &["error_rate", "false_abstention", "harm_rate"],
    )
    .unwrap();

    assert_eq!(alloc.endpoints().len(), 3);
    assert!((alloc.get("error_rate").unwrap() - 0.02).abs() < 1e-9);
    assert!((alloc.get("false_abstention").unwrap() - 0.02).abs() < 1e-9);
    assert!((alloc.get("harm_rate").unwrap() - 0.02).abs() < 1e-9);

    // Over-budget allocation must fail
    let mut explicit_map = BTreeMap::new();
    explicit_map.insert("e1".to_string(), 0.04);
    explicit_map.insert("e2".to_string(), 0.03); // sum 0.07 > 0.06
    assert!(matches!(
        MultiEndpointAlphaAllocation::explicit(0.06, explicit_map),
        Err(DesignWeightedError::AlphaBudgetExceeded { .. })
    ));
}

#[test]
fn test_design_weighted_ratio_estimator() {
    let mut strata = BTreeMap::new();
    strata.insert(
        "s1".to_string(),
        StratumAllocation {
            stratum_key: "s1".to_string(),
            population_size: 100,
            sample_size: 2,
            weight: 0.5,
            inclusion_probability: Some(2.0 / 100.0),
        },
    );
    strata.insert(
        "s2".to_string(),
        StratumAllocation {
            stratum_key: "s2".to_string(),
            population_size: 100,
            sample_size: 2,
            weight: 0.5,
            inclusion_probability: Some(2.0 / 100.0),
        },
    );

    // Ratio: estimated correct emissions / estimated emissions
    let pairs = vec![
        StratumCaseRatioPair {
            stratum_key: "s1".to_string(),
            numerator: SampledCaseLoss::Observed(1.0),
            denominator: SampledCaseLoss::Observed(1.0),
        },
        StratumCaseRatioPair {
            stratum_key: "s1".to_string(),
            numerator: SampledCaseLoss::Observed(0.0),
            denominator: SampledCaseLoss::Observed(1.0),
        },
        StratumCaseRatioPair {
            stratum_key: "s2".to_string(),
            numerator: SampledCaseLoss::Observed(1.0),
            denominator: SampledCaseLoss::Observed(1.0),
        },
        StratumCaseRatioPair {
            stratum_key: "s2".to_string(),
            numerator: SampledCaseLoss::Observed(1.0),
            denominator: SampledCaseLoss::Observed(1.0),
        },
    ];

    let ratio_report = compute_design_weighted_ratio(&strata, &pairs).unwrap();
    // Num: 0.5 * (1/2) + 0.5 * (2/2) = 0.25 + 0.50 = 0.75
    // Den: 0.5 * (2/2) + 0.5 * (2/2) = 0.50 + 0.50 = 1.00
    // Ratio = 0.75 / 1.00 = 0.75
    assert!((ratio_report.numerator_hat - 0.75).abs() < 1e-9);
    assert!((ratio_report.denominator_hat - 1.00).abs() < 1e-9);
    assert!((ratio_report.ratio - 0.75).abs() < 1e-9);
}

#[test]
fn test_manifest_integration_and_replay() {
    let frame: Vec<EvaluationCaseRecord> = (0..8)
        .map(|i| EvaluationCaseRecord {
            schema_version: 1,
            key: CaseKey::new("frame", format!("family-{i}"), "case", 0, "policy"),
            split: EvaluationSplit::Holdout,
            prompt_summary: Some("synthetic test".into()),
            roster_skills: if i < 4 {
                vec!["skill-a".into()]
            } else {
                (0..300).map(|j| format!("skill-{j}")).collect()
            },
            decision: "ranked".into(),
            suggested_skills: vec!["skill-a".into()],
            fits: BTreeMap::new(),
            gate_score: None,
            relevance_abstention: false,
            operational_failure: false,
        })
        .collect();

    let manifest = draw_stratified_sample(
        &frame,
        EvaluationSplit::Holdout,
        4,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed: 999 },
        "policy",
        1,
    )
    .unwrap();

    let mut losses = BTreeMap::new();
    for entry in &manifest.selected_cases {
        losses.insert(entry.case_key.clone(), SampledCaseLoss::Observed(0.25));
    }

    let report = compute_design_weighted_loss_from_manifest(&manifest, &losses, 0.05).unwrap();
    assert_eq!(report.total_sampled_cases, 4);
    assert!((report.r_hat_observed.unwrap() - 0.25).abs() < 1e-9);
    assert!(report.conservative_upper_bound > 0.25);
    assert!(report.conservative_upper_bound <= 1.0);
}

#[test]
fn test_adversarial_inputs_fail_closed() {
    let mut strata = BTreeMap::new();
    strata.insert(
        "s1".to_string(),
        StratumAllocation {
            stratum_key: "s1".to_string(),
            population_size: 10,
            sample_size: 2,
            weight: 1.0,
            inclusion_probability: Some(0.2),
        },
    );
    let cases = vec![
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.5)),
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.5)),
    ];

    // Invalid alpha
    assert!(matches!(
        compute_design_weighted_loss(&strata, &cases, 0.0),
        Err(DesignWeightedError::InvalidAlpha(_))
    ));
    assert!(matches!(
        compute_design_weighted_loss(&strata, &cases, 1.0),
        Err(DesignWeightedError::InvalidAlpha(_))
    ));
    assert!(matches!(
        compute_design_weighted_loss(&strata, &cases, f64::NAN),
        Err(DesignWeightedError::InvalidAlpha(_))
    ));

    // Invalid loss
    let bad_cases = vec![
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(-0.1)),
        StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.5)),
    ];
    assert!(matches!(
        compute_design_weighted_loss(&strata, &bad_cases, 0.05),
        Err(DesignWeightedError::InvalidLoss { .. })
    ));

    // Mismatched sample count
    let short_cases = vec![StratumCaseLoss::new("s1", SampledCaseLoss::Observed(0.5))];
    assert!(matches!(
        compute_design_weighted_loss(&strata, &short_cases, 0.05),
        Err(DesignWeightedError::InconsistentSampleCount { .. })
    ));
}
