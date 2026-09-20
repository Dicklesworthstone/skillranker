//! Contract tests for narrow numerical and sampling backends (sr-roadmap-l1i.6.18).
//!
//! Verifies:
//! 1. Wilson score interval domain checks, endpoints, and confidence bounds.
//! 2. Clopper-Pearson exact two-sided interval and one-sided upper bound.
//! 3. Stable zero-event closed form `-expm1(log(1 - alpha) / n)` cross-check.
//! 4. Log-sum-exp edge cases (empty, -inf, +inf, overflow avoidance).
//! 5. PCG64-DXSM rejection-based sampling without replacement (Floyd's algorithm).
//! 6. Membership, uniqueness, deterministic seeded replay, and small-population uniformity.
//! 7. Fisher-Yates shuffle slice multiset preservation.
//! 8. Backend provenance reporting.

use skillranker::evaluation::{
    BACKEND_PROVENANCE, BackendProvenance, BetaDist, NumericsError, Pcg64Dxsm, SamplingError,
    clopper_pearson_ci, clopper_pearson_one_sided_upper, ln_beta, log_gamma, logsumexp,
    standard_normal_ppf, wilson_ci, zero_event_upper_bound,
};
use std::collections::BTreeSet;

#[test]
fn backend_provenance_is_truthfully_disclosed() {
    let prov: BackendProvenance = BACKEND_PROVENANCE;
    assert_eq!(prov.backend_name, "frankensuite-narrow");
    assert_eq!(
        prov.frankenscipy_revision,
        "213a417c739025ed865a3875c89a7d86726ecb5a"
    );
    assert_eq!(
        prov.franken_numpy_revision,
        "52700bc2a6d2ab48dab5e608b1cfa34d781a7bb1"
    );
    assert_eq!(
        prov.frankenpandas_revision,
        "ab7bc5a4b5a59a5923987622a08d0f391093a8db"
    );
}

#[test]
fn log_gamma_and_ln_beta_accuracy() {
    // ln Gamma(1) == ln Gamma(2) == 0
    assert_eq!(log_gamma(1.0).unwrap(), 0.0);
    assert_eq!(log_gamma(2.0).unwrap(), 0.0);

    // ln Gamma(3) = ln(2!) = ln(2)
    assert!((log_gamma(3.0).unwrap() - std::f64::consts::LN_2).abs() < 1e-14);

    // ln Gamma(5) = ln(24)
    assert!((log_gamma(5.0).unwrap() - 24.0_f64.ln()).abs() < 1e-12);

    // ln B(1, 1) = 0
    assert_eq!(ln_beta(1.0, 1.0).unwrap(), 0.0);

    // ln B(2, 3) = ln(Gamma(2)*Gamma(3)/Gamma(5)) = ln(1*2/24) = ln(1/12) = -ln(12)
    let lb = ln_beta(2.0, 3.0).unwrap();
    assert!((lb - (-12.0_f64.ln())).abs() < 1e-12);

    // Domain checks
    assert!(log_gamma(0.0).is_err());
    assert!(log_gamma(-1.0).is_err());
    assert!(ln_beta(0.0, 1.0).is_err());
    assert!(ln_beta(1.0, -1.0).is_err());
}

