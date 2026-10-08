#![cfg(unix)]

use skillranker::context::discovery::{
    HEAD_BYTES, TAIL_BYTES, attribution, discover_claude_sessions,
    discover_claude_sessions_before_cleanup, parse_utc_ms,
};
use skillranker::identity::WorkspaceId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn tree() -> (PathBuf, PathBuf, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sr-session-discovery-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(workspace).unwrap();
    let projects = root.join("projects");
    let directory = projects.join(workspace.to_str().unwrap().replace('/', "-"));
    std::fs::create_dir_all(&directory).unwrap();
    (workspace, projects, directory)
}

fn record(workspace: &std::path::Path, session: &str) -> String {
    serde_json::json!({"cwd": workspace, "sessionId": session}).to_string() + "\n"
}

#[test]
fn unresolved_neighbor_prevents_a_false_unique_inventory() {
    for unresolved in ["not json\n".to_owned(), "{}\n".to_owned(), "{".to_owned()] {
        let (workspace, projects, directory) = tree();
        std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
        std::fs::write(directory.join("unknown.jsonl"), unresolved).unwrap();
        let inventory = discover_claude_sessions(
            &[projects],
            &workspace,
            &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
        );
        assert_eq!(
            inventory.candidates.len(),
            1,
            "retain the verified candidate"
        );
        assert!(!inventory.complete, "unresolved is not proven absent");
    }
}

#[test]
fn a_head_bound_cannot_hide_another_session() {
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
    let delayed = " ".repeat(HEAD_BYTES) + &record(&workspace, "delayed");
    std::fs::write(directory.join("delayed.jsonl"), delayed).unwrap();
    let inventory = discover_claude_sessions(
        &[projects],
        &workspace,
        &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
    );
    assert_eq!(inventory.candidates.len(), 1);
    assert!(!inventory.complete);
}

#[test]
fn verified_foreign_session_does_not_prevent_unique_discovery() {
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
    std::fs::write(
        directory.join("foreign.jsonl"),
        record(std::path::Path::new("/another/workspace"), "foreign"),
    )
    .unwrap();
    let inventory = discover_claude_sessions(
        &[projects.clone(), projects],
        &workspace,
        &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
    );
    assert!(inventory.complete);
    assert_eq!(inventory.candidates.len(), 1);
    assert_eq!(
        inventory.candidates[0]
            .identity
            .session
            .as_ref()
            .unwrap()
            .as_str(),
        "known"
    );
}

#[test]
fn duplicate_identity_keys_cannot_attribute_a_transcript() {
    let duplicate = b"{\"cwd\":\"/foreign\",\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n";
    assert!(attribution(duplicate, "s", "/workspace").is_none());
    let duplicate = b"{\"cwd\":\"/workspace\",\"sessionId\":\"other\",\"sessionId\":\"s\"}\n";
    assert!(attribution(duplicate, "s", "/workspace").is_none());
    assert!(
        attribution(
            b"{\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n",
            "s",
            "/workspace"
        )
        .is_some()
    );
}

#[test]
fn conflicting_identity_later_in_the_head_invalidates_attribution() {
    let head = b"{\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n{\"sessionId\":\"other\"}\n";
    assert!(attribution(head, "s", "/workspace").is_none());
    let consistent = b"{\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n{\"cwd\":\"/later/cd\",\"sessionId\":\"s\"}\n";
    assert!(attribution(consistent, "s", "/workspace").is_some());
}

fn inventory_of(
    workspace: &std::path::Path,
    projects: PathBuf,
) -> skillranker::context::source::SessionInventory {
    discover_claude_sessions(
        &[projects],
        workspace,
        &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
    )
}

fn timed(workspace: &std::path::Path, session: &str, timestamp: &str) -> String {
    serde_json::json!({"cwd": workspace, "sessionId": session, "timestamp": timestamp}).to_string()
        + "\n"
}

#[test]
fn recency_is_the_last_complete_recorded_time_not_the_file_time() {
    let (workspace, projects, directory) = tree();
    let path = directory.join("s.jsonl");
    // An unfinished last record does not count; the file's own time is ignored.
    let text = timed(&workspace, "s", "2026-09-19T09:00:00Z")
        + &timed(&workspace, "s", "2026-09-19T10:00:00.250Z")
        + r#"{"sessionId": "s", "timestamp": "2026-09-19T11:00:00Z""#;
    std::fs::write(&path, text).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    let inventory = inventory_of(&workspace, projects);
    assert!(inventory.complete);
    assert_eq!(
        inventory.candidates[0].last_activity_unix_ms,
        parse_utc_ms("2026-09-19T10:00:00.250Z")
    );
}

#[test]
fn a_long_transcript_takes_recency_from_its_tail() {
    let (workspace, projects, directory) = tree();
    let mut text = String::new();
    while text.len() < 2 * (HEAD_BYTES + TAIL_BYTES) {
        text += &timed(&workspace, "s", "2026-09-19T09:00:00Z");
    }
    text += &timed(&workspace, "s", "2026-09-19T12:34:56Z");
    std::fs::write(directory.join("s.jsonl"), text).unwrap();
    let inventory = inventory_of(&workspace, projects);
    assert!(inventory.complete);
    assert_eq!(
        inventory.candidates[0].last_activity_unix_ms,
        parse_utc_ms("2026-09-19T12:34:56Z")
    );
}

