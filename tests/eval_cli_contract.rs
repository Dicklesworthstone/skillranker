//! CLI contract tests for `sr eval`.
//!
//! Satisfies contract requirements for evaluation batch execution via the CLI:
//! - `sr eval --help` returns 0 and prints usage.
//! - Capabilities registry reflects `eval` as `implemented`.
//! - Missing `--dataset` fails with exit code 2.
//! - Offline batch execution over recorded replay cases outputs a valid report artifact with exit code 0.
//! - `--online` and `--max-requests` are planned flags: refused with exit code 2 naming the phase.
//! - `--labels` scores a labeled case frame; `--sample-size`/`--seed` freeze a sample first.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("workspace"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run sr binary");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("poll sr").is_some() {
            return child.wait_with_output().expect("collect sr output");
        }
        if Instant::now() >= deadline {
            child.kill().expect("terminate stalled sr");
            let output = child.wait_with_output().expect("reap stalled sr");
            panic!("sr blocked on evaluation input: {:?}", output.status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
    assert!(!stdout.contains("--online"));
    assert!(!stdout.contains("--max-requests"));
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
        &[
            "eval",
            "--dataset",
            dataset_file.to_str().unwrap(),
            "--json",
        ],
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
fn eval_online_is_a_planned_flag_refused_before_reading_the_dataset() {
    let root = temp_root();
    let dataset_file = root.join("workspace/dataset.jsonl");
    fs::write(&dataset_file, format!("{}\n", make_replay_case_json())).unwrap();
    let dataset = dataset_file.to_str().unwrap();
    for (options, flag) in [
        (
            vec!["--online", "--allow-network", "--max-requests", "1"],
            "--online",
        ),
        (vec!["--online"], "--online"),
        (vec!["--max-requests", "1"], "--max-requests"),
    ] {
        let mut args = vec!["eval", "--dataset", dataset, "--json"];
        args.extend(options);
        let out = run_sr(&root, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["error"]["kind"], "invalid-usage");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(flag) && message.contains("P5") && message.contains("sr capabilities"),
            "{message}"
        );
    }
    // Offline replay of the same dataset still runs.
    let out = run_sr(&root, &["eval", "--dataset", dataset, "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[cfg(unix)]
#[test]
fn eval_rejects_nonregular_datasets_without_waiting_for_a_writer() {
    let root = temp_root();
    let fifo = root.join("workspace/dataset.fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create real FIFO without a writer");
    let escaped = root.join("outside.jsonl");
    fs::write(&escaped, make_replay_case_json()).unwrap();
    let link = root.join("workspace/escape.jsonl");
    std::os::unix::fs::symlink(&escaped, &link).unwrap();
    for dataset in [
        &fifo,
        &root.join("workspace"),
        &PathBuf::from("/dev/null"),
        &link,
    ] {
        let out = run_sr(
            &root,
            &["eval", "--dataset", dataset.to_str().unwrap(), "--json"],
        );
        assert_eq!(out.status.code(), Some(7), "dataset {dataset:?}: {out:?}");
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["decision"], "unavailable");
        assert_eq!(error["error"]["kind"], "malformed-input");
        assert!(!String::from_utf8_lossy(&out.stdout).contains(dataset.to_str().unwrap()));
    }
}

#[test]
fn eval_validates_mode_and_bounds_before_opening_any_input() {
    let root = temp_root();
    for (options, expected) in [
        (vec!["--online"], 2),
        (vec!["--online", "--allow-network"], 2),
        (vec!["--max-runtime-ms", "0"], 2),
        (vec!["--max-runtime-ms", "86400001"], 2),
        (vec!["--timeout-ms", "0"], 2),
        (vec!["--max-requests", "nonsense"], 2),
        (vec!["--max-requests", "5"], 2),
    ] {
        let mut args = vec![
            "eval",
            "--dataset",
            "missing-dataset",
            "--policy",
            "missing-policy",
            "--json",
        ];
        args.extend(options);
        let out = run_sr(&root, &args);
        assert_eq!(out.status.code(), Some(expected), "{args:?}: {out:?}");
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        let message = error["error"]["message"].as_str().unwrap();
        assert!(!message.contains("missing-dataset") && !message.contains("missing-policy"));
        assert!(!message.contains("No such file"), "{error}");
    }
}

fn frame_case(family: &str, split: &str, decision: &str, suggested: &[&str]) -> Value {
    serde_json::json!({
        "schema_version": 1,
        "key": {
            "frame_id": "frame-1", "family_id": family, "case_id": format!("{family}-case"),
            "replicate": 0, "policy_id": "baseline"
        },
        "split": split,
        "prompt_summary": "Triage a failing test",
        "roster_skills": ["rust-test-triage", "agent-mail", "planner"],
        "decision": decision,
        "suggested_skills": suggested,
    })
}

fn frame_label(family: &str, acceptable: &[&str]) -> Value {
    serde_json::json!({
        "schema_version": 1, "case_id": format!("{family}-case"), "revision": 1,
        "acceptable_skills": acceptable, "no_skill_needed": acceptable.is_empty(),
        "adjudicator": "judge-a", "created_at_unix_ms": 1_726_700_000_000u64
    })
}

fn write_jsonl(root: &std::path::Path, name: &str, rows: &[Value]) -> String {
    let path = root.join("workspace").join(name);
    let body: String = rows.iter().map(|row| format!("{row}\n")).collect();
    fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

fn uniform_frame(root: &std::path::Path, families: usize) -> (String, String) {
    let names: Vec<String> = (0..families).map(|i| format!("fam-{i:02}")).collect();
    let cases: Vec<Value> = names
        .iter()
        .map(|f| frame_case(f, "holdout", "ranked", &["rust-test-triage"]))
        .collect();
    let labels: Vec<Value> = names
        .iter()
        .map(|f| frame_label(f, &["rust-test-triage"]))
        .collect();
    (
        write_jsonl(root, "frame.jsonl", &cases),
        write_jsonl(root, "labels.jsonl", &labels),
    )
}

fn json_report(output: &Output) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("report JSON")
}

fn selected_ids(report: &Value) -> Vec<String> {
    report["sample_manifest"]["selected_cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["case_key"]["case_id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn labeled_frame_scores_the_frozen_loss_and_keeps_unjudged_cases_visible() {
    let root = temp_root();
    let cases = write_jsonl(
        &root,
        "frame.jsonl",
        &[
            frame_case("fam-hit", "holdout", "ranked", &["rust-test-triage"]),
            frame_case("fam-miss", "holdout", "abstain", &[]),
            frame_case("fam-needless", "holdout", "ranked", &["planner"]),
            frame_case("fam-unjudged", "holdout", "ranked", &["planner"]),
        ],
    );
    let labels = write_jsonl(
        &root,
        "labels.jsonl",
        &[
            frame_label("fam-hit", &["rust-test-triage"]),
            frame_label("fam-miss", &["rust-test-triage"]),
            frame_label("fam-needless", &[]),
        ],
    );
    let report = json_report(&run_sr(
        &root,
        &["eval", "--dataset", &cases, "--labels", &labels, "--json"],
    ));
    assert_eq!(report["kind"], "report");
    assert_eq!(report["actionable"], false);
    assert_eq!(report["gate_status"], "not-established");
    // The unjudged case is neither dropped nor scored as a success.
    assert_eq!(report["run_status"], "partial");
    assert_eq!(report["completeness"]["cases_requested"], 4);
    assert_eq!(report["completeness"]["cases_completed"], 3);
    assert_eq!(report["reconciliation"]["reconciled"], true);
    // evaluation_policy.v1: hit 0, false abstention 1, needless suggestion 2.
    assert_eq!(report["loss_summary"]["attempted_cases"], 3);
    assert_eq!(report["loss_summary"]["total_loss"], 3);
    assert_eq!(report["loss_summary"]["not_estimable_cases"], 1);
    assert_eq!(report["metrics"]["judged_cases"], 3);
    assert_eq!(report["metrics"]["unjudged_cases"], 1);
    let losses: Vec<(String, Value)> = report["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                case["family_id"].as_str().unwrap().to_owned(),
                case["status"]["loss"].clone(),
            )
        })
        .collect();
    assert!(losses.contains(&("fam-hit".into(), Value::from(0))));
    assert!(losses.contains(&("fam-miss".into(), Value::from(1))));
    assert!(losses.contains(&("fam-needless".into(), Value::from(2))));
    assert!(losses.contains(&("fam-unjudged".into(), Value::Null)));
    assert!(report.get("sample_manifest").is_none());
}

#[test]
fn a_supplied_seed_reproduces_a_diagnostic_selection_without_design_weights() {
    let root = temp_root();
    let (cases, labels) = uniform_frame(&root, 12);
    let args = [
        "eval",
        "--dataset",
        &cases,
        "--labels",
        &labels,
        "--sample-size",
        "4",
        "--seed",
        "42",
        "--json",
    ];
    let first = json_report(&run_sr(&root, &args));
    let second = json_report(&run_sr(&root, &args));
    assert_eq!(selected_ids(&first).len(), 4);
    assert_eq!(selected_ids(&first), selected_ids(&second));
    assert_eq!(
        first["sample_manifest"]["design_status"],
        "diagnostic-fixed"
    );
    assert_eq!(
        first["sample_manifest"]["randomization_provenance"]["source"],
        "supplied-manual"
    );
    assert!(first.get("design_weighted_loss").is_none());
    assert_eq!(first["completeness"]["cases_requested"], 4);
    assert_eq!(first["run_status"], "complete");

    // Selection precedes the join: changing every label cannot move the draw.
    let flipped: Vec<Value> = (0..12)
        .map(|i| frame_label(&format!("fam-{i:02}"), &[]))
        .collect();
    let flipped = write_jsonl(&root, "flipped.jsonl", &flipped);
    let third = json_report(&run_sr(
        &root,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &flipped,
            "--sample-size",
            "4",
            "--seed",
            "42",
            "--json",
        ],
    ));
    assert_eq!(selected_ids(&first), selected_ids(&third));
    assert_eq!(third["loss_summary"]["total_loss"], 8);
}

#[test]
fn a_fresh_os_seed_is_recorded_and_supports_design_weighted_loss() {
    let root = temp_root();
    let (cases, labels) = uniform_frame(&root, 12);
    let report = json_report(&run_sr(
        &root,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--sample-size",
            "5",
            "--json",
        ],
    ));
    let manifest = &report["sample_manifest"];
    assert_eq!(manifest["design_status"], "stratified-probability-sample");
    assert_eq!(manifest["randomization_provenance"]["source"], "os-random");
    assert!(manifest["randomization_provenance"]["seed"].is_u64());
    assert_eq!(selected_ids(&report).len(), 5);
    let design = &report["design_weighted_loss"];
    assert_eq!(design["total_sampled_cases"], 5);
    assert_eq!(design["total_missing_labels"], 0);
    assert_eq!(design["r_hat_observed"], 0.0);

    // A budget covering the frame is a census with exact weights.
    let census = json_report(&run_sr(
        &root,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--sample-size",
            "99",
            "--json",
        ],
    ));
    assert_eq!(census["sample_manifest"]["design_status"], "full-census");
    assert_eq!(selected_ids(&census).len(), 12);
    assert_eq!(census["design_weighted_loss"]["total_sampled_cases"], 12);
}

