//! Contract test suite for stratified family sampling and randomization provenance (sr-roadmap-l1i.6.21).

use skillranker::evaluation::stratified::{
    AllocationMethod, DesignStatus, EstimandWeighting, FamilyRepresentativeRule,
    FrozenSampleManifest, InputQuality, ObservableStratum, RandomizationProvenance,
    RetrievalProfile, allocate_sample_sizes, compute_frame_digest, draw_os_seed,
    draw_stratified_sample, draw_stratified_sample_with_rule, partition_strata,
    replay_manifest_sample, select_family_representatives, verify_manifest_against_frame,
};
use skillranker::evaluation::{CaseKey, EvaluationCaseRecord, EvaluationError, EvaluationSplit};
use std::collections::BTreeMap;

#[allow(clippy::too_many_arguments)]
fn make_test_case(
    family_id: &str,
    case_id: &str,
    replicate: u32,
    split: EvaluationSplit,
    roster_count: usize,
    prompt_summary: Option<&str>,
    decision: &str,
    relevance_abstention: bool,
    operational_failure: bool,
) -> EvaluationCaseRecord {
    EvaluationCaseRecord {
        schema_version: 1,
        key: CaseKey::new(
            "frame-2026",
            family_id,
            case_id,
            replicate,
            "policy-prod-v1",
        ),
        split,
        prompt_summary: prompt_summary.map(|s| s.to_string()),
        roster_skills: (0..roster_count).map(|i| format!("skill-{i}")).collect(),
        decision: decision.to_string(),
        suggested_skills: if decision == "ranked" {
            vec!["skill-0".to_string()]
        } else {
            Vec::new()
        },
        fits: BTreeMap::new(),
        gate_score: Some(0.85),
        relevance_abstention,
        operational_failure,
    }
}

