//! CLI contract tests for `sr eval`.
//!
//! Satisfies contract requirements for evaluation batch execution via the CLI:
//! - `sr eval --help` returns 0 and prints usage.
//! - Capabilities registry reflects `eval` as `implemented`.
//! - Missing `--dataset` fails with exit code 2.
//! - Offline batch execution over recorded replay cases outputs a valid report artifact with exit code 0.
//! - `--online` ranks labeled cases live under `--max-requests` and runtime caps, after a
//!   network-free disclosure preflight, and scores baselines from the same answers.
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
fn a_live_batch_needs_labels_consent_a_cap_and_a_key_before_reading_input() {
    let root = temp_root();
    let dataset_file = root.join("workspace/dataset.jsonl");
    fs::write(&dataset_file, format!("{}\n", make_replay_case_json())).unwrap();
    let dataset = dataset_file.to_str().unwrap();
    // Every refusal names its cause before any input is opened: the frame and
    // label paths do not exist.
    for (options, code, kind) in [
        (
            vec!["--online", "--allow-network", "--max-requests", "4"],
            2,
            "invalid-usage",
        ),
        (
            vec!["--labels", "/missing", "--max-requests", "4"],
            2,
            "invalid-usage",
        ),
        (
            vec!["--labels", "/missing", "--online", "--max-requests", "4"],
            8,
            "network-denied",
        ),
        (
            vec!["--labels", "/missing", "--online", "--allow-network"],
            2,
            "invalid-usage",
        ),
        (
            vec![
                "--labels",
                "/missing",
                "--online",
                "--allow-network",
                "--max-requests",
                "0",
            ],
            2,
            "invalid-usage",
        ),
        (
            vec![
                "--labels",
                "/missing",
                "--online",
                "--allow-network",
                "--max-requests",
                "4",
            ],
            4,
            "credential-absent",
        ),
    ] {
        let mut args = vec!["eval", "--dataset", "/missing", "--json"];
        args.extend(options);
        let out = run_sr(&root, &args);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {out:?}");
        // Usage errors from argument parsing are plain; typed refusals are JSON.
        if code != 2 {
            let error: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(error["error"]["kind"], kind, "{args:?}: {error}");
        }
    }
    // Offline replay of a recorded dataset still runs.
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
    // Nothing is drawn, so no seed is read or recorded.
    assert_eq!(
        census["sample_manifest"]["randomization_provenance"]["source"],
        "census"
    );
    assert!(census["sample_manifest"]["randomization_provenance"]["seed"].is_null());
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
    // A weighted design never presents ratio estimators as unbiased means.
    assert!(text.contains("non-linear ratio estimators"), "{text}");
    assert!(text.contains("not a passed quality gate"), "{text}");
}

/// The loopback TLS Jev fixture (synthetic data only), as rank's real tests use it.
struct LiveProvider {
    child: std::process::Child,
    lines: std::io::BufReader<std::process::ChildStdout>,
    port: u16,
}

impl LiveProvider {
    fn start(root: &std::path::Path, scenario: &str) -> Self {
        use std::io::BufRead;
        let dir = root.join("provider");
        fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in [
            (
                "provider_server.py",
                &include_bytes!("fixtures/jev-tls/provider_server.py")[..],
            ),
            (
                "server.pem",
                &include_bytes!("fixtures/jev-tls/server.pem")[..],
            ),
            (
                "server.key",
                &include_bytes!("fixtures/jev-tls/server.key")[..],
            ),
        ] {
            fs::write(dir.join(name), bytes).unwrap();
        }
        let mut child = Command::new("/usr/bin/python3")
            .arg(dir.join("provider_server.py"))
            .arg(scenario)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut hello = String::new();
        lines.read_line(&mut hello).unwrap();
        let hello: Value = serde_json::from_str(&hello).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    /// Every request the provider answered, in order.
    fn finish(mut self) -> Vec<Value> {
        use std::io::BufRead;
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        std::io::Write::write_all(&mut done, b"DONE").unwrap();
        drop(done);
        let mut served = Vec::new();
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "provider ended early"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                break;
            }
            if value["handshake_rejected"] != true {
                served.push(value);
            }
        }
        let _ = self.child.wait();
        served
    }
}