#[test]
fn frame_flags_reject_incoherent_combinations_before_reading_inputs() {
    let root = temp_root();
    let (cases, labels) = uniform_frame(&root, 3);
    for args in [
        vec![
            "eval",
            "--dataset",
            "/missing",
            "--sample-size",
            "2",
            "--json",
        ],
        vec![
            "eval",
            "--dataset",
            "/missing",
            "--labels",
            "/missing",
            "--seed",
            "1",
            "--json",
        ],
        vec![
            "eval",
            "--dataset",
            "/missing",
            "--labels",
            "/missing",
            "--policy",
            "p",
            "--json",
        ],
        vec![
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--sample-size",
            "0",
            "--json",
        ],
        vec![
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--sample-size",
            "x",
            "--json",
        ],
    ] {
        let output = run_sr(&root, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    let mixed = write_jsonl(
        &root,
        "mixed.jsonl",
        &[
            frame_case("fam-a", "holdout", "ranked", &["planner"]),
            frame_case("fam-b", "validation", "ranked", &["planner"]),
        ],
    );
    let output = run_sr(
        &root,
        &[
            "eval",
            "--dataset",
            &mixed,
            "--labels",
            &labels,
            "--sample-size",
            "1",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("one split"),
        "{error}"
    );
}

#[test]
fn explain_derives_equations_from_the_report_without_changing_it() {
    let root = temp_root();
    let (cases, _) = uniform_frame(&root, 12);
    // Every label says no skill was needed, so each ranked case is needless (loss 2).
    let needless: Vec<Value> = (0..12)
        .map(|i| frame_label(&format!("fam-{i:02}"), &[]))
        .collect();
    let labels = write_jsonl(&root, "needless.jsonl", &needless);
    let base = [
        "eval",
        "--dataset",
        &cases,
        "--labels",
        &labels,
        "--sample-size",
        "99",
        "--json",
    ];
    let plain = json_report(&run_sr(&root, &base));
    assert!(plain.get("explanation").is_none());
    let mut explained = json_report(&run_sr(&root, &[&base[..], &["--explain"]].concat()));
    let explanation = explained
        .as_object_mut()
        .unwrap()
        .remove("explanation")
        .expect("explanation");
    // Each run records its own time and fresh OS seed; everything else is identical.
    let mut plain = plain;
    for report in [&mut explained, &mut plain] {
        let manifest = &mut report["sample_manifest"];
        for run_specific in [
            "created_at_unix_ms",
            "manifest_id",
            "randomization_provenance",
        ] {
            manifest[run_specific] = Value::Null;
        }
    }
    assert_eq!(explained, plain);

    let quantity = |name: &str| {
        explanation["quantities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {explanation}"))
            .clone()
    };
    assert_eq!(quantity("mean_loss")["substituted"], "24 / 12");
    assert_eq!(quantity("mean_loss")["value"], 2.0);
    assert_eq!(quantity("mean_normalized_loss")["value"], 1.0);
    assert_eq!(quantity("design_weighted_mean_loss")["value"], 1.0);
    assert_eq!(quantity("design_weighted_upper_bound")["value"], 1.0);
    let text = explanation.to_string();
    assert!(text.contains("evaluation_policy.v1"), "{text}");
    assert!(
        text.contains("Every family in the frame was evaluated"),
        "{text}"
    );
    assert!(text.contains("not a passed quality gate"), "{text}");
}
