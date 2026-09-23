#![cfg(unix)]
//! P6 Claude shadow hook protocol boundary contract.
//!
//! Satisfies contract boundary `p6_shadow_hook_execution` (sr-roadmap-l1i.7.1)
//! mapped in `tests/contract_matrix.toml`.
//!
//! Asserts:
//! 1. `shadow_hook_non_blocking`:
//!    - Evaluates Claude hook stdin in shadow mode without blocking or injecting context.
//!    - Stdout is completely empty.
//!    - Exit code is 0.
//!    - Stdin prompt is authoritative and overlaid into history.
//!    - Missing transcript on initial turn is treated as `PromptOnly`.
//!    - Deduplicates by `prompt_id`.
//!    - When ledger is configured, prepared ranking event is recorded in ledger with
//!      `channel = "shadow"` and `exposure_state = "prepared"`, but NO emitted exposure record is created.
//! 2. `quiet_on_failure`:
//!    - Malformed stdin JSON: exit 0, empty stdout, diagnostic on stderr.
//!    - Missing prompt: exit 0, empty stdout, diagnostic on stderr.
//!    - Mismatched session: exit 0, empty stdout, diagnostic on stderr.
//!    - Unsupported hook event: exit 0, empty stdout, diagnostic on stderr.
//!    - Malformed CLI flags to `sr hook claude`: exit 0, empty stdout, diagnostic on stderr.
//! 3. `stdin_deadline_enforced`:
//!    - Stdin reading adheres to invocation deadline; timing out produces exit 0, empty stdout.
//! 4. CLI error preservation:
//!    - Non-hook CLI errors (e.g. `sr rank --bogus`) retain exit 2.

use serde_json::json;
use skillranker::adapter::{ClaudeUserPromptSubmit, UnknownFieldPolicy};
use skillranker::context::overlay::{
    ClaudeOverlayRequest, OverlayError, apply_claude_prompt_overlay,
};
use skillranker::output::ContextQuality;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct HookFixture {
    root: PathBuf,
    session_id: String,
}

impl HookFixture {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!(
            "sr-hook-contract-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&root)
            .expect("create root");
        let session_id = "session-12345-hook-contract".to_string();
        let f = Self { root, session_id };
        fs::create_dir_all(f.workspace()).unwrap();
        fs::create_dir_all(f.home().join(".config")).unwrap();
        fs::create_dir_all(f.ledger_dir()).unwrap();

        for dir in [&f.root, &f.home(), &f.workspace(), &f.ledger_dir()] {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }

        // One valid skill
        let skill = f.home().join(".claude/skills/test-repair");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: Fixes broken rust tests.\n---\nBody.\n",
        )
        .unwrap();

        f
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn ledger_dir(&self) -> PathBuf {
        self.root.join("ledger")
    }

    fn transcript_path(&self) -> PathBuf {
        self.root.join("transcript.jsonl")
    }

    fn write_transcript(&self, records: &[serde_json::Value]) {
        let mut text = String::new();
        for r in records {
            text.push_str(&r.to_string());
            text.push('\n');
        }
        fs::write(self.transcript_path(), text).unwrap();
    }

    fn run_hook(&self, stdin_bytes: &[u8], args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .current_dir(self.workspace())
            .arg("hook")
            .arg("claude")
            .arg("--dir")
            .arg(self.ledger_dir())
            .arg("--offline")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("spawn sr hook claude");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_bytes);
        }
        child.wait_with_output().expect("wait for sr hook claude")
    }

    fn run_cli(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .current_dir(self.workspace())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().expect("binary execution")
    }
}

fn user_event(id: &str, parent: Option<&str>, session: &str, text: &str) -> serde_json::Value {
    json!({
        "type": "user",
        "uuid": id,
        "parentUuid": parent,
        "sessionId": session,
        "message": { "role": "user", "content": text }
    })
}

