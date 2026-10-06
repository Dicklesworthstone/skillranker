#![cfg(unix)]
//! Contract and integration tests for `--save-case FILE` on `sr rank` (sr-roadmap-l1i.6.15).
//!
//! Validates:
//! 1. `sr rank --help` documents `--save-case FILE`.
//! 2. Mutually exclusive effect conflicts:
//!    - `--save-case` with `--dry-run` exits code 2 (`invalid-usage`).
//!    - `--save-case` with `--no-persist` exits code 2 (`invalid-usage`).
//!    - `--save-case` with `--claude-hook` exits code 2 (`invalid-usage`).
//! 3. Explicit resolution case save & replay:
//!    - Atomic owner-only (0600) file creation.
//!    - Captured candidate options, manifest, and local evidence.
//!    - Validated roundtrip with `sr replay <file> --json`.
//! 4. Atomic no-clobber protections:
//!    - Refusal to overwrite existing target file (exit code 7, `storage-failure`).
//!    - Refusal to write through symlink target (exit code 7).
//!    - Refusal when parent directory does not exist (exit code 7).
//! 5. Provider-backed inference save & replay:
//!    - Multi-stage ("wide", "rerank") captured responses and fits.
//!    - Roundtrip evaluation with `sr replay <file> --json`.
//! 6. Low-need abstention case save & replay:
//!    - Single-stage ("wide") recorded response.
//!    - Recomputed decision matches historical abstention.
//! 7. Incurred usage preservation on export failure:
//!    - If case export fails after inference (e.g. pre-existing file), the
//!      resulting unavailable document preserves provider request and HTTP attempt
//!      metrics incurred during inference.

mod support;

use asupersync::tls::Certificate;
use serde_json::{Value, json};
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::jev::client::{JevClient, JevTransport};
use skillranker::jev::endpoint::EndpointConfig;
use skillranker::limits::DurationMillis;
use skillranker::output::OutputKind;
use skillranker::output::{GateStatus, RunStatus};
use skillranker::pipeline::{RankArgs, execute_pipeline};
use skillranker::privacy::EffectFlags;
use skillranker::replay::frozen::FrozenReplayInputs;
use skillranker::replay::{ReplayCase, ReplayPolicy, execute_replay};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn temp_workspace(name: &str) -> PathBuf {
    let root = Path::new("/tmp").join(format!(
        "sr-save-case-{}-{}-{}",
        name,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)
        .unwrap();
    // These are successful private-export fixtures. Do not let a worker's
    // umask 002 turn their ancestors into group-writable directories.
    for relative in ["home", "config", "workspace/.claude/skills"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(root.join(relative))
            .unwrap();
    }
    root
}

fn write_skill(root: &Path, name: &str, description: &str) {
    let dir = root.join("workspace/.claude/skills").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
    )
    .unwrap();
}

fn write_context_file(root: &Path, session: &str, request: &str) -> PathBuf {
    let path = root.join("workspace/context.json");
    let context = json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": "save-case-test",
        "workspace_root": root.join("workspace").to_string_lossy(),
        "session_id": session,
        "agent_id": null,
        "branch_id": null,
        "context_epoch": null,
        "current_request": {
            "event_id": "req-001",
            "text": request,
            "attachments_omitted": false,
            "essential_attachment_missing": false,
        },
        "events": [{
            "event_id": "req-001",
            "parent_id": null,
            "turn_id": "turn-1",
            "agent_id": null,
            "branch_id": null,
            "role": "user",
            "kind": "message",
            "timestamp_unix_ms": null,
            "text": request,
            "tool": null,
        }],
        "explicit_skill_references": [],
        "supplied_loads": [],
    });
    fs::write(&path, serde_json::to_vec_pretty(&context).unwrap()).unwrap();
    path
}

fn run_sr(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("workspace"))
        .args(args)
        .output()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Loopback Provider Fixture
// ---------------------------------------------------------------------------

struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(root: &Path, scenario: &str) -> Self {
        let directory = root.join(format!("provider-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&directory).unwrap();
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
            fs::write(directory.join(name), bytes).unwrap();
        }
        let mut child = Command::new("/usr/bin/python3")
            .arg(directory.join("provider_server.py"))
            .arg(scenario)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    fn client(&self) -> JevClient {
        let endpoint =
            EndpointConfig::from_base_origin_str(&format!("https://localhost:{}", self.port))
                .unwrap();
        let root = Certificate::from_pem(include_bytes!("fixtures/jev-tls/ca.pem"))
            .unwrap()
            .remove(0);
        JevClient::with_additional_roots(endpoint, vec![root]).unwrap()
    }

    fn finish(mut self) -> Vec<Value> {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        done.write_all(b"DONE").unwrap();
        drop(done);
        let mut served = Vec::new();
        loop {
            let mut line = String::new();
            if self.lines.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let val: Value = serde_json::from_str(&line).unwrap();
            if val["done"] == true {
                break;
            }
            if val["handshake_rejected"] == true {
                continue;
            }
            served.push(val);
        }
        let _ = self.child.wait();
        served
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rank_with_args(
    provider: Option<&Provider>,
    args: RankArgs,
    total_ms: u64,
) -> Result<Value, (u8, &'static str, Option<Value>)> {
    let clock = EntryClock::capture_with(
        DurationMillis::new("save-case-total", total_ms, 30_000).unwrap(),
        DurationMillis::new("save-case-cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let client = provider.map(|p| p.client());
    let transport = client.as_ref().map(|c| c as &dyn JevTransport);
    let result = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, transport).await });
    let shutdown_ok = invocation.shutdown();
    assert!(shutdown_ok, "owned runtime must shut down cleanly");
    match result {
        Ok(doc) => {
            if matches!(
                doc.kind(),
                OutputKind::Decision(skillranker::output::Decision::Unavailable)
            ) {
                let code = doc.exit_code() as u8;
                let val = doc.as_value().clone();
                let _kind = val["error"]["kind"].as_str().unwrap_or("unavailable");
                Err((code, "unavailable", Some(val)))
            } else {
                Ok(doc.as_value().clone())
            }
        }
        Err((code, kind, _msg)) => Err((code, kind, None)),
    }
}

// ---------------------------------------------------------------------------
// CLI Tests
// ---------------------------------------------------------------------------

#[test]
fn save_case_cli_help_documents_option() {
    let root = temp_workspace("help");
    let output = run_sr(&root, &["rank", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--save-case FILE"));
}

#[test]
fn save_case_cli_conflict_with_dry_run_fails() {
    let root = temp_workspace("conflict-dry-run");
    write_context_file(&root, "s1", "Test request");
    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--dry-run",
            "--save-case",
            "case.json",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["error"]["kind"], "invalid-usage");
}

#[test]
fn save_case_cli_conflict_with_no_persist_fails() {
    let root = temp_workspace("conflict-no-persist");
    write_context_file(&root, "s1", "Test request");
    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--no-persist",
            "--save-case",
            "case.json",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["error"]["kind"], "invalid-usage");
}

#[test]
fn save_case_conflict_with_claude_hook_fails() {
    let root = temp_workspace("conflict-claude-hook");
    write_context_file(&root, "s1", "Test request");
    let mut args = make_pipeline_args(&root, Some(root.join("workspace/case.json")));
    args.source_options.claude_hook = true;
    let outcome = rank_with_args(None, args, 3000);
    let (code, kind, _) = outcome.expect_err("claude-hook must conflict with save-case");
    assert_eq!(code, 2);
    assert_eq!(kind, "invalid-usage");
}

#[test]
fn save_case_cli_explicit_resolution_roundtrip_to_replay() {
    let root = temp_workspace("explicit-roundtrip");
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_context_file(&root, "s1", "Review rust test triage");

    let case_path = root.join("workspace/case.json");
    let before_capture = unix_ms();
    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--require-skill",
            "alpha",
            "--save-case",
            "case.json",
            "--offline",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // 1. Verify case file existence and permissions
    assert!(case_path.exists());
    let meta = fs::symlink_metadata(&case_path).unwrap();
    assert!(meta.is_file(), "case file must be regular file");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "case file must be owner-only (0600)");

    // 2. Inspect case content
    let raw_case = fs::read_to_string(&case_path).unwrap();
    let case: ReplayCase = serde_json::from_str(&raw_case).unwrap();
    assert!((before_capture..=unix_ms()).contains(&case.local_evidence.as_of_unix_ms));
    assert_eq!(case.schema_version, 2);
    assert_eq!(case.manifest.evidence_origin, "recorded");
    assert_eq!(case.historical_decision["decision"], "explicit");
    assert!(
        case.captured_request
            .candidate_options
            .iter()
            .any(|c| c.invocation_name == "alpha")
    );

    // 3. Roundtrip with sr replay
    let replay_output = run_sr(&root, &["replay", "case.json", "--json"]);
    assert_eq!(
        replay_output.status.code(),
        Some(0),
        "replay stdout: {}, stderr: {}",
        String::from_utf8_lossy(&replay_output.stdout),
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_val: Value = serde_json::from_slice(&replay_output.stdout).unwrap();
    assert_eq!(replay_val["kind"], "replay");
    assert_eq!(replay_val["historical"]["decision"], "explicit");
    assert_eq!(replay_val["recomputed"]["decision"], "explicit");
}

#[test]
fn save_case_cli_redacts_explicit_prose_before_summary_truncation() {
    let root = temp_workspace("explicit-redaction");
    let canary = "sk-aB7cD8eF9gH0jK1mN2pQ3rS4tU5vW6xY";
    write_skill(
        &root,
        "alpha",
        &format!("Review rust tests. API key {canary}"),
    );
    // The summary cutoff splits this token in the original unredacted input.
    let request = format!("Review rust tests. {}{canary}", " ".repeat(85));
    write_context_file(&root, "s1", &request);
    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--require-skill",
            "alpha",
            "--save-case",
            "case.json",
            "--offline",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let raw = fs::read_to_string(root.join("workspace/case.json")).unwrap();
    assert!(!raw.contains(canary));
    assert!(
        !raw.contains(&canary[..12]),
        "summary must not retain a token prefix"
    );
    let case = ReplayCase::from_json_bytes(raw.as_bytes()).unwrap();
    assert!(
        case.captured_request
            .context_text
            .as_ref()
            .unwrap()
            .contains("[REDACTED]")
    );
    assert!(
        case.captured_request
            .context_text
            .as_ref()
            .unwrap()
            .contains("Review rust tests.")
    );
    assert!(
        case.manifest
            .prompt_summary
            .as_ref()
            .unwrap()
            .contains("[REDACTED]")
    );
    assert!(
        case.captured_request.candidate_options[0]
            .description
            .as_ref()
            .unwrap()
            .contains("[REDACTED]")
    );
    let replay = run_sr(&root, &["replay", "case.json", "--json"]);
    assert_eq!(replay.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(doc["recomputed"]["decision"], "explicit");
    assert_eq!(doc["historical"]["usage"]["http_attempts"], 0);
}

#[test]
fn save_case_cli_refuses_to_overwrite_existing_file() {
    let root = temp_workspace("no-clobber");
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_context_file(&root, "s1", "Review rust test triage");

    let case_path = root.join("workspace/case.json");
    fs::write(&case_path, b"pre-existing-content").unwrap();

    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--require-skill",
            "alpha",
            "--save-case",
            "case.json",
            "--offline",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(9),
        "must exit with code 9 (storage-failure)"
    );
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["error"]["kind"], "storage-failure");
    assert!(
        val["error"]["message"]
            .as_str()
            .unwrap()
            .contains("destination target already exists; refusing to overwrite")
    );

    // Verify existing file was preserved (no clobber)
    assert_eq!(fs::read(&case_path).unwrap(), b"pre-existing-content");
}

#[test]
fn save_case_cli_refuses_symlink() {
    let root = temp_workspace("refuse-symlink");
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_context_file(&root, "s1", "Review rust test triage");

    let target_path = root.join("workspace/target.json");
    let link_path = root.join("workspace/symlink.json");
    fs::write(&target_path, b"target-content").unwrap();
    std::os::unix::fs::symlink(&target_path, &link_path).unwrap();

    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--require-skill",
            "alpha",
            "--save-case",
            "symlink.json",
            "--offline",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(9),
        "must exit with code 9 (storage-failure)"
    );
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["error"]["kind"], "storage-failure");
}

