//! CLI contract tests for `sr eval`.
//!
//! Satisfies contract requirements for evaluation batch execution via the CLI:
//! - `sr eval --help` returns 0 and prints usage.
//! - Capabilities registry reflects `eval` as `implemented`.
//! - Missing `--dataset` fails with exit code 2.
//! - Offline batch execution over recorded replay cases outputs a valid report artifact with exit code 0.
//! - `--online` live execution without `--allow-network` fails with exit code 8 (network denied).
//! - `--online` live execution without `--max-requests` fails with exit code 2 (invalid usage).

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "sr-eval-cli-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(root.join("workspace")).unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    root
}

fn run_sr(root: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("workspace"))
        .args(args)
        .output()
        .expect("run sr binary")
}

fn make_replay_case_json() -> String {
    let historical: Value =
        serde_json::from_str(include_str!("fixtures/output-ranked.v1.json")).unwrap();
    let candidates: Vec<_> = historical["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skill| {
            serde_json::json!({
                "skill_id": skill["skill_id"],
                "invocation_name": skill["invocation_name"],
                "content_hash": skill["content_hash"],
                "source": "workspace",
                "usage_kind": "workflow"
            })
        })
        .collect();

    let value = serde_json::json!({
        "schema_version": 1,
        "case_id": "cli-eval-case-001",
        "created_at_unix_ms": 1726700000000_u64,
        "manifest": {
            "evidence_origin": "recorded",
            "adapter": "claude_code",
            "stages_recorded": ["wide", "rerank"]
        },
        "captured_request": {
            "candidate_options": candidates
        },
        "recorded_responses": {
            "wide": {
                "choice": "s_01",
                "choices_probability": 0.6,
                "gate_score": 0.8,
                "distribution": [
                    {"option_id": "s_01", "probability": 0.6},
                    {"option_id": "s_02", "probability": 0.3},
                    {"option_id": "__none__", "probability": 0.1}
                ]
            },
            "rerank": {
                "choice": "s_01",
                "choices_probability": 0.6,
                "stated_confidence": 0.8,
                "fits": [{"skill_id": "s_01", "fit": 0.8}, {"skill_id": "s_02", "fit": 0.5}],
                "distribution": [
                    {"option_id": "s_01", "probability": 0.6},
                    {"option_id": "s_02", "probability": 0.3},
                    {"option_id": "__none__", "probability": 0.1}
                ]
            }
        },
        "local_evidence": {
            "as_of_unix_ms": 1726700000000_u64,
            "active_snoozes": [],
            "loaded_references": [],
            "scoring_profile": {
                "gate_threshold": 0.3,
                "fit_threshold": 0.3,
                "w_fit": 1.0,
                "w_prior": 0.0,
                "w_phase": 0.0,
                "top_k": 5
            }
        },
        "historical_decision": historical
    });

    serde_json::to_string(&value).unwrap()
}

#[test]
fn eval_help_flag_succeeds_and_documents_options() {
    let root = temp_root();
    let out = run_sr(&root, &["eval", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sr eval --dataset FILE"));
    assert!(stdout.contains("--online"));
    assert!(stdout.contains("--max-requests"));
    assert!(stdout.contains("--max-runtime-ms"));
}

#[test]
fn capabilities_reports_eval_as_implemented() {
    let root = temp_root();
    let out = run_sr(&root, &["capabilities", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    let commands = val["commands"].as_array().unwrap();
    let eval_entry = commands
        .iter()
        .find(|c| c["name"].as_str() == Some("eval"))
        .expect("eval must be listed in capabilities");
    assert_eq!(eval_entry["status"], "implemented");
}

#[test]
fn eval_without_dataset_fails_with_invalid_usage() {
    let root = temp_root();
    let out = run_sr(&root, &["eval"]);
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout} {stderr}");
    assert!(combined.contains("dataset") || combined.contains("invalid-usage"));
}

#[test]
fn eval_offline_replay_batch_succeeds_with_json_report() {
    let root = temp_root();
    let dataset_file = root.join("workspace/dataset.jsonl");
    fs::write(&dataset_file, format!("{}\n", make_replay_case_json())).unwrap();

    let out = run_sr(
        &root,
        &["eval", "--dataset", dataset_file.to_str().unwrap(), "--json"],
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: Value = serde_json::from_str(stdout.trim()).expect("valid json report document");
    assert_eq!(report["kind"], "report");
    assert_eq!(report["run_status"], "complete");
    assert_eq!(report["evidence_origin"], "recorded");
    assert_eq!(report["completeness"]["cases_requested"], 1);
    assert_eq!(report["completeness"]["cases_completed"], 1);
    assert_eq!(report["accounting"]["http_attempts"], 0);
    assert_eq!(report["accounting"]["requests"], 0);
}

#[test]
fn eval_online_without_network_permission_is_denied() {
    let root = temp_root();
    let dataset_file = root.join("workspace/dataset.jsonl");
    fs::write(&dataset_file, format!("{}\n", make_replay_case_json())).unwrap();

    let out = run_sr(
        &root,
        &[
            "eval",
            "--dataset",
            dataset_file.to_str().unwrap(),
            "--online",
            "--max-requests",
            "10",
            "--json",
        ],
    );
    // Exit code 8 is Privacy/NetworkDenied
    assert_eq!(out.status.code(), Some(8));
}

#[test]
fn eval_online_without_max_requests_fails_with_usage_error() {
    let root = temp_root();
    let dataset_file = root.join("workspace/dataset.jsonl");
    fs::write(&dataset_file, format!("{}\n", make_replay_case_json())).unwrap();

    let out = run_sr(
        &root,
        &[
            "eval",
            "--dataset",
            dataset_file.to_str().unwrap(),
            "--online",
            "--allow-network",
            "--json",
        ],
    );
    // Exit code 2 is InvalidUsage
    assert_eq!(out.status.code(), Some(2));
}