impl Drop for LiveProvider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn live_workspace() -> PathBuf {
    let root = temp_root();
    for (name, description) in [
        ("alpha", "Runs and repairs failing rust tests."),
        ("beta", "Drafts release notes from git history."),
    ] {
        let dir = root.join("workspace/.claude/skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("fixture-ca.pem"),
        include_bytes!("fixtures/jev-tls/ca.pem"),
    )
    .unwrap();
    root
}

fn run_live(root: &PathBuf, port: u16, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("TYPESAFE_API_KEY", "synthetic-eval-canary")
        .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
        .env("SSL_CERT_FILE", root.join("fixture-ca.pem"))
        .current_dir(root.join("workspace"))
        .args(args)
        .output()
        .unwrap()
}

fn roster_ids(root: &PathBuf) -> Vec<String> {
    let out = run_sr(root, &["roster", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let listing: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut ids: Vec<String> = listing["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skill| skill["skill_id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

/// Live cases, each a normalized single-request context, plus their labels.
fn live_inputs(root: &std::path::Path, labels: &[(&str, &[String])]) -> (String, String) {
    let cases: Vec<Value> = labels
        .iter()
        .map(|(id, _)| {
            serde_json::json!({
                "schema_version": 1,
                "key": {"frame_id": "live-frame", "family_id": format!("fam-{id}"),
                        "case_id": id, "replicate": 0, "policy_id": "deployed"},
                "split": "holdout",
                "context": {
                    "schema_version": 1, "harness": "claude_code", "producer_id": "eval-test",
                    "workspace_root": root.join("workspace").to_str().unwrap(),
                    "session_id": format!("session-{id}"), "agent_id": null, "branch_id": null,
                    "context_epoch": null,
                    "current_request": {"event_id": format!("{id}-request"),
                        "text": "Our rust test suite started failing; find out why.",
                        "attachments_omitted": false, "essential_attachment_missing": false},
                    "events": [], "explicit_skill_references": [], "supplied_loads": []
                }
            })
        })
        .collect();
    let judged: Vec<Value> = labels
        .iter()
        .map(|(id, acceptable)| {
            serde_json::json!({
                "schema_version": 1, "case_id": id, "revision": 1,
                "acceptable_skills": acceptable, "no_skill_needed": acceptable.is_empty(),
                "adjudicator": "judge-a", "created_at_unix_ms": 1_726_700_000_000u64
            })
        })
        .collect();
    (
        write_jsonl(root, "live-cases.jsonl", &cases),
        write_jsonl(root, "live-labels.jsonl", &judged),
    )
}

#[test]
fn a_live_batch_ranks_each_case_fresh_and_accounts_every_attempt() {
    let root = live_workspace();
    let ids = roster_ids(&root);
    assert_eq!(ids.len(), 2);
    let (cases, labels) = live_inputs(&root, &[("case-a", &ids[..1]), ("case-b", &ids[1..])]);
    let provider = LiveProvider::start(&root, "useful");
    let out = run_live(
        &root,
        provider.port,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--online",
            "--allow-network",
            "--max-requests",
            "8",
            "--explain",
            "--json",
        ],
    );
    let served = provider.finish();
    let report = json_report(&out);
    assert_eq!(report["evidence_origin"], "live");
    assert_eq!(report["run_status"], "complete", "{report}");
    assert_eq!(report["gate_status"], "not-established");
    assert_eq!(report["completeness"]["cases_requested"], 2);
    assert_eq!(report["completeness"]["cases_completed"], 2);
    // Both cases were ranked fresh, and the report's accounting is exactly
    // what the provider served: wide and rerank for each.
    assert_eq!(served.len(), 4, "{served:?}");
    assert_eq!(report["accounting"]["http_attempts"], 4);
    assert_eq!(report["accounting"]["requests"], 4);
    assert_eq!(report["accounting"]["input_tokens"], 2 * (100 + 120));
    assert_eq!(report["accounting"]["output_tokens"], 2 * (25 + 30));
    assert_eq!(report["metrics"]["judged_cases"], 2);
    for case in report["cases"].as_array().unwrap() {
        assert_eq!(case["status"]["status"], "completed", "{case}");
        assert_eq!(case["status"]["decision"], "ranked", "{case}");
    }
    // Exactly one label can match the provider's favored first option, so
    // one case is a hit and the other a wrong suggestion: loss 0 + 2.
    assert_eq!(report["loss_summary"]["total_loss"], 2, "{report}");
    assert!(
        report["explanation"]["assumptions"]
            .to_string()
            .contains("evaluation_policy.v1")
    );
    // Both cases were previewed with no network before the first send; the
    // provider's exact request count above proves the previews sent nothing.
    let frozen = &report["disclosure_preflight"];
    assert_eq!(frozen["cases_checked"], 2, "{frozen}");
    assert_eq!(frozen["cases_refused"], 0);
    assert!(frozen["disclosed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(frozen["receipts_digest"].as_str().unwrap().len(), 64);
    // The same answers score the baselines; no extra request was made.
    let baselines = &report["baselines"];
    let names: Vec<&str> = baselines["policies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["policy"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "quill-only",
            "choice-only",
            "fit-only",
            "cookbook-approx",
            "blend"
        ],
        "{baselines}"
    );
    // Quill ranked the two-skill roster locally for every case.
    assert_eq!(
        baselines["policies"][0]["not_evaluated_cases"], 0,
        "{baselines}"
    );
    assert_eq!(
        baselines["coverage"]["admitted"]["denominator"], 2,
        "{baselines}"
    );
    assert_eq!(baselines["coverage"]["admitted"]["successes"], 2);
    assert_eq!(baselines["cases_without_evidence"], 0);
}

#[test]
fn a_preview_that_ends_locally_is_counted_as_sending_nothing() {
    let root = live_workspace();
    let ids = roster_ids(&root);
    let (cases, labels) = live_inputs(&root, &[("local", &ids[..1]), ("sent", &ids[..1])]);
    // "local" names its skill explicitly, so its run ends before any request.
    let rows: Vec<Value> = fs::read_to_string(&cases)
        .unwrap()
        .lines()
        .map(|line| {
            let mut row: Value = serde_json::from_str(line).unwrap();
            if row["key"]["case_id"] == "local" {
                row["context"]["explicit_skill_references"] = serde_json::json!([ids[0]]);
            }
            row
        })
        .collect();
    let cases = write_jsonl(&root, "live-cases.jsonl", &rows);
    let provider = LiveProvider::start(&root, "useful");
    let out = run_live(
        &root,
        provider.port,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--online",
            "--allow-network",
            "--max-requests",
            "8",
            "--json",
        ],
    );
    let served = provider.finish();
    let report = json_report(&out);
    // Only the advisory case reached the provider.
    assert_eq!(served.len(), 2, "{served:?}");
    let frozen = &report["disclosure_preflight"];
    assert_eq!(frozen["cases_checked"], 2, "{frozen}");
    assert_eq!(frozen["cases_refused"], 0, "{frozen}");
    // Its preview's null disclosure is not a receipt.
    assert_eq!(frozen["cases_without_request"], 1, "{frozen}");
}

#[test]
fn the_request_cap_admits_a_case_only_while_its_worst_case_fits() {
    let root = live_workspace();
    let ids = roster_ids(&root);
    let (cases, labels) = live_inputs(
        &root,
        &[
            ("case-a", &ids[..1]),
            ("case-b", &ids[..1]),
            ("case-c", &ids[..1]),
        ],
    );
    let provider = LiveProvider::start(&root, "useful");
    // Four attempts cover exactly one ranking's worst case: the first case
    // runs (two attempts), and no later case can be admitted.
    let out = run_live(
        &root,
        provider.port,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--online",
            "--allow-network",
            "--max-requests",
            "4",
            "--json",
        ],
    );
    let served = provider.finish();
    let report = json_report(&out);
    assert_eq!(served.len(), 2, "{served:?}");
    assert_eq!(report["accounting"]["http_attempts"], 2);
    assert_eq!(report["run_status"], "partial");
    assert_eq!(report["completeness"]["cases_requested"], 3);
    assert_eq!(report["completeness"]["cases_completed"], 1);
    assert_eq!(report["loss_summary"]["unfinished_cases"], 2);
    assert_eq!(report["error"]["kind"], "request-budget");
    let unfinished = report["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["status"]["status"] == "unfinished")
        .count();
    assert_eq!(unfinished, 2);
    // Never-ranked cases carry no loss and are not counted as successes.
    assert_eq!(report["loss_summary"]["attempted_cases"], 1);
}

#[test]
fn a_fatal_authentication_error_stops_the_batch_after_partial_work() {
    let root = live_workspace();
    let ids = roster_ids(&root);
    let (cases, labels) = live_inputs(&root, &[("case-a", &ids[..1]), ("case-b", &ids[..1])]);
    let provider = LiveProvider::start(&root, "unauthorized");
    let out = run_live(
        &root,
        provider.port,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--online",
            "--allow-network",
            "--max-requests",
            "20",
            "--json",
        ],
    );
    let served = provider.finish();
    let report = json_report(&out);
    // Authentication is not retried blindly, and the second case is never sent.
    assert_eq!(served.len(), 1, "{served:?}");
    assert_eq!(report["accounting"]["http_attempts"], 1);
    assert_eq!(report["run_status"], "partial");
    assert_eq!(report["error"]["kind"], "authentication");
    assert_eq!(report["loss_summary"]["operational_failures"], 1);
    assert_eq!(report["loss_summary"]["unfinished_cases"], 1);
}

#[test]
fn a_label_naming_a_skill_off_the_current_roster_is_never_sent() {
    let root = live_workspace();
    let ids = roster_ids(&root);
    let gone =
        vec!["s_0000000000000000000000000000000000000000000000000000000000000000".to_owned()];
    let (cases, labels) = live_inputs(&root, &[("case-a", &ids[..1]), ("case-gone", &gone)]);
    let provider = LiveProvider::start(&root, "useful");
    let out = run_live(
        &root,
        provider.port,
        &[
            "eval",
            "--dataset",
            &cases,
            "--labels",
            &labels,
            "--online",
            "--allow-network",
            "--max-requests",
            "8",
            "--json",
        ],
    );
    let served = provider.finish();
    let report = json_report(&out);
    assert_eq!(served.len(), 2, "only case-a is ranked: {served:?}");
    assert_eq!(report["run_status"], "partial");
    let gone_case = report["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == "case-gone")
        .unwrap();
    assert_eq!(
        gone_case["status"]["status"], "not-estimable",
        "{gone_case}"
    );
    assert!(
        gone_case["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("current roster")
    );
    assert_eq!(report["loss_summary"]["not_estimable_cases"], 1);
}