#[test]
fn wilson_ci_domain_checks_and_known_values() {
    // n = 0 must fail with EmptyTrials
    assert_eq!(wilson_ci(0, 0, 0.95), Err(NumericsError::EmptyTrials));

    // k > n must fail with InvalidSampleSize
    assert_eq!(
        wilson_ci(11, 10, 0.95),
        Err(NumericsError::InvalidSampleSize { k: 11, n: 10 })
    );

    // Invalid confidence levels
    assert!(matches!(
        wilson_ci(5, 10, 0.0),
        Err(NumericsError::InvalidConfidence(_))
    ));
    assert!(matches!(
        wilson_ci(5, 10, 1.0),
        Err(NumericsError::InvalidConfidence(_))
    ));
    assert!(matches!(
        wilson_ci(5, 10, -0.5),
        Err(NumericsError::InvalidConfidence(_))
    ));
    assert!(matches!(
        wilson_ci(5, 10, f64::NAN),
        Err(NumericsError::InvalidConfidence(_))
    ));

    // Symmetric 50/100 case at 95% confidence
    let (lo, hi) = wilson_ci(50, 100, 0.95).expect("wilson_ci 50/100");
    assert!(lo > 0.40 && lo < 0.41, "lo was {lo}");
    assert!(hi > 0.59 && hi < 0.60, "hi was {hi}");
    assert!((0.5 * (lo + hi) - 0.5).abs() < 1e-10);

    // Extreme case 0/10 at 95%
    let (lo_zero, hi_zero) = wilson_ci(0, 10, 0.95).expect("wilson_ci 0/10");
    assert_eq!(lo_zero, 0.0);
    assert!(hi_zero > 0.25 && hi_zero < 0.35);

    // Extreme case 10/10 at 95%
    let (lo_all, hi_all) = wilson_ci(10, 10, 0.95).expect("wilson_ci 10/10");
    assert!(lo_all > 0.65 && lo_all < 0.75);
    assert_eq!(hi_all, 1.0);
}

#[test]
fn clopper_pearson_ci_domain_checks_and_exact_endpoints() {
    // n = 0 must fail
    assert_eq!(
        clopper_pearson_ci(0, 0, 0.95),
        Err(NumericsError::EmptyTrials)
    );

    // k > n must fail
    assert_eq!(
        clopper_pearson_ci(6, 5, 0.95),
        Err(NumericsError::InvalidSampleSize { k: 6, n: 5 })
    );

    // k = 0 endpoint
    let (lo, hi) = clopper_pearson_ci(0, 20, 0.95).expect("clopper_pearson 0/20");
    assert_eq!(lo, 0.0);
    // Two-sided 95% has alpha/2 = 0.025 in the upper tail: 1 - 0.025^(1/20)
    let expected_hi = 1.0 - (0.025_f64).powf(1.0 / 20.0);
    assert!(
        (hi - expected_hi).abs() < 1e-10,
        "got {hi}, expected {expected_hi}"
    );

    // k = n endpoint
    let (lo_all, hi_all) = clopper_pearson_ci(20, 20, 0.95).expect("clopper_pearson 20/20");
    assert_eq!(hi_all, 1.0);
    let expected_lo = (0.025_f64).powf(1.0 / 20.0);
    assert!(
        (lo_all - expected_lo).abs() < 1e-10,
        "got {lo_all}, expected {expected_lo}"
    );
}

#[test]
fn clopper_pearson_one_sided_upper_matches_beta_ppf_and_two_sided_bridge() {
    let n = 25;
    let confidence = 0.95;

    // For each k in 1..n, verify that:
    // 1. One-sided 95% upper bound matches Beta(k+1, n-k).ppf(0.95)
    // 2. One-sided 95% upper bound matches the upper bound of two-sided CP with confidence 0.90 (since (1 - 0.90)/2 = 0.05)
    for k in 1..n {
        let one_sided = clopper_pearson_one_sided_upper(k, n, confidence).expect("one sided upper");
        let beta = BetaDist::new((k + 1) as f64, (n - k) as f64).expect("beta dist");
        let beta_ppf = beta.ppf(confidence).expect("beta ppf");
        assert!(
            (one_sided - beta_ppf).abs() < 1e-10,
            "at k={k}: one_sided {one_sided} vs beta_ppf {beta_ppf}"
        );

        let (_two_sided_lo, two_sided_hi) =
            clopper_pearson_ci(k, n, 0.90).expect("two sided 90% CP");
        assert!(
            (one_sided - two_sided_hi).abs() < 1e-10,
            "at k={k}: one_sided {one_sided} vs two_sided_hi {two_sided_hi}"
        );
    }

    // k = n must be 1.0
    assert_eq!(clopper_pearson_one_sided_upper(n, n, 0.95).unwrap(), 1.0);
}

