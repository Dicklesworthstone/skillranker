#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P8 acceptance gate: evidence-gated learning, held-out benefit,
//! atomic managed-policy rollback, and sequential composite-risk monitoring
//! verified through the real `sr calibrate` CLI binary.
//!
//! Satisfies contract boundary `p8_acceptance_gate` (sr-roadmap-l1i.9.9) mapped
//! in `tests/contract_matrix.toml`.

mod support;

use serde_json::{Value, json};
use skillranker::learning::{
    CalibrationArtifact, CalibrationCandidateInput, CalibrationCaseInput, CalibrationSplit,
    FinalizedMonitorUnit, MonitorBatchInput, MonitorRestartStep, PriorJudgmentRecord,
};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output};

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("p8-gate");
        for dir in ["home", "config", "config/sr", "state", "workspace"] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(dir))
                .unwrap();
        }
        let config_path = root.join("config/sr/config.toml");
        let skills_root = root.join("workspace");
        std::fs::write(
            &config_path,
            format!(
                "[network]\nenabled = true\n\n[hook]\nmode = \"advisory\"\n\n[roster]\nroots = [{:?}]\n",
                skills_root.to_str().unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("state"))
            .current_dir(self.root.join("workspace"))
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "args={args:?} exit={:?} stdout={} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn build_eval_artifact() -> CalibrationArtifact {
    let mut training_judgments = Vec::new();
    for idx in 0..25 {
        training_judgments.push(PriorJudgmentRecord {
            event_id: format!("ev-tr-{idx}"),
            skill_id: "sk-target".into(),
            content_hash: "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
                     fit_target: f64,
                     fit_other: f64,
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
                    skill_id: "sk-target".into(),
                    content_hash:
                        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .into(),
                    rerank_probability: 0.42,
                    fit: fit_target,
                    eligible: true,
                    phase_match: 0.90,
                },
                CalibrationCandidateInput {
                    skill_id: "sk-other".into(),
                    content_hash:
                        "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    rerank_probability: 0.48,
                    fit: fit_other,
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
        cases.push(make_case(
            &format!("{prefix}-pos"),
            &format!("fam-{prefix}-pos"),
            split,
            base_ts + 1,
            0.75,
            0.55,
            0.55,
            &["sk-target"],
            false,
        ));
        cases.push(make_case(
            &format!("{prefix}-neg"),
            &format!("fam-{prefix}-neg"),
            split,
            base_ts + 2,
            0.32,
            0.55,
            0.55,
            &[],
            false,
        ));
        cases.push(make_case(
            &format!("{prefix}-near"),
            &format!("fam-{prefix}-near"),
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
fn all_p8_contracts_verified() {
    let home = Home::new();
    let eval_path = home.root.join("workspace/calibration-eval.json");
    let state_dir = home.root.join("state");
    std::fs::write(
        &eval_path,
        serde_json::to_vec_pretty(&build_eval_artifact()).unwrap(),
    )
    .unwrap();

    // Initialize ledger in state_dir so `sr calibrate --apply` also records in `calibrations`.
    let init_res = home.json(&[
        "ledger",
        "init",
        "--dir",
        state_dir.to_str().unwrap(),
        "--json",
    ]);
    assert!(init_res["schema_version"].as_u64().unwrap() >= 1);

    // 1. Preview calibration without mutating config.toml.
    let preview = home.json(&[
        "calibrate",
        "--evaluation",
        eval_path.to_str().unwrap(),
        "--dir",
        state_dir.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(preview["mode"], "preview");
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["promotable"], true);
    let rev_id = preview["revision_id"].as_str().unwrap().to_owned();

    // 2. Apply calibrated policy and verify config.toml is updated atomically AND accepted by `sr doctor --config`.
    let applied = home.json(&[
        "calibrate",
        "--evaluation",
        eval_path.to_str().unwrap(),
        "--dir",
        state_dir.to_str().unwrap(),
        "--apply",
        "--json",
    ]);
    assert_eq!(applied["mode"], "applied");
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["revision_id"], rev_id);

    let doc_cfg = home.json(&["doctor", "--config", "--json"]);
    assert_eq!(doc_cfg["command"], "doctor-config");
    assert_eq!(
        doc_cfg["settings"]["ranking.gate"]["value"],
        applied["selected_policy"]["gate_threshold"]
    );
    assert_eq!(
        doc_cfg["settings"]["ranking.fits"]["value"],
        applied["selected_policy"]["fit_threshold"]
    );
    assert!(
        applied["selected_policy"]["w_prior"].as_f64().unwrap() > 0.0,
        "expected calibrated policy to enable positive w_prior"
    );
    assert_eq!(
        doc_cfg["settings"]["ranking.w_prior"]["value"],
        applied["selected_policy"]["w_prior"]
    );
    assert_eq!(
        doc_cfg["settings"]["ranking.w_prior"]["sources"],
        json!(["trusted-user"])
    );

    let clear_preview = home.json(&[
        "ledger",
        "clear",
        "--dir",
        state_dir.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(clear_preview["calibrations_count"], json!(1));

    // 3. Roll back the applied policy revision and verify exact restoration of baseline
    //    managed fields while preserving [network], [hook], and [roster].
    let rb = home.json(&[
        "calibrate",
        "--rollback",
        &rev_id,
        "--dir",
        state_dir.to_str().unwrap(),
        "--apply",
        "--json",
    ]);
    assert_eq!(rb["mode"], "rollback-applied");
    assert_eq!(rb["applied"], true);
    let preserved: Vec<&str> = rb["preserved_unrelated_sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(preserved.contains(&"network"));
    assert!(preserved.contains(&"hook"));
    assert!(preserved.contains(&"roster"));

    let doc_cfg_after_rb = home.json(&["doctor", "--config", "--json"]);
    assert_eq!(
        doc_cfg_after_rb["settings"]["ranking.fits"]["value"],
        json!(0.3)
    );
    assert_eq!(
        doc_cfg_after_rb["settings"]["ranking.w_prior"]["value"],
        json!(0.0)
    );

    // 4. Run sequential composite-risk monitor with Ville bound verification and nonrefundable restart.
    let mon_path = home.root.join("workspace/monitor-batch.json");
    let mon_input = MonitorBatchInput {
        schema_version: 1,
        alpha_total: 0.05,
        first_epoch_alpha: 0.03,
        prespecified_order: vec!["u-1".into(), "u-2".into()],
        units: vec![
            FinalizedMonitorUnit {
                unit_id: "u-1".into(),
                label_revision: 1,
                judged_harm: false,
                unresolved: false,
            },
            FinalizedMonitorUnit {
                unit_id: "u-2".into(),
                label_revision: 1,
                judged_harm: false,
                unresolved: false,
            },
        ],
        restarts: vec![MonitorRestartStep {
            new_alpha: 0.02,
            prespecified_order: vec!["u-3".into()],
            units: vec![FinalizedMonitorUnit {
                unit_id: "u-3".into(),
                label_revision: 1,
                judged_harm: false,
                unresolved: false,
            }],
        }],
        ville_horizon: Some(12),
    };
    std::fs::write(&mon_path, serde_json::to_vec_pretty(&mon_input).unwrap()).unwrap();
    let mon_out = home.json(&[
        "calibrate",
        "--monitor",
        mon_path.to_str().unwrap(),
        "--dir",
        state_dir.to_str().unwrap(),
        "--apply",
        "--json",
    ]);
    assert_eq!(mon_out["status"], "active");
    assert_eq!(mon_out["current_epoch"], 2);
    assert_eq!(
        mon_out["ville_verification"]["ville_bound_satisfied"],
        json!(true)
    );
    assert!(mon_out["remaining_alpha"].as_f64().unwrap() < 1e-12);
}