#[test]
fn test_family_representative_selection_and_split_isolation() {
    let cases = vec![
        // Family 1: 3 replicates of case-b, 1 of case-a
        make_test_case(
            "fam-1",
            "case-b",
            2,
            EvaluationSplit::Holdout,
            50,
            Some("prompt b2"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-1",
            "case-a",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("prompt a1"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-1",
            "case-b",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("prompt b1"),
            "ranked",
            false,
            false,
        ),
        // Family 2: 1 case
        make_test_case(
            "fam-2",
            "case-x",
            1,
            EvaluationSplit::Holdout,
            10,
            Some("prompt x"),
            "abstain",
            true,
            false,
        ),
        // Family 3: in Train split
        make_test_case(
            "fam-3",
            "case-t",
            1,
            EvaluationSplit::Train,
            20,
            Some("prompt t"),
            "ranked",
            false,
            false,
        ),
    ];

    // Selecting for Holdout should return fam-1 and fam-2 only
    let reps = select_family_representatives(
        &cases,
        EvaluationSplit::Holdout,
        FamilyRepresentativeRule::FirstByCaseIdReplicate,
    )
    .expect("selection should succeed");

    assert_eq!(reps.len(), 2);
    assert_eq!(reps[0].key.family_id, "fam-1");
    // Under FirstByCaseIdReplicate, case-a:1 precedes case-b:1 and case-b:2
    assert_eq!(reps[0].key.case_id, "case-a");
    assert_eq!(reps[0].key.replicate, 1);

    assert_eq!(reps[1].key.family_id, "fam-2");
    assert_eq!(reps[1].key.case_id, "case-x");

    // Under LowestReplicateFirst on another group
    let cases_reps = vec![
        make_test_case(
            "fam-z",
            "case-z2",
            1,
            EvaluationSplit::Validation,
            10,
            Some("p"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-z",
            "case-z1",
            2,
            EvaluationSplit::Validation,
            10,
            Some("p"),
            "ranked",
            false,
            false,
        ),
    ];
    let reps_low = select_family_representatives(
        &cases_reps,
        EvaluationSplit::Validation,
        FamilyRepresentativeRule::LowestReplicateFirst,
    )
    .expect("lowest replicate first should succeed");
    assert_eq!(reps_low.len(), 1);
    assert_eq!(reps_low[0].key.case_id, "case-z2"); // replicate 1 wins over replicate 2

    // Test split contamination rejection
    let contaminated_cases = vec![
        make_test_case(
            "fam-bad",
            "case-1",
            1,
            EvaluationSplit::Train,
            10,
            Some("p"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-bad",
            "case-2",
            1,
            EvaluationSplit::Holdout,
            10,
            Some("p"),
            "ranked",
            false,
            false,
        ),
    ];
    let err = select_family_representatives(
        &contaminated_cases,
        EvaluationSplit::Holdout,
        FamilyRepresentativeRule::FirstByCaseIdReplicate,
    )
    .unwrap_err();

    assert!(matches!(err, EvaluationError::SplitContamination(_)));
}

#[test]
fn test_observable_stratum_classification_and_invariants() {
    // Normal + Complete
    let c1 = make_test_case(
        "fam-1",
        "c1",
        1,
        EvaluationSplit::Holdout,
        200,
        Some("full prompt"),
        "ranked",
        false,
        false,
    );
    let s1 = ObservableStratum::classify(&c1);
    assert_eq!(s1.retrieval, RetrievalProfile::Normal);
    assert_eq!(s1.input_quality, InputQuality::Complete);
    assert_eq!(s1.key(), "normal:complete");

    // Overflow + Degraded (no prompt summary)
    let c2 = make_test_case(
        "fam-2",
        "c2",
        1,
        EvaluationSplit::Holdout,
        300,
        None,
        "ranked",
        false,
        false,
    );
    let s2 = ObservableStratum::classify(&c2);
    assert_eq!(s2.retrieval, RetrievalProfile::Overflow);
    assert_eq!(s2.input_quality, InputQuality::Degraded);
    assert_eq!(s2.key(), "overflow:degraded");

    // Gate-abstained case remains in the frame and gets proper observable stratum
    let c3 = make_test_case(
        "fam-3",
        "c3",
        1,
        EvaluationSplit::Holdout,
        100,
        Some("prompt"),
        "abstain",
        true,
        false,
    );
    let s3 = ObservableStratum::classify(&c3);
    assert_eq!(s3.retrieval, RetrievalProfile::Normal);
    assert_eq!(s3.input_quality, InputQuality::Complete);
    assert_eq!(s3.key(), "normal:complete");

    // Operational failure with missing roster skills gets explicit unknown stratum
    let c4 = make_test_case(
        "fam-4",
        "c4",
        1,
        EvaluationSplit::Holdout,
        0,
        None,
        "unavailable",
        false,
        true,
    );
    let s4 = ObservableStratum::classify(&c4);
    assert_eq!(s4.retrieval, RetrievalProfile::Unknown);
    assert_eq!(s4.input_quality, InputQuality::Unknown);
    assert_eq!(s4.key(), "unknown:unknown");

    let partitioned = partition_strata(&[c1, c2, c3, c4]);
    assert_eq!(partitioned.len(), 3);
    assert_eq!(partitioned.get("normal:complete").unwrap().len(), 2);
    assert_eq!(partitioned.get("overflow:degraded").unwrap().len(), 1);
    assert_eq!(partitioned.get("unknown:unknown").unwrap().len(), 1);
}

#[test]
fn test_allocation_methods_and_floors() {
    let mut strata_sizes = BTreeMap::new();
    strata_sizes.insert("normal:complete".to_string(), 90);
    strata_sizes.insert("overflow:complete".to_string(), 10);

    // Proportional allocation: total 50 out of 100
    let alloc = allocate_sample_sizes(
        &strata_sizes,
        50,
        &AllocationMethod::Proportional { min_floor: 2 },
    )
    .expect("proportional allocation should succeed");

    assert_eq!(alloc.len(), 2);
    let n_norm = alloc["normal:complete"];
    let n_over = alloc["overflow:complete"];
    assert_eq!(n_norm + n_over, 50);
    assert!(n_over >= 2, "must meet floor of 2");
    assert!(n_norm <= 90);
    assert!(n_over <= 10);

    // Budget covers total population -> exact census
    let alloc_census = allocate_sample_sizes(
        &strata_sizes,
        150,
        &AllocationMethod::Proportional { min_floor: 1 },
    )
    .expect("census allocation should succeed");
    assert_eq!(alloc_census["normal:complete"], 90);
    assert_eq!(alloc_census["overflow:complete"], 10);

    // Insufficient budget for stratum floors
    let err = allocate_sample_sizes(
        &strata_sizes,
        3,
        &AllocationMethod::Proportional { min_floor: 2 },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        EvaluationError::InsufficientSampleBudgetForStrata {
            budget: 3,
            required_floor: 4
        }
    ));

    // Equal allocation
    let alloc_eq = allocate_sample_sizes(
        &strata_sizes,
        20,
        &AllocationMethod::EqualPerStratum { min_floor: 1 },
    )
    .expect("equal allocation should succeed");
    assert_eq!(
        alloc_eq["normal:complete"] + alloc_eq["overflow:complete"],
        20
    );

    // Explicit allocation
    let mut explicit_map = BTreeMap::new();
    explicit_map.insert("normal:complete".to_string(), 40);
    explicit_map.insert("overflow:complete".to_string(), 8);
    let alloc_exp = allocate_sample_sizes(
        &strata_sizes,
        48,
        &AllocationMethod::Explicit {
            allocations: explicit_map,
        },
    )
    .expect("explicit allocation should succeed");
    assert_eq!(alloc_exp["normal:complete"], 40);
    assert_eq!(alloc_exp["overflow:complete"], 8);
}

#[test]
fn test_stratified_sample_draw_and_provenance() {
    let mut reps = Vec::new();
    // 80 normal cases
    for i in 0..80 {
        reps.push(make_test_case(
            &format!("fam-norm-{i:03}"),
            "case-1",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("prompt"),
            "ranked",
            false,
            false,
        ));
    }
    // 20 overflow cases
    for i in 0..20 {
        reps.push(make_test_case(
            &format!("fam-over-{i:03}"),
            "case-1",
            1,
            EvaluationSplit::Holdout,
            300,
            Some("prompt"),
            "ranked",
            false,
            false,
        ));
    }

    // 1. Draw with trusted OS randomness
    let os_seed = draw_os_seed().expect("drawing OS randomness should succeed");
    let manifest_os = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        40,
        &AllocationMethod::Proportional { min_floor: 2 },
        RandomizationProvenance::OsRandom {
            entropy_source: "/dev/urandom".into(),
            seed: os_seed,
        },
        "policy-test-v1",
        1_700_000_000_000,
    )
    .expect("OS random draw should succeed");

    assert_eq!(manifest_os.total_frame_families, 100);
    assert_eq!(manifest_os.total_sampled_families, 40);
    assert_eq!(
        manifest_os.design_status,
        DesignStatus::StratifiedProbabilitySample
    );
    assert_eq!(
        manifest_os.estimand_weighting,
        EstimandWeighting::FamilyWeighted
    );

    // Verify inclusion probabilities are present and mathematically consistent
    for entry in &manifest_os.selected_cases {
        let prob = entry
            .inclusion_probability
            .expect("must have inclusion prob");
        assert!(prob > 0.0 && prob <= 1.0);
        let alloc = &manifest_os.strata[&entry.stratum_key];
        let expected_prob = (alloc.sample_size as f64) / (alloc.population_size as f64);
        assert!((prob - expected_prob).abs() < 1e-12);
    }

    // 2. Draw with manual diagnostic seed
    let manifest_manual = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        40,
        &AllocationMethod::Proportional { min_floor: 2 },
        RandomizationProvenance::SuppliedManual { seed: 42 },
        "policy-test-v1",
        1_700_000_000_000,
    )
    .expect("manual draw should succeed");

    // Manual seed is diagnostic: do NOT assert design-based coverage
    assert_eq!(manifest_manual.design_status, DesignStatus::DiagnosticFixed);
    for entry in &manifest_manual.selected_cases {
        assert!(
            entry.inclusion_probability.is_none(),
            "diagnostic manual seed must not fabricate design inclusion probabilities"
        );
    }

    // 3. Complete Census
    let manifest_census = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        100,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::Census,
        "policy-test-v1",
        1_700_000_000_000,
    )
    .expect("census draw should succeed");

    assert_eq!(manifest_census.design_status, DesignStatus::FullCensus);
    assert_eq!(manifest_census.total_sampled_families, 100);
    for entry in &manifest_census.selected_cases {
        assert_eq!(entry.inclusion_probability, Some(1.0));
    }
}

