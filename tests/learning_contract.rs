#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Evidence-gated learning, centered shrunk usefulness priors, threshold
//! calibration, managed-policy rollback, and composite-risk supermartingale
//! monitoring contracts (Phase P8: sr-roadmap-l1i.9.1..9.8).
//!
//! Satisfies contract boundary `p8_learning_lifecycle` mapped in
//! `tests/contract_matrix.toml`.

mod support;

use skillranker::learning::{
    CalibrationArtifact, CalibrationCandidateInput, CalibrationCaseInput, CalibrationSplit,
    CompositeRiskAccumulator, FinalizedMonitorUnit, LearningError, ManagedRankingPolicy,
    MonitorLedgerState, PriorJudgmentRecord, PriorSnapshot, apply_calibrated_policy,
    beta_shrunk_prior_delta, read_managed_policy_from_toml, rollback_managed_policy,
    run_calibration_from_bytes, verify_supermartingale_ville_bound,
};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

fn sample_calibration_artifact() -> CalibrationArtifact {
    let mut training_judgments = Vec::new();
    for idx in 0..25 {
        training_judgments.push(PriorJudgmentRecord {
            event_id: format!("ev-train-{idx}"),
            skill_id: "sk-useful".into(),
            content_hash: "blake3:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            policy_version: "jev.rerank.v1".into(),
            phase: Some("debugging".into()),
            useful: idx < 22,
            label_source: "independent_judgment".into(),
            judged_at_unix_ms: 1_000 + idx * 10,
        });
    }

    let make_case = |case_id: &str,
                     family_id: &str,
                     split: CalibrationSplit,
                     ts: u64,
                     needs_skill_gate: f64,
                     fit_useful: f64,
                     fit_decoy: f64,
                     acceptable: &[&str],
                     near_miss: bool|
     -> CalibrationCaseInput {
        CalibrationCaseInput {
            case_id: case_id.into(),
            family_id: family_id.into(),
            split,
            timestamp_unix_ms: ts,
            needs_skill_gate,
            stage2_present: true,
            none_probability: 0.10,
            phase: Some("debugging".into()),
            candidates: vec![
                CalibrationCandidateInput {
                    skill_id: "sk-useful".into(),
                    content_hash:
                        "blake3:1111111111111111111111111111111111111111111111111111111111111111"
                            .into(),
                    rerank_probability: 0.42,
                    fit: fit_useful,
                    eligible: true,
                    phase_match: 0.90,
                },
                CalibrationCandidateInput {
                    skill_id: "sk-decoy".into(),
                    content_hash:
                        "blake3:2222222222222222222222222222222222222222222222222222222222222222"
                            .into(),
                    rerank_probability: 0.48,
                    fit: fit_decoy,
                    eligible: true,
                    phase_match: 0.10,
                },
            ],
            acceptable_skills: acceptable.iter().map(|s| (*s).to_owned()).collect(),
            near_miss,
            operationally_unavailable: false,
        }
    };

    let mut cases = Vec::new();
    for (prefix, split, base_ts) in [
        ("tr", CalibrationSplit::Training, 2_000_u64),
        ("va", CalibrationSplit::Validation, 10_000_u64),
        ("ho", CalibrationSplit::Holdout, 20_000_u64),
    ] {
        // Positive case where sk-decoy has slightly higher raw rerank (0.48 vs 0.42)
        // and similar fit (0.55 vs 0.55), so baseline (w_prior=0) picks sk-decoy (loss=2),
        // while enabling shrunk prior (w_prior=0.5) boosts sk-useful to top-1 (loss=0)!
        cases.push(make_case(
            &format!("{prefix}-pos-1"),
            &format!("fam-{prefix}-pos-1"),
            split,
            base_ts + 1,
            0.75,
            0.55,
            0.55,
            &["sk-useful"],
            false,
        ));
        // Negative case with needs_skill = 0.32 (baseline gate 0.30 passes and suggests -> loss 2;
        // raising gate to 0.35 abstains -> loss 0).
        cases.push(make_case(
            &format!("{prefix}-neg-1"),
            &format!("fam-{prefix}-neg-1"),
            split,
            base_ts + 2,
            0.32,
            0.55,
            0.55,
            &[],
            false,
        ));
        // Near-miss case with needs_skill = 0.33 and fit = 0.35 (raising gate to 0.35 abstains -> loss 0).
        cases.push(make_case(
            &format!("{prefix}-near-1"),
            &format!("fam-{prefix}-near-1"),
            split,
            base_ts + 3,
            0.33,
            0.35,
            0.35,
            &[],
            true,
        ));
    }

    CalibrationArtifact {
        schema_version: 1,
        policy_version: "jev.rerank.v1".into(),
        min_split_cases: 3,
        unfinished_cases: 0,
        training_judgments,
        cases,
        candidate_policies: Vec::new(),
    }
}