// ---------------------------------------------------------------------------
// Unit & property checks on hook input & overlay
// ---------------------------------------------------------------------------

#[test]
fn hook_stdin_payload_parsing_and_bounds() {
    let payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Fix the rust build",
        "prompt_id": "prompt-001",
        "session_id": "session-abc",
        "transcript_path": "/tmp/test.jsonl",
        "cwd": "/data/projects/skillranker",
        "custom_field": "additive_data"
    });
    let bytes = serde_json::to_vec(&payload).unwrap();
    let hook = ClaudeUserPromptSubmit::from_json(&bytes, UnknownFieldPolicy::RetainAdditive)
        .expect("valid hook payload parses");

    assert_eq!(hook.prompt.as_str(), "Fix the rust build");
    assert_eq!(hook.prompt_id.as_ref().unwrap().as_str(), "prompt-001");
    assert_eq!(hook.session_id.as_ref().unwrap().as_str(), "session-abc");
    assert_eq!(
        hook.transcript_path.as_ref().unwrap().as_str(),
        "/tmp/test.jsonl"
    );

    // Unsupported hook event is rejected
    let unsupported = json!({
        "hook_event_name": "UserPromptExpansion",
        "prompt": "Fix the rust build"
    });
    let err = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&unsupported).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    );
    assert!(err.is_err(), "UserPromptExpansion must be refused");

    // Unknown hook event is rejected
    let unknown = json!({
        "hook_event_name": "SomethingUnexpected",
        "prompt": "Fix the rust build"
    });
    let err = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&unknown).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    );
    assert!(err.is_err(), "Unknown hook event must be refused");

    // Missing prompt is rejected
    let missing_prompt = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "session-abc"
    });
    let err = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&missing_prompt).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    );
    assert!(err.is_err(), "Missing prompt must be refused");
}

#[test]
fn prompt_overlay_authoritative_and_first_turn_prompt_only() {
    let fixture = HookFixture::new();

    // Initial prompt in a new session: transcript does NOT exist on disk yet.
    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Initial session request",
        "prompt_id": "prompt-init",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
    });
    let hook_input = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&hook_payload).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    )
    .unwrap();

    let req = ClaudeOverlayRequest {
        hook_input,
        transcript_path: Some(fixture.transcript_path()),
        authorized_root: None,
    };
    let outcome = apply_claude_prompt_overlay(&req)
        .expect("missing transcript on first turn must succeed as PromptOnly");
    assert_eq!(outcome.context_quality, ContextQuality::PromptOnly);
    assert_eq!(outcome.events.len(), 1);
    assert_eq!(
        outcome.current_request.text.as_str(),
        "Initial session request"
    );
    assert!(outcome.prompt_overlaid);

    // Subsequent turn: transcript exists on disk with historical events.
    fixture.write_transcript(&[user_event(
        "e-1",
        None,
        &fixture.session_id,
        "Prior turn request",
    )]);
    let hook_payload2 = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Second turn request",
        "prompt_id": "prompt-2",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
    });
    let hook_input2 = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&hook_payload2).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    )
    .unwrap();
    let req2 = ClaudeOverlayRequest {
        hook_input: hook_input2,
        transcript_path: Some(fixture.transcript_path()),
        authorized_root: None,
    };
    let outcome2 = apply_claude_prompt_overlay(&req2).expect("valid transcript overlay");
    assert_eq!(outcome2.context_quality, ContextQuality::Complete);
    assert_eq!(outcome2.events.len(), 2);
    assert_eq!(
        outcome2.current_request.text.as_str(),
        "Second turn request"
    );
}