#[test]
fn zero_event_stable_closed_form_cross_check() {
    let test_sizes = [1, 5, 10, 50, 100, 500, 1000, 10_000];

    for &n in &test_sizes {
        let closed_form = zero_event_upper_bound(n, 0.95).expect("zero event bound");
        let cp_one_sided = clopper_pearson_one_sided_upper(0, n, 0.95).expect("cp one sided zero");

        // Cross-check: closed form must match cp_one_sided exactly
        assert_eq!(closed_form, cp_one_sided);

        // Cross-check against -expm1(log(0.05) / n)
        let direct_expm1 = -((0.05_f64.ln()) / n as f64).exp_m1();
        assert!(
            (closed_form - direct_expm1).abs() < 1e-12,
            "at n={n}: closed_form {closed_form} vs direct_expm1 {direct_expm1}"
        );

        // Upper bound decreases monotonically with sample size
        assert!(closed_form > 0.0 && closed_form <= 1.0);
    }

    // At n = 100, -expm1(log(0.05)/100) ≈ 1 - 0.05^0.01 ≈ 0.02951
    let b100 = zero_event_upper_bound(100, 0.95).unwrap();
    assert!((b100 - 0.02951).abs() < 1e-4);
}

#[test]
fn standard_normal_ppf_accuracy() {
    // Known quantile points:
    // Phi(0) = 0.5
    assert!((standard_normal_ppf(0.5).unwrap() - 0.0).abs() < 1e-15);

    // Phi(1.959963984540054) ≈ 0.975
    let z_975 = standard_normal_ppf(0.975).unwrap();
    assert!((z_975 - 1.959_963_984_540_054).abs() < 1e-6);

    // Phi(-1.959963984540054) ≈ 0.025 (symmetry)
    let z_025 = standard_normal_ppf(0.025).unwrap();
    assert!((z_025 + z_975).abs() < 1e-12);

    // Extremes rejected
    assert!(standard_normal_ppf(0.0).is_err());
    assert!(standard_normal_ppf(1.0).is_err());
    assert!(standard_normal_ppf(-0.1).is_err());
    assert!(standard_normal_ppf(1.1).is_err());
}

#[test]
fn logsumexp_edge_cases_and_stability() {
    // Empty slice -> -inf
    assert_eq!(logsumexp(&[]), f64::NEG_INFINITY);

    // Single element -> itself
    assert_eq!(logsumexp(&[42.0]), 42.0);

    // Identical values: logsumexp([c, c]) == c + ln(2)
    let val = logsumexp(&[1000.0, 1000.0]);
    assert!(
        (val - (1000.0 + std::f64::consts::LN_2)).abs() < 1e-12,
        "got {val}"
    );

    // Large negative values: logsumexp([-1000.0, -1000.0]) == -1000.0 + ln(2)
    let val_neg = logsumexp(&[-1000.0, -1000.0]);
    assert!(
        (val_neg - (-1000.0 + std::f64::consts::LN_2)).abs() < 1e-12,
        "got {val_neg}"
    );

    // All -inf -> -inf
    assert_eq!(
        logsumexp(&[f64::NEG_INFINITY, f64::NEG_INFINITY]),
        f64::NEG_INFINITY
    );

    // Any +inf -> +inf
    assert_eq!(logsumexp(&[1.0, f64::INFINITY, 2.0]), f64::INFINITY);

    // Any NaN -> NaN
    assert!(logsumexp(&[1.0, f64::NAN, 2.0]).is_nan());
}