#[test]
fn test_sampling_uniqueness_membership_and_deterministic_replay() {
    let mut reps = Vec::new();
    for i in 0..50 {
        reps.push(make_test_case(
            &format!("fam-{i:03}"),
            "case-1",
            1,
            EvaluationSplit::Holdout,
            if i % 2 == 0 { 50 } else { 350 },
            Some("prompt"),
            "ranked",
            false,
            false,
        ));
    }

    let seed = 123456789u64;
    let manifest1 = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        25,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed },
        "policy-deterministic",
        1_700_000_000_000,
    )
    .expect("draw 1 should succeed");

    let manifest2 = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        25,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed },
        "policy-deterministic",
        1_700_000_000_000,
    )
    .expect("draw 2 should succeed");

    // Exact deterministic identity
    assert_eq!(manifest1.manifest_id, manifest2.manifest_id);
    assert_eq!(manifest1.frame_digest, manifest2.frame_digest);
    assert_eq!(manifest1.selected_cases, manifest2.selected_cases);

    // Verify all selected cases are unique
    let mut seen = std::collections::BTreeSet::new();
    for entry in &manifest1.selected_cases {
        assert!(
            seen.insert(&entry.case_key.family_id),
            "no duplicate family"
        );
    }
    assert_eq!(seen.len(), 25);

    // Verify manifest against frame
    verify_manifest_against_frame(&manifest1, &reps).expect("manifest should verify against frame");

    // Replay sample from frame
    let replayed =
        replay_manifest_sample(&manifest1, &reps).expect("replay manifest sample should succeed");
    assert_eq!(replayed.len(), 25);
    for (entry, rep_case) in manifest1.selected_cases.iter().zip(replayed.iter()) {
        assert_eq!(entry.case_key, rep_case.key);
    }
}