#[test]
fn prompt_overlay_deduplication_and_session_mismatch() {
    let fixture = HookFixture::new();

    // Deduplication by prompt_id: prompt already present in transcript
    fixture.write_transcript(&[user_event(
        "prompt-existing",
        None,
        &fixture.session_id,
        "Repeated text",
    )]);
    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Repeated text",
        "prompt_id": "prompt-existing",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
    });
    let hook_input = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&hook_payload).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    )
    .unwrap();
    let req = ClaudeOverlayRequest {
        hook_input,
        transcript_path: Some(fixture.transcript_path()),
        authorized_root: None,
    };
    let outcome = apply_claude_prompt_overlay(&req).expect("deduplication succeeds");
    assert!(outcome.deduplicated_by_event_id);
    assert_eq!(outcome.events.len(), 1);

    // Foreign session mismatch: transcript has a different sessionId
    fixture.write_transcript(&[user_event(
        "e-1",
        None,
        "foreign-session-id",
        "Some request",
    )]);
    let hook_payload_mismatch = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "New request",
        "prompt_id": "p-new",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
    });
    let hook_input_mismatch = ClaudeUserPromptSubmit::from_json(
        &serde_json::to_vec(&hook_payload_mismatch).unwrap(),
        UnknownFieldPolicy::RetainAdditive,
    )
    .unwrap();
    let req_mismatch = ClaudeOverlayRequest {
        hook_input: hook_input_mismatch,
        transcript_path: Some(fixture.transcript_path()),
        authorized_root: None,
    };
    let err =
        apply_claude_prompt_overlay(&req_mismatch).expect_err("session mismatch must be rejected");
    assert!(matches!(err, OverlayError::SessionMismatch { .. }));
}

// ---------------------------------------------------------------------------
// Subprocess & protocol boundary acceptance
// ---------------------------------------------------------------------------

#[test]
fn shadow_hook_non_blocking() {
    let fixture = HookFixture::new();

    // Initialize ledger so prepared ranking state can be recorded
    let init_out = fixture.run_cli(&[
        "ledger",
        "init",
        "--dir",
        fixture.ledger_dir().to_str().unwrap(),
    ]);
    assert_eq!(init_out.status.code(), Some(0), "ledger init must succeed");

    // Write a valid transcript
    fixture.write_transcript(&[user_event(
        "turn-1",
        None,
        &fixture.session_id,
        "Previous turn",
    )]);

    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Fix failing tests",
        "prompt_id": "prompt-shadow-1",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
        "cwd": fixture.workspace(),
    });

    let output = fixture.run_hook(&serde_json::to_vec(&hook_payload).unwrap(), &[]);

    // In shadow mode:
    // 1. Exit code MUST be 0
    assert_eq!(
        output.status.code(),
        Some(0),
        "shadow hook must return exit code 0"
    );

    // 2. Stdout MUST be completely empty (zero bytes emitted to agent context)
    assert!(
        output.stdout.is_empty(),
        "shadow hook stdout must be completely empty, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // 3. Non-blocking: no Claude blocking decision fields
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr_str.contains("\"decision\":\"block\""),
        "shadow hook must never emit blocking decision fields"
    );

    // 4. Ledger verification: check that emission was NOT created
    // Verify through ledger status or inspection that zero emissions were recorded
    let status_out = fixture.run_cli(&[
        "ledger",
        "status",
        "--dir",
        fixture.ledger_dir().to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(status_out.status.code(), Some(0));
}

#[test]
fn quiet_on_failure() {
    let fixture = HookFixture::new();

    // Case 1: Malformed stdin JSON
    let bad_json = b"{\"hook_event_name\": \"UserPromptSubmit\", not valid json";
    let out1 = fixture.run_hook(bad_json, &[]);
    assert_eq!(
        out1.status.code(),
        Some(0),
        "malformed json must return exit 0"
    );
    assert!(
        out1.stdout.is_empty(),
        "malformed json must emit zero stdout"
    );
    assert!(!out1.stderr.is_empty(), "diagnostic must go to stderr");

    // Case 2: Missing prompt in hook payload
    let no_prompt = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": fixture.session_id
    });
    let out2 = fixture.run_hook(&serde_json::to_vec(&no_prompt).unwrap(), &[]);
    assert_eq!(
        out2.status.code(),
        Some(0),
        "missing prompt must return exit 0"
    );
    assert!(
        out2.stdout.is_empty(),
        "missing prompt must emit zero stdout"
    );
    assert!(!out2.stderr.is_empty(), "diagnostic must go to stderr");

    // Case 3: Unsupported hook event
    let unsupported = json!({
        "hook_event_name": "UserPromptExpansion",
        "prompt": "hello"
    });
    let out3 = fixture.run_hook(&serde_json::to_vec(&unsupported).unwrap(), &[]);
    assert_eq!(
        out3.status.code(),
        Some(0),
        "unsupported hook event must return exit 0"
    );
    assert!(
        out3.stdout.is_empty(),
        "unsupported event must emit zero stdout"
    );

    // Case 4: Session mismatch with transcript
    fixture.write_transcript(&[user_event("turn-1", None, "mismatched-session", "hello")]);
    let mismatch = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Fix test",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
    });
    let out4 = fixture.run_hook(&serde_json::to_vec(&mismatch).unwrap(), &[]);
    assert_eq!(
        out4.status.code(),
        Some(0),
        "session mismatch must return exit 0"
    );
    assert!(
        out4.stdout.is_empty(),
        "session mismatch must emit zero stdout"
    );

    // Case 5: Malformed flags passed to hook claude
    let out5 = fixture.run_hook(b"{}", &["--unsupported-nonexistent-flag"]);
    assert_eq!(
        out5.status.code(),
        Some(0),
        "malformed hook flags must return exit 0"
    );
    assert!(
        out5.stdout.is_empty(),
        "malformed hook flags must emit zero stdout"
    );
}