#[test]
fn a_session_without_recorded_times_has_unknown_recency() {
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("s.jsonl"), record(&workspace, "s")).unwrap();
    let inventory = inventory_of(&workspace, projects);
    assert!(inventory.complete);
    assert_eq!(inventory.candidates[0].last_activity_unix_ms, None);
}

#[test]
fn impossible_calendar_dates_cannot_establish_recency() {
    for timestamp in [
        "2026-02-29T12:00:00Z",
        "1900-02-29T12:00:00Z",
        "2100-02-29T12:00:00Z",
        "2024-02-30T12:00:00Z",
        "2026-04-31T12:00:00Z",
        "2026-06-31T12:00:00Z",
        "2026-09-31T12:00:00Z",
        "2026-11-31T12:00:00Z",
    ] {
        assert_eq!(parse_utc_ms(timestamp), None, "{timestamp}");
        let (workspace, projects, directory) = tree();
        std::fs::write(directory.join("s.jsonl"), timed(&workspace, "s", timestamp)).unwrap();
        let inventory = inventory_of(&workspace, projects);
        assert!(inventory.complete);
        assert_eq!(inventory.candidates[0].last_activity_unix_ms, None);
    }
    for timestamp in [
        "2000-02-29T12:00:00Z",
        "2024-02-29T12:00:00Z",
        "2400-02-29T12:00:00Z",
        "2026-02-28T12:00:00Z",
        "2026-04-30T12:00:00Z",
        "2026-01-31T12:00:00Z",
    ] {
        assert!(parse_utc_ms(timestamp).is_some(), "{timestamp}");
    }
}

#[test]
fn native_discovery_preserves_expiry_and_cancellation_instead_of_partial_inventory() {
    use skillranker::limits::DurationMillis;
    use skillranker::runtime::{EntryClock, ProcessInvocation, RuntimeError};
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
    let workspace_id = WorkspaceId::new(workspace.to_str().unwrap()).unwrap();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let roots = [projects];
    let healthy = discover_claude_sessions_before_cleanup(
        &roots,
        &workspace,
        &workspace_id,
        &cx,
        &invocation.clock(),
    )
    .unwrap();
    assert!(healthy.complete);
    assert_eq!(healthy.candidates.len(), 1);

    let expired = EntryClock::capture_with(
        DurationMillis::new("test_total", 201, 3_000).unwrap(),
        DurationMillis::new("test_reserve", 200, 3_000).unwrap(),
    )
    .unwrap();
    while expired.admit_new_work().is_ok() {
        std::thread::yield_now();
    }
    assert!(matches!(
        discover_claude_sessions_before_cleanup(&roots, &workspace, &workspace_id, &cx, &expired),
        Err(RuntimeError::Deadline(_))
    ));
    invocation.cancel_user(&cx);
    assert!(matches!(
        discover_claude_sessions_before_cleanup(
            &roots,
            &workspace,
            &workspace_id,
            &cx,
            &invocation.clock()
        ),
        Err(RuntimeError::Cancelled)
    ));
    assert!(invocation.shutdown());
}

#[test]
fn latest_requires_valid_calendar_recency_through_the_cli() {
    let (workspace, projects, directory) = tree();
    let root = workspace.parent().unwrap();
    std::fs::create_dir_all(root.join("config/sr")).unwrap();
    std::fs::write(
        root.join("config/sr/config.toml"),
        format!("[context]\ntranscript_roots = ['{}']\n", projects.display()),
    )
    .unwrap();
    let skill = workspace.join(".claude/skills/test-review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: test-review\ndescription: Review code carefully\n---\nInspect code.\n",
    )
    .unwrap();
    let write = |session: &str, timestamp: &str| {
        std::fs::write(
            directory.join(format!("{session}.jsonl")),
            serde_json::json!({
                "type":"user", "uuid":format!("{session}-turn"), "parentUuid":null,
                "sessionId":session, "cwd":workspace, "timestamp":timestamp,
                "message":{"role":"user", "content":"Review code"}
            })
            .to_string()
                + "\n",
        )
        .unwrap();
    };
    write("older", "2026-02-28T12:00:00Z");
    for (timestamp, expected_exit) in [("2026-02-29T12:00:00Z", 3), ("2026-03-01T12:00:00Z", 0)] {
        write("newer", timestamp);
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .current_dir(&workspace)
            .stdin(std::process::Stdio::null())
            .args([
                "rank",
                "--latest",
                "--offline",
                "--no-persist",
                "--json",
                "--require-skill",
                "test-review",
            ])
            .output()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{value}; {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if expected_exit == 3 {
            assert_eq!(value["error"]["kind"], "ambiguous-session");
        } else {
            assert_eq!(value["decision"], "explicit");
            assert_eq!(value["event_id"], "newer-turn");
            assert_eq!(value["usage"]["http_attempts"], 0);
        }
    }
}

#[test]
fn an_empty_stub_session_does_not_block_unique_discovery() {
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
    // Claude leaves such stubs: session metadata, never a working directory.
    let stub = [
        serde_json::json!({"type": "last-prompt", "sessionId": "stub"}),
        serde_json::json!({"type": "ai-title", "sessionId": "stub"}),
    ]
    .map(|record| record.to_string() + "\n")
    .concat();
    std::fs::write(directory.join("stub.jsonl"), stub).unwrap();
    let inventory = inventory_of(&workspace, projects);
    assert!(inventory.complete, "a whole stub is proven not a candidate");
    assert_eq!(inventory.candidates.len(), 1);
}