#[test]
fn test_changed_frame_rejection() {
    let mut reps = Vec::new();
    for i in 0..30 {
        reps.push(make_test_case(
            &format!("fam-{i:03}"),
            "case-1",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("prompt"),
            "ranked",
            false,
            false,
        ));
    }

    let manifest = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        15,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed: 999 },
        "policy-1",
        1_700_000_000_000,
    )
    .expect("draw should succeed");

    // Verify against unmodified frame passes
    assert!(verify_manifest_against_frame(&manifest, &reps).is_ok());

    // 1. Frame changed by altering a case key
    let mut modified_reps = reps.clone();
    modified_reps[0].key.case_id = "different-case-id".to_string();
    let err1 = verify_manifest_against_frame(&manifest, &modified_reps).unwrap_err();
    assert!(matches!(err1, EvaluationError::FrameDigestMismatch { .. }));

    // 2. Frame changed by adding a case
    let mut enlarged_reps = reps.clone();
    enlarged_reps.push(make_test_case(
        "fam-extra",
        "case-1",
        1,
        EvaluationSplit::Holdout,
        50,
        Some("prompt"),
        "ranked",
        false,
        false,
    ));
    let err2 = verify_manifest_against_frame(&manifest, &enlarged_reps).unwrap_err();
    assert!(matches!(err2, EvaluationError::FrameDigestMismatch { .. }));

    // 3. Replay against altered frame fails
    let err3 = replay_manifest_sample(&manifest, &enlarged_reps).unwrap_err();
    assert!(matches!(err3, EvaluationError::FrameDigestMismatch { .. }));
}