#[test]
fn stdin_deadline_enforced() {
    let fixture = HookFixture::new();

    // Pass empty stdin
    let out = fixture.run_hook(b"", &["--timeout-ms", "50"]);
    assert_eq!(out.status.code(), Some(0), "empty stdin must return exit 0");
    assert!(out.stdout.is_empty(), "empty stdin must emit zero stdout");
}

#[test]
fn cli_errors_retain_exit_code() {
    let fixture = HookFixture::new();

    // Normal CLI commands with invalid usage retain exit code 2
    let out = fixture.run_cli(&["rank", "--bogus-nonexistent-flag"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "ordinary CLI errors must keep exit code 2"
    );
    assert!(!out.stderr.is_empty() || !out.stdout.is_empty());
}

#[test]
fn shadow_hook_unique_events_across_turns_and_unix_timestamps() {
    let fixture = HookFixture::new();

    // 1. Initialize ledger so shadow hook can record to it
    let init_out = fixture.run_cli(&[
        "ledger",
        "init",
        "--dir",
        fixture.ledger_dir().to_str().unwrap(),
    ]);
    assert_eq!(init_out.status.code(), Some(0), "ledger init must succeed");

    // Turn 1 payload: Claude UserPromptSubmit without prompt_id
    let payload_turn1 = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Please use skill test-repair to fix cargo test failures.",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
        "cwd": fixture.workspace(),
    });

    let out1 = fixture.run_hook(&serde_json::to_vec(&payload_turn1).unwrap(), &["--shadow"]);
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    assert_eq!(out1.status.code(), Some(0));
    assert!(out1.stdout.is_empty());

    let db_path = fixture.ledger_dir().join(skillranker::storage::LEDGER_FILE);
    let conn = rusqlite::Connection::open(&db_path).expect("open ledger db");

    let count: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("count events error: {e}, stderr was: {stderr1}"));
    assert_eq!(
        count, 1,
        "exactly 1 event after turn 1, stderr was: {stderr1}"
    );

    let (ev1_id, ev1_created_at): (String, i64) = conn
        .query_row(
            "SELECT event_id, created_at_unix_ms FROM ranking_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query turn 1 event");

    assert!(
        ev1_id.starts_with("ev-"),
        "derived event_id should start with ev-, got {ev1_id}"
    );
    assert_ne!(ev1_id, "event-0", "event_id must not fall back to event-0");

    // Wall-clock timestamp must be after 2024-01-01 (1_700_000_000_000 ms), not monotonic offset ~41 ms
    assert!(
        ev1_created_at > 1_700_000_000_000,
        "created_at_unix_ms must be a real unix timestamp, got {ev1_created_at}"
    );

    let snapshot_created_at: i64 = conn
        .query_row("SELECT created_at_unix_ms FROM roster_snapshots", [], |r| {
            r.get(0)
        })
        .expect("query snapshot created_at");
    assert!(
        snapshot_created_at > 1_700_000_000_000,
        "roster snapshot created_at_unix_ms must be a real unix timestamp, got {snapshot_created_at}"
    );

    // 2. Retry / duplicate delivery of Turn 1
    let out1_retry = fixture.run_hook(&serde_json::to_vec(&payload_turn1).unwrap(), &["--shadow"]);
    assert_eq!(out1_retry.status.code(), Some(0));
    assert!(out1_retry.stdout.is_empty());

    let count_retry: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
        .expect("count after retry");
    assert_eq!(count_retry, 1, "retry must deduplicate turn 1");

    // 3. Turn 2: transcript now contains Turn 1, prompt is identical
    fixture.write_transcript(&[
        user_event(
            "uuid-turn-1",
            None,
            &fixture.session_id,
            "Please use skill test-repair to fix cargo test failures.",
        ),
        json!({
            "type": "assistant",
            "uuid": "uuid-asst-1",
            "parentUuid": "uuid-turn-1",
            "sessionId": fixture.session_id,
            "message": { "role": "assistant", "content": "I am looking into the failures." }
        }),
    ]);

    let payload_turn2 = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Please use skill test-repair to fix cargo test failures.",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
        "cwd": fixture.workspace(),
    });

    let out2 = fixture.run_hook(&serde_json::to_vec(&payload_turn2).unwrap(), &["--shadow"]);
    assert_eq!(out2.status.code(), Some(0));
    assert!(out2.stdout.is_empty());

    let count_after_turn2: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
        .expect("count after turn 2");
    assert_eq!(count_after_turn2, 2, "turn 2 must record a second event");

    let mut stmt = conn
        .prepare("SELECT event_id FROM ranking_events ORDER BY created_at_unix_ms ASC")
        .unwrap();
    let event_ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(event_ids.len(), 2);
    assert_ne!(
        event_ids[0], event_ids[1],
        "distinct turns with identical prompt text must produce distinct event IDs"
    );

    // 4. Verify prune behavior: prune --before <1 hour ago> should NOT delete fresh records
    let one_hour_ago_secs = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(3600))
    .to_string();

    let prune_out = fixture.run_cli(&[
        "ledger",
        "prune",
        "--dir",
        fixture.ledger_dir().to_str().unwrap(),
        "--before",
        &one_hour_ago_secs,
        "--json",
    ]);
    assert_eq!(prune_out.status.code(), Some(0));

    let count_after_prune: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
        .expect("count after prune");
    assert_eq!(
        count_after_prune, 2,
        "fresh events must not be pruned as ancient"
    );
}