#[test]
fn calibration_and_supermartingale() {
    // 1. Centered shrunk Beta(1, 4) prior properties (.9.1)
    assert!(
        beta_shrunk_prior_delta(0, 0).unwrap().abs() < 1e-12,
        "unjudged skill must have exact zero prior delta"
    );
    assert!(
        beta_shrunk_prior_delta(20, 20).unwrap() > 0.0,
        "repeatedly judged useful skill must have positive prior delta"
    );
    assert!(
        beta_shrunk_prior_delta(0, 20).unwrap() < 0.0,
        "repeatedly judged non-useful skill must have negative prior delta"
    );

    // Adoption-only provenance is rejected.
    let bad_provenance = vec![PriorJudgmentRecord {
        event_id: "ev-1".into(),
        skill_id: "sk-1".into(),
        content_hash: "blake3:1111".into(),
        policy_version: "jev.rerank.v1".into(),
        phase: None,
        useful: true,
        label_source: "observed_load".into(),
        judged_at_unix_ms: 100,
    }];
    assert!(matches!(
        PriorSnapshot::from_judgments(&bad_provenance, 200),
        Err(LearningError::MalformedInput(_))
    ));

    // Temporal cutoff strictly excludes future labels.
    let mixed_ts = vec![
        PriorJudgmentRecord {
            event_id: "ev-past".into(),
            skill_id: "sk-1".into(),
            content_hash: "blake3:1111".into(),
            policy_version: "jev.rerank.v1".into(),
            phase: Some("debugging".into()),
            useful: true,
            label_source: "independent_judgment".into(),
            judged_at_unix_ms: 100,
        },
        PriorJudgmentRecord {
            event_id: "ev-future".into(),
            skill_id: "sk-1".into(),
            content_hash: "blake3:1111".into(),
            policy_version: "jev.rerank.v1".into(),
            phase: Some("debugging".into()),
            useful: false,
            label_source: "independent_judgment".into(),
            judged_at_unix_ms: 500,
        },
    ];
    let snap_early = PriorSnapshot::from_judgments(&mixed_ts, 200).unwrap();
    assert!(snap_early.prior_delta("sk-1", "blake3:1111", "jev.rerank.v1", None) > 0.0);
    // Different content_hash or policy_version backs off to 0.0.
    assert_eq!(
        snap_early.prior_delta("sk-1", "blake3:changed", "jev.rerank.v1", None),
        0.0
    );

    // 2. Calibration over separated splits and managed policy apply + rollback (.9.2, .9.3, .9.4)
    let artifact = sample_calibration_artifact();
    let bytes = serde_json::to_vec(&artifact).unwrap();
    let report = run_calibration_from_bytes(&bytes).unwrap();
    assert!(report.promotable, "{report:?}");
    assert_eq!(
        report.promotion_reason,
        "held_out_loss_improved_over_baseline"
    );
    assert!(report.selected_holdout.mean_loss < report.baseline_holdout.mean_loss);

    // Verify family split contamination is rejected.
    let mut contaminated = artifact.clone();
    contaminated.cases[3].family_id = contaminated.cases[0].family_id.clone();
    assert!(matches!(
        run_calibration_from_bytes(&serde_json::to_vec(&contaminated).unwrap()),
        Err(LearningError::SplitContamination(_))
    ));

    // Verify atomic apply and conflict-safe rollback preserve unrelated sections.
    let root = support::private_store_dir("p8-learning");
    let config_dir = root.join("config/sr");
    let state_dir = root.join("state");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&config_dir)
        .unwrap();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state_dir)
        .unwrap();
    let config_path = config_dir.join("config.toml");
    let initial_toml = "[network]\nenabled = true\n\n[hook]\nmode = \"advisory\"\n\n[roster]\nexcluded_skills = [\"sk-banned\"]\n";
    std::fs::write(&config_path, initial_toml).unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let applied = apply_calibrated_policy(report.clone(), &config_path, &state_dir).unwrap();
    assert!(applied.applied);
    assert!(
        state_dir
            .join("policy-revisions")
            .join(format!("{}.bak.toml", applied.revision_id))
            .exists(),
        "expected owner-only .bak.toml backup in policy-revisions"
    );
    let active_snap = skillranker::learning::load_active_prior_snapshot(&state_dir)
        .unwrap()
        .expect("expected active prior_snapshot.json when w_prior > 0");
    assert!(
        active_snap.prior_delta(
            "sk-useful",
            "blake3:1111111111111111111111111111111111111111111111111111111111111111",
            "jev.rerank.v1",
            Some("debugging")
        ) > 0.45,
        "persisted active prior snapshot must reproduce positive shrunk delta"
    );
    let (after_apply_table, after_apply_policy) = read_managed_policy_from_toml(&config_path).unwrap();
    assert_eq!(after_apply_policy, report.selected_policy);
    assert!(
        after_apply_table["ranking"].get("gate").is_some()
            && after_apply_table["ranking"].get("fits").is_some(),
        "config.toml [ranking] must use canonical KEY_SPECS names gate and fits"
    );
    assert_eq!(
        after_apply_table["network"]["enabled"].as_bool(),
        Some(true)
    );
    assert_eq!(
        after_apply_table["hook"]["mode"].as_str(),
        Some("advisory")
    );

    // Preview rollback without mutating config.toml.
    let preview_rb =
        rollback_managed_policy(&applied.revision_id, false, &config_path, &state_dir).unwrap();
    assert!(!preview_rb.applied);
    assert_eq!(preview_rb.restored_policy, ManagedRankingPolicy::BASELINE);

    // Apply rollback and verify exact restoration of baseline managed fields while keeping [network], [hook], [roster].
    let applied_rb =
        rollback_managed_policy(&applied.revision_id, true, &config_path, &state_dir).unwrap();
    assert!(applied_rb.applied);
    assert!(
        skillranker::learning::load_active_prior_snapshot(&state_dir)
            .unwrap()
            .is_none(),
        "rolling back to baseline (w_prior=0) must clear active prior snapshot"
    );
    let (after_rb_table, after_rb_policy) = read_managed_policy_from_toml(&config_path).unwrap();
    assert_eq!(after_rb_policy, ManagedRankingPolicy::BASELINE);
    assert_eq!(after_rb_table["network"]["enabled"].as_bool(), Some(true));
    assert_eq!(after_rb_table["hook"]["mode"].as_str(), Some("advisory"));
    assert_eq!(
        after_rb_table["roster"]["excluded_skills"][0].as_str(),
        Some("sk-banned")
    );

    // Second rollback on already-rolled-back config must detect digest conflict and refuse.
    assert!(matches!(
        rollback_managed_policy(&applied.revision_id, true, &config_path, &state_dir),
        Err(LearningError::PolicyConflict(_))
    ));

    // 3. Composite-risk supermartingale Ville's inequality & nonrefundable alpha (.9.5, .9.6, .9.7)
    let (crossing_prob, holds) = verify_supermartingale_ville_bound(14, 0.02, 0.05).unwrap();
    assert!(
        holds && crossing_prob <= 0.05,
        "Ville's inequality violated: crossing_prob={crossing_prob}"
    );

    // Verify alarm under elevated composite risk (e.g. 3 consecutive flagged units at tau=0.02, alpha=0.05 -> threshold=20).
    let mut acc = CompositeRiskAccumulator::new(0.05).unwrap();
    let e1 = acc.observe_unit(true, false);
    let e2 = acc.observe_unit(false, true);
    let e3 = acc.observe_unit(true, false);
    assert!(e1 > 1.0 && e2 > e1 && e3 >= 20.0, "e1={e1}, e2={e2}, e3={e3}");
    assert!(acc.alarm_triggered);
    assert_eq!(acc.judged_harm_units, 2);
    assert_eq!(acc.unresolved_units, 1);

    // Verify MonitorLedgerState prespecified order, post-finalization invalidation, and nonrefundable alpha budget.
    let mut mon = MonitorLedgerState::initialize(
        0.05,
        0.03,
        vec!["u-1".into(), "u-2".into(), "u-3".into()],
    )
    .unwrap();
    // Out-of-order unit is rejected.
    assert!(matches!(
        mon.record_unit(FinalizedMonitorUnit {
            unit_id: "u-2".into(),
            label_revision: 1,
            judged_harm: false,
            unresolved: false,
        }),
        Err(LearningError::MonitorInvalid(_))
    ));
    // In-order unit succeeds.
    mon.record_unit(FinalizedMonitorUnit {
        unit_id: "u-1".into(),
        label_revision: 1,
        judged_harm: false,
        unresolved: false,
    })
    .unwrap();
    // Post-finalization correction to u-1 invalidates the current epoch without refunding the 0.03 alpha.
    mon.record_unit(FinalizedMonitorUnit {
        unit_id: "u-1".into(),
        label_revision: 2,
        judged_harm: true,
        unresolved: false,
    })
    .unwrap();
    assert_eq!(mon.status, "invalidated");
    assert!((mon.remaining_alpha() - 0.02).abs() < 1e-12);
    assert_eq!(mon.historical_epochs.len(), 1);

    // Attempting to restart with duplicate unit IDs is rejected.
    assert!(matches!(
        mon.start_new_epoch(0.02, vec!["u-4".into(), "u-4".into()]),
        Err(LearningError::MonitorInvalid(_))
    ));
    // Attempting to restart with 0.03 exceeds remaining 0.02 alpha budget.
    assert!(matches!(
        mon.start_new_epoch(0.03, vec!["u-4".into()]),
        Err(LearningError::AlphaBudgetExhausted { .. })
    ));
    // Restarting with remaining 0.02 succeeds and advances to epoch 2.
    mon.start_new_epoch(0.02, vec!["u-4".into()]).unwrap();
    assert_eq!(mon.current_epoch, 2);
    assert_eq!(mon.status, "active");
    assert!(mon.remaining_alpha() < 1e-12);
}