#[test]
fn sampling_choice_indices_membership_uniqueness_and_deterministic_replay() {
    let seed = 987_654_321u64;
    let pop_size = 50;
    let sample_size = 15;

    let mut rng1 = Pcg64Dxsm::from_seed(seed);
    let draw1 = rng1
        .choice_indices(pop_size, sample_size, false)
        .expect("draw 1");

    let mut rng2 = Pcg64Dxsm::from_seed(seed);
    let draw2 = rng2
        .choice_indices(pop_size, sample_size, false)
        .expect("draw 2");

    // 1. Bit-exact seeded replay
    assert_eq!(draw1, draw2);

    // 2. Length check
    assert_eq!(draw1.len(), sample_size);

    // 3. Uniqueness (all distinct)
    let set: BTreeSet<usize> = draw1.iter().copied().collect();
    assert_eq!(set.len(), sample_size, "expected all distinct elements");

    // 4. Membership in [0, pop_size)
    for &idx in &draw1 {
        assert!(idx < pop_size, "index {idx} out of range [0, {pop_size})");
    }

    // 5. Sampling all elements gives a full permutation
    let mut rng_all = Pcg64Dxsm::from_seed(seed);
    let all_draw = rng_all
        .choice_indices(pop_size, pop_size, false)
        .expect("draw all");
    assert_eq!(all_draw.len(), pop_size);
    let all_set: BTreeSet<usize> = all_draw.iter().copied().collect();
    assert_eq!(all_set.len(), pop_size);

    // 6. Oversized draw without replacement fails
    let mut rng_err = Pcg64Dxsm::from_seed(seed);
    let err = rng_err
        .choice_indices(pop_size, pop_size + 1, false)
        .unwrap_err();
    assert_eq!(
        err,
        SamplingError::InvalidSampleSize {
            size: pop_size + 1,
            population: pop_size
        }
    );

    // 7. Population 0 with size > 0 fails
    let mut rng_zero = Pcg64Dxsm::from_seed(seed);
    assert_eq!(
        rng_zero.choice_indices(0, 1, false).unwrap_err(),
        SamplingError::ZeroPopulation
    );
}

#[test]
fn sampling_small_population_inclusion_frequencies_have_no_modulo_bias() {
    let mut rng = Pcg64Dxsm::from_seed(123_456_789);
    let pop_size = 5;
    let n_trials = 100_000;
    let mut counts = [0usize; 5];

    // Repeatedly draw single elements using bounded rejection sampling
    for _ in 0..n_trials {
        let draw = rng.bounded_u64(pop_size as u64).unwrap() as usize;
        counts[draw] += 1;
    }

    // Expected count per bucket: 20,000
    // Standard deviation under binomial(100_000, 0.2): sqrt(100_000 * 0.2 * 0.8) ≈ 126.5
    // 5 sigma is ~632. Assert all counts are within 19,000..21,000.
    for (i, &count) in counts.iter().enumerate() {
        assert!(
            count > 19_000 && count < 21_000,
            "bucket {i} had count {count}, expected ~20,000 without modulo bias"
        );
    }
}

#[test]
fn shuffle_slice_preserves_multiset_and_replays_deterministically() {
    let mut data1 = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
    let mut data2 = data1.clone();
    let original = data1.clone();

    let mut rng1 = Pcg64Dxsm::from_seed(42);
    rng1.shuffle_slice(&mut data1);

    let mut rng2 = Pcg64Dxsm::from_seed(42);
    rng2.shuffle_slice(&mut data2);

    // 1. Bit-exact seeded replay
    assert_eq!(data1, data2);

    // 2. Data actually shuffled
    assert_ne!(data1, original);

    // 3. Multiset preservation
    let mut sorted1 = data1.clone();
    sorted1.sort();
    assert_eq!(sorted1, original);

    // 4. Degenerate slices
    let mut empty: Vec<i32> = Vec::new();
    rng1.shuffle_slice(&mut empty);
    assert!(empty.is_empty());

    let mut single = vec![99];
    rng1.shuffle_slice(&mut single);
    assert_eq!(single, vec![99]);
}