#[test]
fn test_json_roundtrip_and_schema_fidelity() {
    let reps = vec![
        make_test_case(
            "fam-1",
            "c1",
            1,
            EvaluationSplit::Validation,
            100,
            Some("prompt 1"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-2",
            "c2",
            1,
            EvaluationSplit::Validation,
            100,
            Some("prompt 2"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-3",
            "c3",
            1,
            EvaluationSplit::Validation,
            300,
            None,
            "abstain",
            true,
            false,
        ),
        make_test_case(
            "fam-4",
            "c4",
            1,
            EvaluationSplit::Validation,
            300,
            None,
            "abstain",
            true,
            false,
        ),
    ];

    let manifest = draw_stratified_sample(
        &reps,
        EvaluationSplit::Validation,
        2,
        &AllocationMethod::EqualPerStratum { min_floor: 1 },
        RandomizationProvenance::OsRandom {
            entropy_source: "/dev/urandom".into(),
            seed: 777,
        },
        "policy-prod",
        1_700_000_123_456,
    )
    .expect("draw should succeed");

    assert_eq!(
        manifest.design_status,
        DesignStatus::StratifiedProbabilitySample
    );
    let json_str = serde_json::to_string_pretty(&manifest).expect("serialize to json");
    assert!(json_str.contains("\"schema_version\": 1"));
    assert!(json_str.contains("\"manifest_id\":"));
    assert!(json_str.contains("\"stratified-probability-sample\""));
    assert!(json_str.contains("\"os-random\""));

    let deserialized: FrozenSampleManifest =
        serde_json::from_str(&json_str).expect("deserialize from json");
    assert_eq!(manifest, deserialized);

    // Also verify FullCensus JSON roundtrip
    let manifest_census = draw_stratified_sample(
        &reps,
        EvaluationSplit::Validation,
        4,
        &AllocationMethod::EqualPerStratum { min_floor: 1 },
        RandomizationProvenance::Census,
        "policy-prod",
        1_700_000_123_456,
    )
    .expect("census draw should succeed");

    assert_eq!(manifest_census.design_status, DesignStatus::FullCensus);
    let json_census = serde_json::to_string_pretty(&manifest_census).expect("serialize census");
    assert!(json_census.contains("\"full-census\""));
    assert!(json_census.contains("\"census\""));
    let deserialized_census: FrozenSampleManifest =
        serde_json::from_str(&json_census).expect("deserialize census");
    assert_eq!(manifest_census, deserialized_census);
}

#[test]
fn test_census_provenance_requires_full_census_allocation() {
    let reps = vec![
        make_test_case(
            "fam-1",
            "c1",
            1,
            EvaluationSplit::Holdout,
            100,
            Some("p1"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-2",
            "c2",
            1,
            EvaluationSplit::Holdout,
            100,
            Some("p2"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-3",
            "c3",
            1,
            EvaluationSplit::Holdout,
            100,
            Some("p3"),
            "ranked",
            false,
            false,
        ),
    ];

    // Attempting Census provenance with partial sample size (2 out of 3) must fail
    let err = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        2,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::Census,
        "policy-test",
        1_700_000_000_000,
    )
    .unwrap_err();

    assert!(
        matches!(err, EvaluationError::SamplingFailure(ref msg) if msg.contains("census provenance requires full census allocation")),
        "census with partial budget must be rejected: {err:?}"
    );

    // With full budget (3 out of 3), Census provenance succeeds and yields FullCensus
    let ok = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        3,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::Census,
        "policy-test",
        1_700_000_000_000,
    )
    .expect("full census should succeed");

    assert_eq!(ok.design_status, DesignStatus::FullCensus);
    assert_eq!(ok.total_sampled_families, 3);
}

#[test]
fn test_manifest_tampering_and_verification_integrity() {
    let reps = vec![
        make_test_case(
            "fam-1",
            "c1",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("p1"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-2",
            "c2",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("p2"),
            "ranked",
            false,
            false,
        ),
    ];

    let manifest = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        2,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::OsRandom {
            entropy_source: "/dev/urandom".into(),
            seed: 42,
        },
        "policy-test",
        1_700_000_000_000,
    )
    .expect("draw should succeed");

    assert!(verify_manifest_against_frame(&manifest, &reps).is_ok());

    // 1. Tampering with manifest_id causes verification failure
    let mut tampered_id = manifest.clone();
    tampered_id.manifest_id = "man-0000000000000000".to_string();
    let err_id = verify_manifest_against_frame(&tampered_id, &reps).unwrap_err();
    assert!(
        matches!(err_id, EvaluationError::ManifestVerificationFailure(ref msg) if msg.contains("manifest ID mismatch")),
        "tampered manifest ID must be rejected: {err_id:?}"
    );

    // 2. Tampering with sampling algorithm version causes verification failure
    let mut tampered_ver = manifest.clone();
    tampered_ver.sampling_algorithm_version = "v999.0".to_string();
    tampered_ver.manifest_id = format!("man-{}", &tampered_ver.compute_manifest_digest()[..16]);
    let err_ver = verify_manifest_against_frame(&tampered_ver, &reps).unwrap_err();
    assert!(
        matches!(err_ver, EvaluationError::ManifestVerificationFailure(ref msg) if msg.contains("algorithm version")),
        "unmatched sampling algorithm version must be rejected: {err_ver:?}"
    );

    // 3. Tampering with selected case replicate causes verification failure
    let mut tampered_rep = manifest.clone();
    tampered_rep.selected_cases[0].case_key.replicate = 999;
    tampered_rep.manifest_id = format!("man-{}", &tampered_rep.compute_manifest_digest()[..16]);
    let err_rep = verify_manifest_against_frame(&tampered_rep, &reps).unwrap_err();
    assert!(
        matches!(err_rep, EvaluationError::ManifestVerificationFailure(ref msg) if msg.contains("selected case key mismatch")),
        "tampered case replicate must be rejected: {err_rep:?}"
    );

    // 4. Tampering with inclusion probability on an entry causes verification failure
    let mut tampered_prob = manifest.clone();
    tampered_prob.selected_cases[0].inclusion_probability = Some(0.123);
    tampered_prob.manifest_id = format!("man-{}", &tampered_prob.compute_manifest_digest()[..16]);
    let err_prob = verify_manifest_against_frame(&tampered_prob, &reps).unwrap_err();
    assert!(
        matches!(err_prob, EvaluationError::ManifestVerificationFailure(ref msg) if msg.contains("inclusion probability")),
        "tampered entry inclusion probability must be rejected: {err_prob:?}"
    );

    // 5. DiagnosticFixed design asserting inclusion probability causes verification failure
    let manual_manifest = draw_stratified_sample(
        &reps,
        EvaluationSplit::Holdout,
        1,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed: 99 },
        "policy-test",
        1_700_000_000_000,
    )
    .expect("manual draw should succeed");
    assert_eq!(manual_manifest.design_status, DesignStatus::DiagnosticFixed);

    let mut tampered_diag = manual_manifest.clone();
    tampered_diag.selected_cases[0].inclusion_probability = Some(1.0);
    tampered_diag.manifest_id = format!("man-{}", &tampered_diag.compute_manifest_digest()[..16]);
    let err_diag = verify_manifest_against_frame(&tampered_diag, &reps).unwrap_err();
    assert!(
        matches!(err_diag, EvaluationError::ManifestVerificationFailure(ref msg) if msg.contains("diagnostic")),
        "diagnostic fixed design asserting inclusion probability must be rejected: {err_diag:?}"
    );
}

#[test]
fn test_frame_digest_sensitivity_to_stratum_and_delimiters() {
    // Case with non-empty prompt summary: stratum is normal:complete
    let case_complete = make_test_case(
        "fam-1",
        "c1",
        1,
        EvaluationSplit::Holdout,
        50,
        Some("valid prompt text"),
        "ranked",
        false,
        false,
    );

    // Case with empty prompt summary: stratum is normal:degraded
    let case_degraded = make_test_case(
        "fam-1",
        "c1",
        1,
        EvaluationSplit::Holdout,
        50,
        Some(""),
        "ranked",
        false,
        false,
    );

    // Both have prompt_summary.is_some() == true, but belong to different strata!
    assert_eq!(
        ObservableStratum::classify(&case_complete).key(),
        "normal:complete"
    );
    assert_eq!(
        ObservableStratum::classify(&case_degraded).key(),
        "normal:degraded"
    );

    let digest_complete = compute_frame_digest(&[case_complete]);
    let digest_degraded = compute_frame_digest(&[case_degraded]);

    assert_ne!(
        digest_complete, digest_degraded,
        "frame digest must be sensitive to stratum classification, not just Option::is_some"
    );

    // Delimiter collision test:
    // Case A: frame_id = "frame:part1", family_id = "part2"
    // Case B: frame_id = "frame", family_id = "part1:part2"
    let mut case_a = make_test_case(
        "part2",
        "c1",
        1,
        EvaluationSplit::Holdout,
        50,
        Some("p"),
        "ranked",
        false,
        false,
    );
    case_a.key.frame_id = "frame:part1".to_string();

    let mut case_b = make_test_case(
        "part1:part2",
        "c1",
        1,
        EvaluationSplit::Holdout,
        50,
        Some("p"),
        "ranked",
        false,
        false,
    );
    case_b.key.frame_id = "frame".to_string();

    let digest_a = compute_frame_digest(&[case_a]);
    let digest_b = compute_frame_digest(&[case_b]);

    assert_ne!(
        digest_a, digest_b,
        "length-prefixed frame digest must prevent delimiter collisions"
    );
}

#[test]
fn test_manifest_preserves_and_verifies_representative_rule() {
    let cases = vec![
        make_test_case(
            "fam-1",
            "case-b",
            1,
            EvaluationSplit::Holdout,
            50,
            Some("p"),
            "ranked",
            false,
            false,
        ),
        make_test_case(
            "fam-1",
            "case-a",
            2,
            EvaluationSplit::Holdout,
            50,
            Some("p"),
            "ranked",
            false,
            false,
        ),
    ];

    // Under LowestReplicateFirst, case-b (replicate 1) is selected over case-a (replicate 2)
    let reps_lowest = select_family_representatives(
        &cases,
        EvaluationSplit::Holdout,
        FamilyRepresentativeRule::LowestReplicateFirst,
    )
    .expect("select reps");
    assert_eq!(reps_lowest[0].key.case_id, "case-b");

    let manifest_lowest = draw_stratified_sample_with_rule(
        &reps_lowest,
        EvaluationSplit::Holdout,
        1,
        &AllocationMethod::Proportional { min_floor: 1 },
        RandomizationProvenance::SuppliedManual { seed: 123 },
        FamilyRepresentativeRule::LowestReplicateFirst,
        "policy-rule-test",
        1_700_000_000_000,
    )
    .expect("draw with LowestReplicateFirst");

    assert_eq!(
        manifest_lowest.representative_rule,
        FamilyRepresentativeRule::LowestReplicateFirst
    );

    // Verifying against the raw cases should succeed because verify uses manifest.representative_rule
    verify_manifest_against_frame(&manifest_lowest, &cases)
        .expect("verification with recorded rule must succeed");

    let replayed = replay_manifest_sample(&manifest_lowest, &cases)
        .expect("replay with recorded rule must succeed");
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].key.case_id, "case-b");
    assert_eq!(replayed[0].key.replicate, 1);
}