#[test]
fn save_case_cli_refuses_missing_parent_directory() {
    let root = temp_workspace("missing-parent");
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_context_file(&root, "s1", "Review rust test triage");

    let output = run_sr(
        &root,
        &[
            "rank",
            "--context",
            "context.json",
            "--require-skill",
            "alpha",
            "--save-case",
            "nonexistent_subdir/case.json",
            "--offline",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(9),
        "must exit with code 9 (storage-failure)"
    );
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["error"]["kind"], "storage-failure");
}

// ---------------------------------------------------------------------------
// Pipeline & Loopback Inference Tests
// ---------------------------------------------------------------------------

fn make_pipeline_args(root: &Path, save_case: Option<PathBuf>) -> RankArgs {
    let flags = EffectFlags {
        offline: false,
        allow_network: true,
        dry_run: false,
        no_cache: false,
        no_ledger: true,
        no_persist: false,
        save_case: save_case.is_some(),
    };
    RankArgs {
        workspace: root.join("workspace"),
        user_config_root: Some(root.join("config")),
        home: Some(root.join("home")),
        cache_dir: Some(support::private_store_dir("cache")),
        ledger_dir: Some(support::private_store_dir("ledger")),
        sources: ConfigSources {
            environment: vec![(
                OsString::from("TYPESAFE_API_KEY"),
                OsString::from("test-key-acceptance"),
            )],
            ..Default::default()
        },
        gate: EffectGate::new(flags, Scope::Rank).unwrap(),
        source_options: SourceOptions {
            context: Some(LocalPath::new(root.join("workspace/context.json"))),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case,
    }
}

#[test]
fn save_case_pipeline_inference_roundtrip_to_replay() {
    let root = temp_workspace("inference-roundtrip");
    let canary = "sk-aB7cD8eF9gH0jK1mN2pQ3rS4tU5vW6xY";
    write_skill(
        &root,
        "alpha",
        &format!("Runs and repairs failing rust tests. API key {canary}"),
    );
    write_skill(&root, "beta", "Drafts release notes from git history.");
    write_context_file(&root, "s1", "Failing rust test triage needed");
    fs::write(
        root.join("config/config.toml"),
        "[network]\nenabled = true\n",
    )
    .unwrap();

    let provider = Provider::start(&root, "useful");
    let case_path = root.join("workspace/inference_case.json");
    let mut args = make_pipeline_args(&root, Some(case_path.clone()));
    args.roster_file = Some(write_authorized_roster(&root, &["alpha", "beta"]));

    let before_capture = unix_ms();
    let outcome = rank_with_args(Some(&provider), args, 5000);
    let doc = outcome.expect("pipeline execution must succeed");
    assert_eq!(doc["decision"], "ranked");

    let served = provider.finish();
    assert_eq!(
        served.len(),
        2,
        "both wide and rerank stages must be served"
    );

    // Verify case file written with owner-only permissions
    assert!(case_path.exists());
    let meta = fs::symlink_metadata(&case_path).unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "case file must be 0600 owner-only");

    // Parse case and verify recorded stages
    let raw = fs::read_to_string(&case_path).unwrap();
    assert!(!raw.contains(canary));
    let case: ReplayCase = serde_json::from_str(&raw).unwrap();
    assert!((before_capture..=unix_ms()).contains(&case.local_evidence.as_of_unix_ms));
    assert!(case.captured_request.candidate_options.iter().any(|c| {
        c.description
            .as_ref()
            .is_some_and(|d| d.contains("[REDACTED]"))
    }));
    assert_eq!(case.manifest.stages_recorded, vec!["wide", "rerank"]);
    assert!(case.recorded_responses.wide.is_some());
    assert!(case.recorded_responses.rerank.is_some());
    assert!(!case.captured_request.candidate_options.is_empty());
    assert_eq!(case.historical_decision["decision"], "ranked");
    let frozen = case.frozen_inputs.as_ref().expect("actual frozen inputs");
    assert!(frozen.exact_compatible());
    assert_eq!(frozen.stages.len(), 2);
    assert_eq!(frozen.stages[0].stage, "wide");
    assert_eq!(frozen.stages[1].stage, "rerank");
    assert!(
        frozen
            .numeric_inputs
            .iter()
            .all(|n| n.prior_delta == 0.0 && n.phase_match == 0.0)
    );
    assert!(
        frozen
            .stages
            .iter()
            .all(|s| s.response_json.is_some() && !s.request_json.contains(canary))
    );
    for (stage, observed) in frozen.stages.iter().zip(&served) {
        assert_eq!(
            stage.wire_request_json,
            observed["body"].as_str().unwrap(),
            "actual provider request bytes"
        );
    }
    assert_frozen_artifact_controls(&case);
    // Neither source paths nor ambient policy may be consulted during replay.
    fs::write(
        root.join("workspace/context.json"),
        b"invalid ambient context",
    )
    .unwrap();
    fs::write(
        root.join("workspace/.claude/skills/alpha/SKILL.md"),
        b"changed source",
    )
    .unwrap();
    fs::write(root.join("config/config.toml"), b"invalid ambient policy").unwrap();

    // Replay the saved case with sr replay
    let replay_output = run_sr(&root, &["replay", "inference_case.json", "--json"]);
    assert_eq!(
        replay_output.status.code(),
        Some(0),
        "replay stdout: {}, stderr: {}",
        String::from_utf8_lossy(&replay_output.stdout),
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_val: Value = serde_json::from_slice(&replay_output.stdout).unwrap();
    assert_eq!(replay_val["kind"], "replay");
    assert_eq!(replay_val["gate_status"], "passed");
    assert_eq!(replay_val["historical"]["decision"], "ranked");
    assert_eq!(replay_val["recomputed"]["decision"], "ranked");
    for field in [
        "needs_skill",
        "phase",
        "none_probability",
        "choice_confidence",
        "omitted_rank_mass",
    ] {
        assert_eq!(
            replay_val["recomputed"][field], replay_val["historical"][field],
            "{field}"
        );
    }
    for (historical, replayed) in replay_val["historical"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .zip(replay_val["recomputed"]["skills"].as_array().unwrap())
    {
        for field in [
            "skill_id",
            "content_hash",
            "rank_score",
            "wide_probability",
            "rerank_probability",
            "fits",
        ] {
            assert_eq!(historical[field], replayed[field], "{field}");
        }
    }
}

#[test]
fn save_case_pipeline_low_need_roundtrip_to_replay() {
    let root = temp_workspace("low-need-roundtrip");
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_context_file(&root, "s1", "Simple greeting message");
    fs::write(
        root.join("config/config.toml"),
        "[network]\nenabled = true\n",
    )
    .unwrap();

    let provider = Provider::start(&root, "low-need");
    let case_path = root.join("workspace/low_need_case.json");
    let mut args = make_pipeline_args(&root, Some(case_path.clone()));
    args.roster_file = Some(write_authorized_roster(&root, &["alpha"]));

    let before_capture = unix_ms();
    let outcome = rank_with_args(Some(&provider), args, 5000);
    let doc = outcome.expect("pipeline execution must succeed with abstain");
    assert_eq!(doc["decision"], "abstain");

    let served = provider.finish();
    assert_eq!(
        served.len(),
        1,
        "only wide stage should be served for low-need"
    );

    // Verify case file written
    assert!(case_path.exists());
    let case: ReplayCase = serde_json::from_str(&fs::read_to_string(&case_path).unwrap()).unwrap();
    assert!((before_capture..=unix_ms()).contains(&case.local_evidence.as_of_unix_ms));
    assert_eq!(case.manifest.stages_recorded, vec!["wide"]);
    assert!(case.recorded_responses.wide.is_some());
    assert!(case.recorded_responses.rerank.is_none());
    assert_eq!(case.historical_decision["decision"], "abstain");

    // Replay
    let replay_output = run_sr(&root, &["replay", "low_need_case.json", "--json"]);
    assert_eq!(
        replay_output.status.code(),
        Some(0),
        "replay stdout: {}, stderr: {}",
        String::from_utf8_lossy(&replay_output.stdout),
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_val: Value = serde_json::from_slice(&replay_output.stdout).unwrap();
    assert_eq!(replay_val["kind"], "replay");
    assert_eq!(replay_val["historical"]["decision"], "abstain");
    assert_eq!(replay_val["recomputed"]["decision"], "abstain");
    assert_eq!(replay_val["gate_status"], "passed");
    assert_eq!(replay_val["recomputed"]["reason"], "low-need");
    let comparison = skillranker::replay::execute_replay_comparison(
        &case,
        None,
        Some(&ReplayPolicy {
            gate_threshold: Some(0.0),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(comparison.run_status, RunStatus::Partial);
    assert_eq!(comparison.document.as_value()["run_status"], "partial");
    assert_eq!(
        comparison.document.as_value()["completeness"]["evidence_compatible"],
        false
    );

    // A captured request remains an attempted inference even if its response
    // and historical gate were lost. It cannot turn into a local abstention.
    let mut missing = case.clone();
    missing.recorded_responses.wide = None;
    missing.manifest.stages_recorded.clear();
    missing.frozen_inputs.as_mut().unwrap().stages[0].response_json = None;
    missing.historical_decision["needs_skill"] = Value::Null;
    skillranker::replay::frozen::FrozenReplayInputs::seal(&mut missing).unwrap();
    missing.validate().unwrap();
    let replayed = execute_replay(&missing, None).unwrap();
    assert_eq!(replayed.run_status, RunStatus::Partial);
    assert!(replayed.recomputed_decision.is_none());
    assert_eq!(
        replayed.document.as_value()["completeness"]["stages_required"],
        1
    );
    assert_eq!(
        replayed.document.as_value()["completeness"]["stages_completed"],
        0
    );
}

#[test]
fn save_case_multi_candidate_capture_preserves_exact_numeric_parity() {
    let root = temp_workspace("multi-candidate-parity");
    let names: Vec<_> = (0..16).map(|i| format!("method-{i:02}")).collect();
    for name in &names {
        write_skill(
            &root,
            name,
            "Diagnose a failing Rust integration test and verify the smallest correct repair.",
        );
    }
    write_context_file(
        &root,
        "numeric-parity",
        "Triage a failing Rust integration test and isolate the parser bug",
    );
    let provider = Provider::start(&root, "useful");
    let path = root.join("workspace/multi-candidate.json");
    let mut args = make_pipeline_args(&root, Some(path.clone()));
    let references: Vec<_> = names.iter().map(String::as_str).collect();
    args.roster_file = Some(write_authorized_roster(&root, &references));
    let doc = rank_with_args(Some(&provider), args, 5000).unwrap();
    assert_eq!(doc["decision"], "ranked");
    assert_eq!(provider.finish().len(), 2);
    let case = ReplayCase::load_from_file(&path).unwrap();
    assert_eq!(case.captured_request.candidate_options.len(), 16);
    assert!(case.recorded_responses.rerank.as_ref().unwrap().fits.len() > 2);
    let replay = execute_replay(&case, None).unwrap();
    assert_eq!(replay.run_status, RunStatus::Complete);
    assert_eq!(
        replay.gate_status,
        GateStatus::Passed,
        "{}",
        replay.document.as_value()
    );
}

#[test]
fn save_case_export_outcome_is_finalized_in_the_real_ledger() {
    for (fail_export, explicit) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = temp_workspace(if fail_export {
            "export-ledger-failure"
        } else {
            "export-ledger-success"
        });
        write_skill(&root, "alpha", "Runs and repairs failing Rust tests.");
        write_skill(&root, "beta", "Drafts release notes from git history.");
        write_context_file(&root, "ledger-capture", "Failing Rust test triage needed");
        let case_path = root.join("workspace/ledger-case.json");
        if fail_export {
            fs::write(&case_path, b"unchanged existing target").unwrap();
        }
        let mut args = make_pipeline_args(&root, Some(case_path.clone()));
        args.roster_file = Some(write_authorized_roster(&root, &["alpha", "beta"]));
        if explicit {
            args.require_skills = vec![skillranker::identity::SkillId::new("alpha").unwrap()];
        }
        args.gate = EffectGate::new(
            EffectFlags {
                allow_network: true,
                no_cache: true,
                save_case: true,
                ..Default::default()
            },
            Scope::Rank,
        )
        .unwrap();
        let ledger = args.ledger_dir.clone().unwrap();
        let setup_clock = EntryClock::capture_with(
            DurationMillis::new("ledger-setup", 5000, 30_000).unwrap(),
            DurationMillis::new("ledger-cleanup", 200, 30_000).unwrap(),
        )
        .unwrap();
        let setup = ProcessInvocation::from_clock(setup_clock).unwrap();
        let setup_cx = setup.request_cx().unwrap();
        skillranker::storage::init_ledger(
            &setup,
            &setup_cx,
            skillranker::storage::LedgerLocation::Directory(ledger.clone()),
        )
        .unwrap();
        assert!(setup.shutdown());
        let provider = (!explicit).then(|| Provider::start(&root, "useful"));
        let result = rank_with_args(provider.as_ref(), args, 5000);
        let attempts = if explicit { 0 } else { 2 };
        if let Some(provider) = provider {
            assert_eq!(provider.finish().len(), attempts);
        }
        let doc = if fail_export {
            let (code, _, doc) = result.expect_err("no-clobber export must fail");
            assert_eq!(code, 9);
            let doc = doc.unwrap();
            assert_eq!(doc["error"]["kind"], "storage-failure");
            assert_eq!(fs::read(&case_path).unwrap(), b"unchanged existing target");
            doc
        } else {
            let doc = result.unwrap();
            assert_eq!(
                doc["decision"],
                if explicit { "explicit" } else { "ranked" }
            );
            let case = ReplayCase::load_from_file(&case_path).unwrap();
            assert_eq!(
                execute_replay(&case, None).unwrap().gate_status,
                if explicit {
                    GateStatus::NotApplicable
                } else {
                    GateStatus::Passed
                }
            );
            doc
        };
        let db =
            rusqlite::Connection::open(ledger.join(skillranker::storage::LEDGER_FILE)).unwrap();
        let rows: Vec<(String, String, String)> = db
            .prepare("SELECT decision, reason, exposure_state FROM ranking_events")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].0,
            doc["decision"].as_str().unwrap(),
            "CLI and durable outcome must agree"
        );
        assert_eq!(
            rows[0].1,
            if fail_export {
                "storage-failure"
            } else if explicit {
                "explicit-match"
            } else {
                "eligible-candidates"
            }
        );
        assert_eq!(rows[0].2, "prepared");
        let (count, inputs, outputs): (i64, i64, i64) = db
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0) FROM provider_attempts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            count,
            i64::try_from(attempts).unwrap(),
            "late finalization cannot duplicate incurred attempts"
        );
        assert_eq!(
            u64::try_from(inputs).unwrap(),
            doc["usage"]["input_tokens"].as_u64().unwrap()
        );
        assert_eq!(
            u64::try_from(outputs).unwrap(),
            doc["usage"]["output_tokens"].as_u64().unwrap()
        );
    }
}

#[test]
fn save_case_pipeline_export_failure_preserves_incurred_usage() {
    let root = temp_workspace("export-failure-usage");
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_skill(&root, "beta", "Drafts release notes from git history.");
    write_context_file(&root, "s1", "Failing rust test triage needed");
    fs::write(
        root.join("config/config.toml"),
        "[network]\nenabled = true\n",
    )
    .unwrap();

    let provider = Provider::start(&root, "useful");

    // Pre-create the destination file so that atomic export fails (no-clobber)
    let case_path = root.join("workspace/clash_case.json");
    fs::write(&case_path, b"original-content").unwrap();

    let args = make_pipeline_args(&root, Some(case_path.clone()));
    let outcome = rank_with_args(Some(&provider), args, 5000);

    let served = provider.finish();
    assert_eq!(
        served.len(),
        2,
        "provider must have served both stages before export failed"
    );

    // Pipeline must return an unavailable decision with code 9 (storage failure)
    let (code, _kind, doc_opt) =
        outcome.expect_err("export failure must yield unavailable outcome");
    assert_eq!(code, 9, "exit code must be 9 (storage-failure)");

    let doc = doc_opt.expect("pipeline must return unavailable document on export failure");
    assert_eq!(doc["decision"], "unavailable");
    assert_eq!(doc["error"]["kind"], "storage-failure");

    // Preserved incurred usage: failing to save a case does not make the requests free!
    let usage = &doc["usage"];
    assert!(
        usage["requests"].as_u64().unwrap_or(0) >= 2,
        "must preserve provider requests incurred (got {})",
        usage["requests"]
    );
    assert!(
        usage["http_attempts"].as_u64().unwrap_or(0) >= 2,
        "must preserve HTTP attempts incurred (got {})",
        usage["http_attempts"]
    );
    assert!(
        usage["input_tokens"].as_u64().unwrap_or(0) > 0,
        "must preserve input tokens"
    );
    assert!(
        usage["output_tokens"].as_u64().unwrap_or(0) > 0,
        "must preserve output tokens"
    );

    // Ensure pre-existing file was not clobbered
    assert_eq!(fs::read(&case_path).unwrap(), b"original-content");
}

fn assert_frozen_artifact_controls(case: &ReplayCase) {
    case.validate().expect("actual successful capture");
    let exact = execute_replay(case, None).unwrap();
    assert_eq!(
        exact.gate_status,
        GateStatus::Passed,
        "{}",
        exact.document.as_value()
    );
    let mut corrupt = case.clone();
    corrupt.captured_request.candidate_options[0].content_hash = "9".repeat(64);
    assert!(corrupt.validate().is_err(), "unresealed tamper");
    for mutation in 0..10 {
        let mut corrupt = case.clone();
        let frozen = corrupt.frozen_inputs.as_mut().unwrap();
        match mutation {
            0 => {
                let duplicate = frozen.stages[0].options[0].clone();
                frozen.stages[0].options.push(duplicate);
            }
            1 => frozen.visible_roster.push(frozen.visible_roster[0].clone()),
            2 => frozen.numeric_inputs.push(frozen.numeric_inputs[0].clone()),
            3 => corrupt.recorded_responses.rerank.as_mut().unwrap().fits[0].fit = 0.01,
            4 => {
                let response = frozen.stages[0].response_json.as_mut().unwrap();
                *response = response.replacen('{', "{\"model\":\"duplicate\",", 1);
            }
            5 => frozen.stages[0].options[0].content_hash = "9".repeat(64),
            6 => {
                frozen.stages[0].wire_request_json =
                    frozen.stages[0]
                        .wire_request_json
                        .replacen("jev-latest", "jev-other", 1)
            }
            7 => {
                frozen.stages.swap(0, 1);
                corrupt.manifest.stages_recorded.swap(0, 1);
            }
            8 => {
                corrupt
                    .recorded_responses
                    .wide
                    .as_mut()
                    .unwrap()
                    .choices_probability = 0.01
            }
            _ => {
                corrupt
                    .recorded_responses
                    .rerank
                    .as_mut()
                    .unwrap()
                    .choices_probability = 0.01
            }
        }
        FrozenReplayInputs::seal(&mut corrupt).unwrap();
        assert!(
            corrupt.validate().is_err(),
            "resealed inconsistent input {mutation}"
        );
    }
    let mut incompatible = case.clone();
    incompatible
        .frozen_inputs
        .as_mut()
        .unwrap()
        .numeric_profile
        .backend = "different-f64-backend".into();
    FrozenReplayInputs::seal(&mut incompatible).unwrap();
    let outcome = execute_replay(&incompatible, None).unwrap();
    assert_eq!(outcome.run_status, RunStatus::Partial);
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
    assert!(outcome.recomputed_decision.is_none());
    assert!(
        execute_replay(
            &incompatible,
            Some(&ReplayPolicy {
                fit_threshold: Some(2.0),
                ..Default::default()
            })
        )
        .is_err(),
        "invalid policy must not be hidden by incompatible evidence"
    );
    let mut all_removed = case.clone();
    for member in &mut all_removed.frozen_inputs.as_mut().unwrap().visible_roster {
        member.pre_fit_eligible = false;
    }
    FrozenReplayInputs::seal(&mut all_removed).unwrap();
    let outcome = execute_replay(&all_removed, None).unwrap();
    assert_eq!(outcome.run_status, RunStatus::Partial);
    assert!(
        outcome.recomputed_decision.is_none(),
        "an uncaptured local reason cannot be borrowed from history"
    );
    let mut redacted = case.clone();
    let canary = "sk-aB7cD8eF9gH0jK1mN2pQ3rS4tU5vW6xY";
    redacted.historical_decision["model"]["wide_returned"] = json!(canary);
    FrozenReplayInputs::redact_history(&mut redacted).unwrap();
    FrozenReplayInputs::seal(&mut redacted).unwrap();
    assert!(!serde_json::to_string(&redacted).unwrap().contains(canary));
    assert!(redacted.frozen_inputs.as_ref().unwrap().privacy_transformed);
    assert_eq!(
        execute_replay(&redacted, None).unwrap().gate_status,
        GateStatus::NotEstablished
    );
    let mut transformed = case.clone();
    transformed
        .frozen_inputs
        .as_mut()
        .unwrap()
        .privacy_transformed = true;
    FrozenReplayInputs::seal(&mut transformed).unwrap();
    let outcome = execute_replay(&transformed, None).unwrap();
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
    assert_eq!(
        outcome.document.as_value()["completeness"]["evidence_compatible"],
        false
    );
    let outcome = execute_replay(
        case,
        Some(&ReplayPolicy {
            w_prior: Some(0.2),
            w_phase: Some(0.5),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(
        outcome.recomputed_decision.as_deref(),
        Some("ranked"),
        "captured actual zero adjustments permit a local weight change"
    );
    assert_frozen_rerank_bound(case);
}

fn assert_frozen_rerank_bound(case: &ReplayCase) {
    use skillranker::jev::codec::{Request, RequestFormat};
    use skillranker::jev::{rerank, wide};
    use skillranker::replay::frozen::{FrozenMember, FrozenOption, FrozenScoreInput};
    for count in [32, 33] {
        let mut probe = case.clone();
        probe.manifest.evidence_origin = "synthetic".into();
        let template = probe.captured_request.candidate_options[0].clone();
        probe.captured_request.candidate_options = (0..count)
            .map(|index| {
                let mut candidate = template.clone();
                candidate.skill_id = format!("s_bound{index:02}");
                candidate.invocation_name = format!("bound-{index:02}");
                candidate
            })
            .collect();
        let candidates = &probe.captured_request.candidate_options;
        let inputs = probe.frozen_inputs.as_mut().unwrap();
        inputs.visible_roster = candidates
            .iter()
            .map(|c| FrozenMember {
                skill_id: c.skill_id.clone(),
                content_hash: c.content_hash.clone(),
                visibility: c.visibility.clone().unwrap(),
                pre_fit_eligible: true,
            })
            .collect();
        inputs.numeric_inputs = candidates
            .iter()
            .map(|c| FrozenScoreInput {
                skill_id: c.skill_id.clone(),
                prior_delta: 0.0,
                phase_match: 0.0,
            })
            .collect();
        let options: Vec<_> = candidates
            .iter()
            .enumerate()
            .map(|(index, c)| FrozenOption {
                option_id: format!("bound{index:02}"),
                skill_id: c.skill_id.clone(),
                content_hash: c.content_hash.clone(),
            })
            .collect();
        for stage in &mut inputs.stages {
            let old_id = stage.options[0].option_id.clone();
            let choice = if stage.stage == "wide" {
                wide::WHICH
            } else {
                rerank::RERANK
            };
            let mut request: Value = serde_json::from_str(&stage.request_json).unwrap();
            let description = request["questions"][choice]["criteria"][&old_id].clone();
            let mut criteria = serde_json::Map::new();
            criteria.insert(
                wide::NONE_OPTION.into(),
                request["questions"][choice]["criteria"][wide::NONE_OPTION].clone(),
            );
            for option in &options {
                criteria.insert(option.option_id.clone(), description.clone());
            }
            request["questions"][choice]["criteria"] = Value::Object(criteria);
            if stage.stage == "rerank" {
                let fit = request["questions"][format!("{}{}", rerank::FIT_PREFIX, old_id)].clone();
                let questions = request["questions"].as_object_mut().unwrap();
                questions.retain(|key, _| key == rerank::RERANK);
                for option in &options {
                    questions.insert(
                        format!("{}{}", rerank::FIT_PREFIX, option.option_id),
                        fit.clone(),
                    );
                }
                stage.response_json = None;
            } else {
                let mut response: Value =
                    serde_json::from_str(stage.response_json.as_ref().unwrap()).unwrap();
                let mut probabilities = serde_json::Map::new();
                probabilities.insert(wide::NONE_OPTION.into(), json!(1.0));
                for option in &options {
                    probabilities.insert(option.option_id.clone(), json!(0.0));
                }
                response["answers"][wide::WHICH]["probabilities"] = Value::Object(probabilities);
                response["answers"][wide::WHICH]["choice"] = json!(wide::NONE_OPTION);
                stage.response_json = Some(serde_json::to_string(&response).unwrap());
            }
            let request = Request::from_json(&serde_json::to_vec(&request).unwrap()).unwrap();
            stage.request_json = String::from_utf8(request.to_json().unwrap()).unwrap();
            stage.wire_request_json =
                String::from_utf8(request.to_wire_json(RequestFormat::TypeSafe).unwrap()).unwrap();
            stage.options = options.clone();
        }
        probe.recorded_responses.rerank = None;
        probe.manifest.stages_recorded = vec!["wide".into()];
        let projected = probe.recorded_responses.wide.as_mut().unwrap();
        projected.choice = wide::NONE_OPTION.into();
        projected.choices_probability = 1.0;
        projected.distribution = candidates
            .iter()
            .map(|c| skillranker::replay::ChoiceDistributionItem {
                option_id: c.skill_id.clone(),
                probability: 0.0,
            })
            .chain(std::iter::once(
                skillranker::replay::ChoiceDistributionItem {
                    option_id: wide::NONE_OPTION.into(),
                    probability: 1.0,
                },
            ))
            .collect();
        FrozenReplayInputs::seal(&mut probe).unwrap();
        if count == 32 {
            probe
                .validate()
                .expect("honest maximum-size attempted rerank counterpart");
        } else {
            assert!(
                probe.validate().is_err(),
                "33 real rerank options exceed the live maximum"
            );
        }
    }
}

fn write_authorized_roster(root: &Path, names: &[&str]) -> PathBuf {
    let path = root.join("workspace/roster.json");
    let skills: Vec<_> = names
        .iter()
        .map(|name| json!({"source":"claude_code.project", "path":format!("{name}/SKILL.md")}))
        .collect();
    fs::write(&path, serde_json::to_vec(&json!({"schema":"sr.roster.v1", "harness":"claude_code", "mode":"authorized_files", "skills":skills})).unwrap()).unwrap();
    path
}

#[test]
fn save_case_partial_roster_cannot_pass_exact_parity_gate() {
    let root = temp_workspace("partial-capture");
    write_skill(&root, "alpha", "Repairs failing rust tests.");
    write_skill(&root, "bad", "Malformed metadata");
    fs::write(
        root.join("workspace/.claude/skills/bad/SKILL.md"),
        b"---\nname: bad\ndescription: [unterminated\n---\n",
    )
    .unwrap();
    write_context_file(&root, "partial-session", "Triage failing rust tests");
    let provider = Provider::start(&root, "useful");
    let path = root.join("workspace/partial.json");
    let doc = rank_with_args(
        Some(&provider),
        make_pipeline_args(&root, Some(path.clone())),
        5000,
    )
    .unwrap();
    assert_eq!(provider.finish().len(), 2);
    assert_eq!(doc["roster"]["partial"], true);
    let case = ReplayCase::load_from_file(&path).unwrap();
    let outcome = execute_replay(&case, None).unwrap();
    assert_eq!(outcome.run_status, RunStatus::Complete);
    assert_eq!(outcome.gate_status, GateStatus::NotEstablished);
    assert_eq!(
        outcome.document.as_value()["input_completeness"]["computation_profile"],
        true
    );
    assert_eq!(
        outcome.document.as_value()["completeness"]["evidence_compatible"],
        false
    );
}