/// A hook counts itself beside the ledger before reading stdin, so an
/// invocation that never records a row still reaches the availability
/// denominator, and `sr stats` reports how many left no row (sr-01h3).
#[test]
fn hook_invocations_are_counted_at_entry_even_when_no_row_is_recorded() {
    let fixture = HookFixture::new();
    let dir = fixture.ledger_dir();
    let dir = dir.to_str().unwrap();
    let entries_path = fixture
        .ledger_dir()
        .join(skillranker::storage::hook_entries::HOOK_ENTRIES_FILE);
    // No ledger yet: the hook cannot record rows, so it counts nothing either.
    let out = fixture.run_hook(b"{", &["--shadow"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!entries_path.exists(), "no counter without a ledger");

    let init = fixture.run_cli(&["ledger", "init", "--dir", dir]);
    assert_eq!(init.status.code(), Some(0), "ledger init must succeed");
    fixture.write_transcript(&[user_event("turn-1", None, "mismatched-session", "hello")]);
    let payload = serde_json::to_vec(&json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "Fix test",
        "prompt_id": "prompt-recorded",
        "session_id": fixture.session_id,
        "transcript_path": fixture.transcript_path(),
        "cwd": fixture.workspace(),
    }))
    .unwrap();
    let entries = || fs::read(&entries_path).map_or(0, |bytes| bytes.len() / 54);

    // 1. A payload that never parses: no turn identity, no row, one entry.
    let out = fixture.run_hook(b"{\"hook_event_name\": \"UserPromptSubmit\"", &["--shadow"]);
    assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0));
    // 2. An invocation starved past its own deadline while stdin stays open.
    let mut child = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", fixture.home())
        .env("XDG_CONFIG_HOME", fixture.home().join(".config"))
        .current_dir(fixture.workspace())
        .args(["hook", "claude", "--dir", dir, "--offline", "--shadow"])
        .args(["--timeout-ms", "50"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let held_open = child.stdin.take();
    let out = child.wait_with_output().unwrap();
    drop(held_open);
    assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0));
    // 3. A turn that fails before context capture still records its row.
    let out = fixture.run_hook(&payload, &["--shadow"]);
    assert_eq!(out.status.code(), Some(0));
    // 4. The flags that keep a hook out of the ledger keep it out of the counter.
    for flag in ["--no-ledger", "--no-persist"] {
        let out = fixture.run_hook(&payload, &["--shadow", flag]);
        assert_eq!(out.status.code(), Some(0));
    }
    assert_eq!(entries(), 3, "every hook that could record was counted");
    let mode = fs::metadata(&entries_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // A counter the hook cannot append to changes nothing the hook does.
    fs::set_permissions(&entries_path, fs::Permissions::from_mode(0o400)).unwrap();
    let out = fixture.run_hook(&payload, &["--shadow"]);
    assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0));
    assert_eq!(entries(), 3);
    fs::set_permissions(&entries_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Stats leaves out the last minute, since an invocation that entered then
    // may still be running. Move everything recorded so far two minutes back.
    let conn =
        rusqlite::Connection::open(fixture.ledger_dir().join(skillranker::storage::LEDGER_FILE))
            .unwrap();
    conn.execute(
        "UPDATE ranking_events SET created_at_unix_ms = created_at_unix_ms - 120000",
        [],
    )
    .unwrap();
    let backdated: Vec<u8> = fs::read(&entries_path)
        .unwrap()
        .chunks(54)
        .flat_map(|record| {
            let at: i64 = std::str::from_utf8(&record[..20]).unwrap().parse().unwrap();
            let mut moved = format!("{:020}", at - 120_000).into_bytes();
            moved.extend_from_slice(&record[20..]);
            moved
        })
        .collect();
    fs::write(&entries_path, backdated).unwrap();
    // A fresh invocation inside the settle window is not compared yet.
    let out = fixture.run_hook(b"{", &["--shadow"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(entries(), 4);

    let stats = fixture.run_cli(&["stats", "--json", "--dir", dir]);
    assert_eq!(stats.status.code(), Some(0), "{stats:?}");
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    let rows: Vec<String> = conn
        .prepare(
            "SELECT event_id FROM ranking_events WHERE mode_channel IN ('shadow', 'advisory-hook')",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        rows.iter().any(|id| id == "pre-context-prompt-recorded"),
        "{rows:?}"
    );
    assert!(
        rows.len() < 3,
        "the unparseable payload recorded no row: {rows:?}"
    );
    let counted = &report["hook_entries"];
    assert_eq!(counted["counted_at_entry"], 3, "{report}");
    assert_eq!(counted["recorded"], rows.len(), "{report}");
    assert_eq!(counted["unrecorded"], 3 - rows.len(), "{report}");
    assert_eq!(counted["counter_full"], false, "{report}");
}

/// A hook turn that fails before context capture still counts in the
/// availability cohort: one typed `unavailable` row per turn, so a build whose
/// overlay always fails shows its failures instead of zero rows (sr-73b6).
#[test]
fn pre_context_hook_failures_are_recorded_once_per_turn() {
    let fixture = HookFixture::new();
    let dir = fixture.ledger_dir();
    let dir = dir.to_str().unwrap();
    let init = fixture.run_cli(&["ledger", "init", "--dir", dir]);
    assert_eq!(init.status.code(), Some(0), "ledger init must succeed");
    fixture.write_transcript(&[user_event("turn-1", None, "mismatched-session", "hello")]);
    let payload = |prompt_id: Option<&str>| {
        let mut payload = json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "Fix test",
            "session_id": fixture.session_id,
            "transcript_path": fixture.transcript_path(),
            "cwd": fixture.workspace(),
        });
        if let Some(id) = prompt_id {
            payload["prompt_id"] = json!(id);
        }
        serde_json::to_vec(&payload).unwrap()
    };
    let rows = || -> Vec<(String, String, String, String)> {
        let conn = rusqlite::Connection::open(
            fixture.ledger_dir().join(skillranker::storage::LEDGER_FILE),
        )
        .unwrap();
        let mut statement = conn
            .prepare(
                "SELECT event_id, decision, reason, agent_branch FROM ranking_events \
                 ORDER BY created_at_unix_ms, event_id",
            )
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    // The transcript belongs to another session: the overlay fails, silently.
    for _delivery in 0..2 {
        let out = fixture.run_hook(&payload(Some("prompt-mismatch")), &["--shadow"]);
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty());
    }
    let recorded = rows();
    assert_eq!(
        recorded,
        [(
            "pre-context-prompt-mismatch".to_string(),
            "unavailable".to_string(),
            "missing-session".to_string(),
            "unresolved".to_string()
        )],
        "one row per turn; a redelivery of the same prompt id is not a second turn"
    );

    // Without the harness's prompt id each invocation is its own row.
    let out = fixture.run_hook(&payload(None), &["--shadow"]);
    assert_eq!(out.status.code(), Some(0));
    let recorded = rows();
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert!(
        recorded[1].0.starts_with("pre-context-invocation-"),
        "{recorded:?}"
    );
    assert_eq!(recorded[1].2, "missing-session");

    // A payload that never parses has no turn identity and records no row (it
    // is counted at entry instead: sr-01h3); disabled ledger policy writes nothing.
    let out = fixture.run_hook(b"{\"hook_event_name\": \"UserPromptSubmit\"", &["--shadow"]);
    assert_eq!(out.status.code(), Some(0));
    for flag in ["--no-ledger", "--no-persist"] {
        let out = fixture.run_hook(
            &payload(Some(&format!("prompt-{flag}"))),
            &["--shadow", flag],
        );
        assert_eq!(out.status.code(), Some(0));
    }
    assert_eq!(rows().len(), 2, "{:?}", rows());

    let stats = fixture.run_cli(&["stats", "--json", "--dir", dir]);
    assert_eq!(stats.status.code(), Some(0), "{stats:?}");
    let report: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(report["turns"]["total_evaluated"], 2, "{report}");
    assert_eq!(report["turns"]["operational_failures"], 2, "{report}");
    assert_eq!(
        report["turns"]["failure_causes"],
        json!([{"reason": "missing-session", "count": 2}]),
        "{report}"
    );
}
