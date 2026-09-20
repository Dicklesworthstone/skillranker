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

use asupersync::tls::Certificate;
use serde_json::{Value, json};
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::jev::client::{JevClient, JevTransport};
use skillranker::jev::endpoint::EndpointConfig;
use skillranker::limits::DurationMillis;
use skillranker::output::OutputKind;
use skillranker::pipeline::{RankArgs, execute_pipeline};
use skillranker::privacy::EffectFlags;
use skillranker::replay::ReplayCase;
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

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
    fs::create_dir_all(root.join("home")).unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(root.join("workspace/.claude/skills")).unwrap();
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
    assert_eq!(case.schema_version, 1);
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
        cache_dir: None,
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
    write_skill(&root, "alpha", "Runs and repairs failing rust tests.");
    write_skill(&root, "beta", "Drafts release notes from git history.");
    write_context_file(&root, "s1", "Failing rust test triage needed");
    fs::write(
        root.join("config/config.toml"),
        "[network]\nenabled = true\n",
    )
    .unwrap();

    let provider = Provider::start(&root, "useful");
    let case_path = root.join("workspace/inference_case.json");
    let args = make_pipeline_args(&root, Some(case_path.clone()));

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
    let case: ReplayCase = serde_json::from_str(&fs::read_to_string(&case_path).unwrap()).unwrap();
    assert_eq!(case.manifest.stages_recorded, vec!["wide", "rerank"]);
    assert!(case.recorded_responses.wide.is_some());
    assert!(case.recorded_responses.rerank.is_some());
    assert!(!case.captured_request.candidate_options.is_empty());
    assert_eq!(case.historical_decision["decision"], "ranked");

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
    let args = make_pipeline_args(&root, Some(case_path.clone()));

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
