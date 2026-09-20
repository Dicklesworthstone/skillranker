use skillranker::evaluation::stratified::*;
use skillranker::evaluation::{CaseKey, EvaluationCaseRecord, EvaluationSplit};
use std::collections::BTreeMap;

fn frame() -> Vec<EvaluationCaseRecord> {
    (0..4)
        .map(|i| EvaluationCaseRecord {
            schema_version: 1,
            key: CaseKey::new("frame", format!("family-{i}"), "case", 0, "policy"),
            split: EvaluationSplit::Holdout,
            prompt_summary: Some("bounded synthetic request".into()),
            roster_skills: vec!["skill".into()],
            decision: "abstain".into(),
            suggested_skills: vec![],
            fits: BTreeMap::new(),
            gate_score: None,
            relevance_abstention: true,
            operational_failure: false,
        })
        .collect()
}

fn sample(provenance: RandomizationProvenance, count: usize) -> FrozenSampleManifest {
    draw_stratified_sample(
        &frame(),
        EvaluationSplit::Holdout,
        count,
        &AllocationMethod::Proportional { min_floor: 1 },
        provenance,
        "policy",
        1,
    )
    .unwrap()
}

fn reseal(manifest: &mut FrozenSampleManifest) {
    manifest.manifest_id = format!("man-{}", &manifest.compute_manifest_digest()[..16]);
}

fn rejected(mut manifest: FrozenSampleManifest) {
    // An ordinary unkeyed digest is integrity metadata, not authority. Test
    // semantic validation after the caller has recomputed a consistent digest.
    reseal(&mut manifest);
    assert!(verify_manifest_against_frame(&manifest, &frame()).is_err());
    assert!(replay_manifest_sample(&manifest, &frame()).is_err());
}

#[test]
fn supported_designs_replay_exact_membership_and_order() {
    for (provenance, count) in [
        (RandomizationProvenance::SuppliedManual { seed: 42 }, 2),
        (
            RandomizationProvenance::SuppliedImported {
                seed: 42,
                manifest_digest: "recorded-origin".into(),
            },
            2,
        ),
        (
            RandomizationProvenance::OsRandom {
                seed: 42,
                entropy_source: "recorded-os-source".into(),
            },
            2,
        ),
        (RandomizationProvenance::Census, 4),
    ] {
        let manifest = sample(provenance, count);
        let replay = replay_manifest_sample(&manifest, &frame()).unwrap();
        assert_eq!(replay.len(), count);
        assert!(
            replay
                .iter()
                .zip(&manifest.selected_cases)
                .all(|(case, entry)| case.key == entry.case_key)
        );
    }
}

#[test]
fn supplied_seed_cannot_acquire_probability_design_by_resealing() {
    let original = sample(
        RandomizationProvenance::OsRandom {
            seed: 42,
            entropy_source: "recorded-os-source".into(),
        },
        2,
    );
    for provenance in [
        RandomizationProvenance::SuppliedManual { seed: 42 },
        RandomizationProvenance::SuppliedImported {
            seed: 42,
            manifest_digest: "arbitrary".into(),
        },
    ] {
        let mut manifest = original.clone();
        manifest.randomization_provenance = provenance;
        rejected(manifest);
    }
}

#[test]
fn version_weighting_order_and_stratum_identity_are_validated_semantically() {
    let original = sample(RandomizationProvenance::SuppliedManual { seed: 42 }, 2);
    let mut manifest = original.clone();
    manifest.schema_version += 1;
    rejected(manifest);
    let mut manifest = original.clone();
    manifest.estimand_weighting = EstimandWeighting::TrafficWeighted;
    rejected(manifest);
    let mut manifest = original.clone();
    manifest.selected_cases[0].sample_order = 99;
    rejected(manifest);
    let mut manifest = original;
    manifest.strata.values_mut().next().unwrap().stratum_key = "foreign".into();
    rejected(manifest);
}

#[test]
fn stratum_weights_must_be_finite_and_match_the_family_frame() {
    let original = sample(RandomizationProvenance::SuppliedManual { seed: 42 }, 2);
    for weight in [0.25, -1.0, f64::NAN, f64::INFINITY] {
        let mut manifest = original.clone();
        manifest.strata.values_mut().next().unwrap().weight = weight;
        rejected(manifest);
    }
}

#[test]
fn represented_strata_cannot_be_empty_samples_even_with_consistent_counts() {
    let mut manifest = sample(RandomizationProvenance::SuppliedManual { seed: 42 }, 2);
    manifest.total_sampled_families = 0;
    manifest.selected_cases.clear();
    manifest.strata.values_mut().next().unwrap().sample_size = 0;
    rejected(manifest);
}

#[test]
fn nonfinite_probabilities_never_pass_the_numeric_boundary() {
    for probability in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let mut manifest = sample(
            RandomizationProvenance::OsRandom {
                seed: 42,
                entropy_source: "recorded-os-source".into(),
            },
            2,
        );
        manifest
            .strata
            .values_mut()
            .next()
            .unwrap()
            .inclusion_probability = Some(probability);
        for entry in &mut manifest.selected_cases {
            entry.inclusion_probability = Some(probability);
        }
        rejected(manifest);
    }
}
