#![cfg(unix)]
//! Real binary checks of how `sr rank` reads its inputs and derives its
//! effects: bounded regular-file context and roster reads, the trusted context
//! profile, and gate-derived source restrictions. No provider is contacted:
//! every case runs offline or as a dry run.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    // Intentionally retained: repository policy forbids automatic tree deletion.
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-rank-inputs-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        for dir in ["workspace/.claude/skills", "home", "config/sr"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let f = Self { root };
        for name in ["alpha", "beta"] {
            let dir = f.workspace().join(".claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\ndescription: {name} helps with rust tests.\n---\nBody.\n"),
            )
            .unwrap();
        }
        f
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn context(&self, name: &str, tool_output: &str) -> PathBuf {
        let path = self.workspace().join(name);
        let event = |id: &str, role: &str, kind: &str, text: &str, tool: Value| {
            json!({"event_id": id, "parent_id": null, "turn_id": "turn-1", "agent_id": null,
                   "branch_id": null, "role": role, "kind": kind, "timestamp_unix_ms": null,
                   "text": text, "tool": tool})
        };
        let context = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": "synthetic-test",
            "workspace_root": self.workspace().to_string_lossy(),
            "session_id": "session-1",
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {"event_id": "request-2", "text": "Fix the failing rust test.",
                                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [
                event("request-1", "user", "message", "Run the rust tests.", Value::Null),
                event("call-1", "assistant", "tool_invocation", "", json!({
                    "call_id": "c1", "name": "Bash", "status": "succeeded",
                    "arguments": "cargo test", "result": tool_output})),
                event("request-2", "user", "message", "Fix the failing rust test.", Value::Null),
            ],
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        std::fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        path
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .current_dir(self.workspace())
            .args(args)
            .output()
            .unwrap()
    }
    fn error_kind(&self, args: &[&str]) -> (Option<i32>, String) {
        let output = self.run(args);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (
            output.status.code(),
            value["error"]["kind"].as_str().unwrap_or("").to_owned(),
        )
    }
}

fn dry_run_request_bytes(output: &Output) -> u64 {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["provider_request"]["stages"][0]["request_bytes"]
        .as_u64()
        .unwrap()
}

#[test]
fn context_files_are_bounded_regular_files() {
    let f = Fixture::new();
    // Positive twin: an ordinary context file previews.
    f.context("context.json", "test result: FAILED");
    let ok = f.run(&["rank", "--context", "context.json", "--dry-run", "--json"]);
    assert!(dry_run_request_bytes(&ok) > 0);
    // Oversized: refused before parsing, with a typed kind and no path.
    let big = f.workspace().join("big.json");
    std::fs::write(&big, vec![b' '; 1024 * 1024 + 1]).unwrap();
    let (code, kind) = f.error_kind(&["rank", "--context", "big.json", "--dry-run", "--json"]);
    assert_eq!((code, kind.as_str()), (Some(7), "oversized-input"));
    // A FIFO is refused without blocking.
    nix::unistd::mkfifo(
        &f.workspace().join("fifo.json"),
        nix::sys::stat::Mode::S_IRWXU,
    )
    .unwrap();
    let (code, kind) = f.error_kind(&["rank", "--context", "fifo.json", "--dry-run", "--json"]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-input"));
    // A symlink leaving its directory is refused.
    let outside = f.root.join("outside.json");
    std::fs::copy(f.workspace().join("context.json"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, f.workspace().join("link.json")).unwrap();
    let (code, kind) = f.error_kind(&["rank", "--context", "link.json", "--dry-run", "--json"]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-input"));
    let text = String::from_utf8_lossy(
        &f.run(&["rank", "--context", "link.json", "--dry-run", "--json"])
            .stdout,
    )
    .into_owned();
    assert!(
        !text.contains(f.root.to_str().unwrap()),
        "no path is echoed"
    );
}

#[test]
fn explicit_roster_files_are_bounded_regular_files() {
    let f = Fixture::new();
    f.context("context.json", "ok");
    nix::unistd::mkfifo(
        &f.workspace().join("roster.fifo"),
        nix::sys::stat::Mode::S_IRWXU,
    )
    .unwrap();
    let (code, kind) = f.error_kind(&[
        "rank",
        "--context",
        "context.json",
        "--roster",
        "roster.fifo",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(code, Some(5));
    assert_eq!(kind, "unusable-roster");
}

#[test]
fn the_trusted_minimal_profile_is_honored() {
    let f = Fixture::new();
    f.context("context.json", &"error: ".repeat(400));
    let standard = dry_run_request_bytes(&f.run(&[
        "rank",
        "--context",
        "context.json",
        "--dry-run",
        "--json",
    ]));
    // Trusted user configuration selects the minimal disclosure profile.
    std::fs::write(
        f.root.join("config/sr/config.toml"),
        "[context]\nprofile = \"minimal\"\n",
    )
    .unwrap();
    let minimal = dry_run_request_bytes(&f.run(&[
        "rank",
        "--context",
        "context.json",
        "--dry-run",
        "--json",
    ]));
    // Standard carries history and a bounded tool excerpt; minimal omits both,
    // so the same context renders strictly fewer request bytes.
    assert!(
        minimal < standard,
        "minimal {minimal} must render less than standard {standard}"
    );
}

#[test]
fn a_dry_run_refuses_cass_before_any_child_or_read() {
    let f = Fixture::new();
    let (code, kind) = f.error_kind(&[
        "rank",
        "--session",
        "/synthetic/session.jsonl",
        "--dry-run",
        "--json",
    ]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-source-mode"));
    let (code, kind) = f.error_kind(&[
        "rank",
        "--session",
        "/synthetic/session.jsonl",
        "--offline",
        "--json",
    ]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-source-mode"));
    assert!(!Path::new("/synthetic/session.jsonl").exists());
}

#[test]
fn claude_user_skills_come_from_home_not_the_config_root() {
    let f = Fixture::new();
    f.context("context.json", "ok");
    // A user-level skill exists only under $HOME/.claude/skills.
    let dir = f.root.join("home/.claude/skills/gamma");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\ndescription: gamma formats rust code.\n---\nBody.\n",
    )
    .unwrap();
    let output = f.run(&[
        "rank",
        "--context",
        "context.json",
        "--require-skill",
        "gamma",
        "--offline",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value}");
    assert_eq!(value["decision"], "explicit");
    assert_eq!(value["skills"][0]["invocation_name"], "gamma");
    // Negative twin: the same skill under the configuration root is not a
    // Claude skill root and stays invisible.
    let g = Fixture::new();
    g.context("context.json", "ok");
    let dir = g.root.join("config/.claude/skills/gamma");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\ndescription: gamma formats rust code.\n---\nBody.\n",
    )
    .unwrap();
    let output = g.run(&[
        "rank",
        "--context",
        "context.json",
        "--require-skill",
        "gamma",
        "--offline",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(value["decision"], "explicit", "{value}");
}

#[test]
fn offline_ranking_without_a_cached_result_is_a_cache_miss() {
    let f = Fixture::new();
    f.context("context.json", "ok");
    let output = f.run(&["rank", "--context", "context.json", "--offline", "--json"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(11), "{value}");
    assert_eq!(value["decision"], "unavailable");
    assert_eq!(value["error"]["kind"], "cache-miss");
    // Input was admitted, so this is a full decision showing nothing was sent.
    assert!(value["event_id"].is_string(), "{value}");
    assert_eq!(value["usage"]["requests"], 0);
    assert_eq!(value["usage"]["http_attempts"], 0);
    // Twin: online without trusted consent is a privacy denial instead.
    let output = f.run(&["rank", "--context", "context.json", "--json"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(8), "{value}");
    assert_eq!(value["error"]["kind"], "network-denied");
}

#[test]
fn shared_ranking_flags_reach_the_rank_command() {
    let f = Fixture::new();
    f.context("context.json", &"error: ".repeat(400));
    let base = ["rank", "--context", "context.json", "--dry-run", "--json"];
    let standard = dry_run_request_bytes(&f.run(&base));
    // The registry-driven CLI flag narrows disclosure like trusted config does.
    let minimal =
        dry_run_request_bytes(&f.run(&[&base[..], &["--context-profile", "minimal"]].concat()));
    assert!(
        minimal < standard,
        "minimal {minimal} vs standard {standard}"
    );
    // Documented bounds hold at the CLI layer too.
    let output = f.run(&[&base[..], &["--top", "0"]].concat());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{value}");
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn previewed_request(output: &Output) -> String {
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value}");
    value["provider_request"]["stages"][0]["request"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn project_signals_reach_the_request_unless_the_profile_is_minimal() {
    let f = Fixture::new();
    let workspace = f.workspace();
    std::fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
    git(&workspace, &["init", "-q"]);
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-qm", "base"]);
    std::fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"b\"\n").unwrap();
    f.context("context.json", "ok");
    let base = ["rank", "--context", "context.json", "--dry-run", "--json"];
    // The repository-relative dirty path reaches the provider request.
    assert!(previewed_request(&f.run(&base)).contains("Cargo.toml"));
    // The minimal profile omits dirty paths.
    let minimal = [&base[..], &["--context-profile", "minimal"]].concat();
    assert!(!previewed_request(&f.run(&minimal)).contains("Cargo.toml"));
}

impl Fixture {
    /// A context with prior user messages and an optional missing-essential flag.
    fn conversation(&self, request: &str, history: &[&str], essential_missing: bool) {
        let message = |id: String, text: &str| {
            json!({"event_id": id, "parent_id": null, "turn_id": "turn-1", "agent_id": null,
                   "branch_id": null, "role": "user", "kind": "message",
                   "timestamp_unix_ms": null, "text": text, "tool": null})
        };
        let mut events: Vec<Value> = history
            .iter()
            .enumerate()
            .map(|(i, text)| message(format!("earlier-{i}"), text))
            .collect();
        events.push(message("current".into(), request));
        let context = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": "synthetic-test",
            "workspace_root": self.workspace().to_string_lossy(),
            "session_id": "session-1",
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {"event_id": "current", "text": request,
                                "attachments_omitted": essential_missing,
                                "essential_attachment_missing": essential_missing},
            "events": events,
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        std::fs::write(
            self.workspace().join("context.json"),
            serde_json::to_vec(&context).unwrap(),
        )
        .unwrap();
    }
}

fn failure_kind(output: &Output) -> (Option<i32>, String) {
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = if value["error"].is_object() {
        &value["error"]
    } else {
        &value["local_decision"]["error"]
    };
    (
        output.status.code(),
        error["kind"].as_str().unwrap_or("").to_owned(),
    )
}

/// Feed the real CLI through a pipe, optionally retaining its writer after the
/// document. The watchdog reaps a broken implementation before asserting.
fn rank_stdin(f: &Fixture, input: Vec<u8>, hold_open: bool) -> (Output, bool) {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", f.root.join("home"))
        .env("XDG_CONFIG_HOME", f.root.join("config"))
        .current_dir(f.workspace())
        .args([
            "rank",
            "--context",
            "-",
            "--dry-run",
            "--json",
            "--timeout-ms",
            if hold_open { "1200" } else { "10000" },
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let result = pipe.write_all(&input);
        if hold_open {
            let _ = held.recv();
        }
        result
    });
    let end = Instant::now() + Duration::from_secs(if hold_open { 3 } else { 15 });
    let mut killed = false;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= end {
            child.kill().unwrap();
            killed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(release);
    let output = child.wait_with_output().unwrap();
    if let Err(error) = writer.join().unwrap() {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    (output, killed)
}

#[test]
fn normalized_stdin_deadline_does_not_wait_for_the_pipe_writer() {
    let f = Fixture::new();
    let valid = std::fs::read(f.context("context.json", "test failed")).unwrap();
    // Even a syntactically complete document needs EOF to establish that no
    // trailing document or extra bytes follow it.
    for input in [Vec::new(), b"{\"schema_version\":".to_vec(), valid] {
        let (output, killed) = rank_stdin(&f, input, true);
        assert!(!killed, "sr waited for stdin beyond its own deadline");
        assert_eq!(failure_kind(&output), (Some(6), "timeout".to_owned()));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["decision"], "unavailable");
    }
}

#[test]
fn normalized_stdin_eof_preserves_valid_malformed_and_byte_limit_results() {
    let f = Fixture::new();
    let valid = std::fs::read(f.context("context.json", "test failed")).unwrap();
    for size in [valid.len(), 1_048_575, 1_048_576] {
        let mut input = valid.clone();
        input.resize(size, b' ');
        let (output, killed) = rank_stdin(&f, input, false);
        assert!(!killed);
        assert!(dry_run_request_bytes(&output) > 0);
    }
    let (output, killed) = rank_stdin(&f, b"{".to_vec(), false);
    assert!(!killed);
    assert_eq!(
        failure_kind(&output),
        (Some(7), "malformed-input".to_owned())
    );
    let mut oversized = valid;
    oversized.resize(1_048_577, b' ');
    let (output, killed) = rank_stdin(&f, oversized, false);
    assert!(!killed);
    assert_eq!(
        failure_kind(&output),
        (Some(7), "oversized-input".to_owned())
    );
}

#[test]
fn input_that_cannot_support_an_evaluation_is_never_ranked() {
    let f = Fixture::new();
    let offline = ["rank", "--context", "context.json", "--offline", "--json"];
    // A terse continuation with no antecedent instruction.
    f.conversation("continue", &[], false);
    assert_eq!(
        failure_kind(&f.run(&offline)),
        (Some(7), "insufficient-context".to_owned())
    );
    // Twin: with the instruction in history, the same request is evaluated
    // and only the offline cache miss stops it.
    f.conversation("continue", &["Fix the failing rust test."], false);
    assert_eq!(
        failure_kind(&f.run(&offline)),
        (Some(11), "cache-miss".to_owned())
    );
    // Essential attached content is missing.
    f.conversation("Fix the bug in the attached screenshot.", &[], true);
    assert_eq!(
        failure_kind(&f.run(&offline)),
        (Some(7), "insufficient-context".to_owned())
    );
    // The latest request does not fit the rendering budget.
    f.conversation(&"Fix the failing rust test. ".repeat(20), &[], false);
    let small = [&offline[..], &["--budget-chars", "200"]].concat();
    assert_eq!(
        failure_kind(&f.run(&small)),
        (Some(7), "insufficient-context".to_owned())
    );
    // The context both requires and excludes the same skill.
    f.conversation(
        "Don't use skill alpha.",
        &["Please use skill alpha."],
        false,
    );
    assert_eq!(
        failure_kind(&f.run(&offline)),
        (Some(5), "unresolved-explicit".to_owned())
    );
}

/// A Claude transcript: `filler` earlier user records of `filler_bytes` text
/// each, then an explicit request, then `tail` verbatim (for example an
/// unfinished record).
fn transcript(f: &Fixture, filler: usize, filler_bytes: usize, tail: &str) -> PathBuf {
    let record = |n: usize, text: &str| {
        json!({"type": "user", "uuid": format!("u-{n}"), "parentUuid": null,
               "message": {"role": "user", "content": text}})
        .to_string()
    };
    let mut lines: Vec<String> = (0..filler)
        .map(|n| record(n, &"earlier context ".repeat(filler_bytes / 16)))
        .collect();
    lines.push(record(filler, "Please use skill alpha to fix this."));
    let path = f.workspace().join("session.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n" + tail).unwrap();
    path
}

/// Reads one transcript's quality flags. The subject is the reported history,
/// not the deadline, so the budget is generous: on a loaded machine the
/// default 3 s can expire during roster validation (sr-5n0b).
fn transcript_quality(f: &Fixture, path: &Path) -> Value {
    let output = f.run(&[
        "rank",
        "--transcript",
        path.to_str().unwrap(),
        "--harness",
        "claude_code",
        "--offline",
        "--timeout-ms",
        "20000",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value}");
    assert_eq!(value["decision"], "explicit", "{value}");
    value
}

#[test]
fn a_transcript_read_with_gaps_reports_them() {
    let f = Fixture::new();
    // Honest twin: a clean transcript is complete, with no gaps.
    let clean = transcript_quality(&f, &transcript(&f, 2, 64, ""));
    assert_eq!(clean["context_quality"], "complete", "{clean}");
    assert_eq!(clean["quality"]["source_gaps"], false);
    assert_eq!(clean["quality"]["history_windowed"], false);
    // An unfinished last record may be the real latest request.
    let unfinished = transcript(&f, 2, 64, r#"{"type": "user", "message": {"role": "#);
    let value = transcript_quality(&f, &unfinished);
    assert_eq!(value["context_quality"], "partial", "{value}");
    assert_eq!(value["quality"]["source_gaps"], true, "{value}");
}

#[test]
fn a_transcript_longer_than_its_tail_window_reports_windowed_history() {
    let f = Fixture::new();
    // About 2.4 MiB: the bounded 2 MiB tail read cannot include the start.
    let value = transcript_quality(&f, &transcript(&f, 120, 20 * 1024, ""));
    assert_eq!(value["quality"]["history_windowed"], true, "{value}");
    assert_eq!(value["quality"]["source_gaps"], false, "{value}");
}

/// A normalized context declaring `harness`, whose request is `request`.
fn context_for(f: &Fixture, harness: &str, request: &str) -> PathBuf {
    let path = f.workspace().join(format!("context-{harness}.json"));
    let context = json!({
        "schema_version": 1, "harness": harness, "producer_id": "synthetic-test",
        "workspace_root": f.workspace().to_string_lossy(), "session_id": "session-1",
        "agent_id": null, "branch_id": null, "context_epoch": null,
        "current_request": {"event_id": "request-1", "text": request,
                            "attachments_omitted": false, "essential_attachment_missing": false},
        "events": [{"event_id": "request-1", "parent_id": null, "turn_id": "turn-1",
                    "agent_id": null, "branch_id": null, "role": "user", "kind": "message",
                    "timestamp_unix_ms": null, "text": request, "tool": null}],
        "explicit_skill_references": [], "supplied_loads": []
    });
    std::fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
    path
}

#[test]
fn rank_sees_configured_roots_like_sr_roster() {
    let f = Fixture::new();
    let custom = f.workspace().join("custom/team-review");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(
        custom.join("SKILL.md"),
        "---\nname: team-review\ndescription: Reviews pull requests the team way.\n---\nBody.\n",
    )
    .unwrap();
    std::fs::create_dir_all(f.workspace().join(".sr")).unwrap();
    std::fs::write(
        f.workspace().join(".sr/config.toml"),
        "[roster]\nroots=['custom']\n",
    )
    .unwrap();
    // An explicit request for a configured skill resolves, labeled unverified.
    let request = context_for(&f, "claude_code", "Please use skill team-review here.");
    let output = f.run(&[
        "rank",
        "--context",
        request.to_str().unwrap(),
        "--offline",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value}");
    assert_eq!(value["decision"], "explicit", "{value}");
    assert_eq!(
        value["skills"][0]["invocation_name"], "team-review",
        "{value}"
    );
    assert_eq!(value["skills"][0]["visibility"], "unverified", "{value}");
    // An advisory preview offers it alongside the default Claude skills.
    let request = context_for(&f, "claude_code", "Review this pull request for problems.");
    let output = f.run(&[
        "rank",
        "--context",
        request.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    let preview = previewed_request(&output);
    assert!(
        preview.contains("Reviews pull requests the team way"),
        "{preview}"
    );
    assert!(preview.contains("alpha helps with rust tests"), "{preview}");
}

#[test]
fn another_harness_needs_a_supplied_roster() {
    let f = Fixture::new();
    // Claude's directories hold skills, but this is a Codex session.
    let request = context_for(&f, "codex", "Review this pull request for problems.");
    let (code, kind) = f.error_kind(&[
        "rank",
        "--context",
        request.to_str().unwrap(),
        "--offline",
        "--json",
    ]);
    assert_eq!((code, kind.as_str()), (Some(5), "unusable-roster"));
    // Honest twin: the same request from a Claude session previews normally.
    let request = context_for(&f, "claude_code", "Review this pull request for problems.");
    let output = f.run(&[
        "rank",
        "--context",
        request.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert!(previewed_request(&output).contains("alpha helps with rust tests"));
}
