//! Real-file adversarial checks of the production Claude overlay boundary.
use serde_json::json;
use skillranker::adapter::{ClaudeUserPromptSubmit, UnknownFieldPolicy};
use skillranker::context::overlay::{ClaudeOverlayRequest, apply_claude_prompt_overlay};
use skillranker::output::ContextQuality;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn request(records: &[serde_json::Value], prompt_id: &str) -> ClaudeOverlayRequest {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sr-overlay-safety-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("session.jsonl");
    let bytes = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    fs::write(&path, bytes).unwrap();
    let hook = json!({"hook_event_name":"UserPromptSubmit","session_id":"expected-session","prompt_id":prompt_id,"prompt":"current request","transcript_path":path});
    ClaudeOverlayRequest {
        hook_input: ClaudeUserPromptSubmit::from_json(
            &serde_json::to_vec(&hook).unwrap(),
            UnknownFieldPolicy::RetainAdditive,
        )
        .unwrap(),
        transcript_path: Some(path),
        authorized_root: Some(root),
    }
}
fn event(id: &str, parent: Option<&str>) -> serde_json::Value {
    json!({"type":"user","uuid":id,"parentUuid":parent,"sessionId":"expected-session","message":{"content":"history"}})
}
#[test]
fn wrong_native_session_is_rejected_with_an_honest_matching_twin() {
    let good = request(&[event("a", None)], "new");
    assert!(apply_claude_prompt_overlay(&good).is_ok());
    let mut foreign = event("a", None);
    foreign["sessionId"] = json!("foreign-private-session");
    let bad = request(&[foreign], "new");
    let error = apply_claude_prompt_overlay(&bad).expect_err("foreign transcript must be refused");
    assert!(!format!("{error}").contains("foreign-private-session"));
}
#[test]
fn subagent_sidechain_leaves_do_not_make_the_main_chain_unresolvable() {
    // A session that spawned a subagent has extra leaves: the sidechain's
    // tip. The overlay advises the main agent, so the pending prompt must
    // attach to the main-chain tip, not fail on the extra leaf.
    let sidechain = |id: &str, parent: &str| json!({"type":"assistant","uuid":id,"parentUuid":parent,"isSidechain":true,"sessionId":"expected-session","message":{"role":"assistant","content":[{"type":"text","text":"subagent work"}]}});
    let req = request(
        &[
            event("root", None),
            json!({"type":"assistant","uuid":"task-call","parentUuid":"root","sessionId":"expected-session","message":{"role":"assistant","content":[{"type":"tool_use","id":"task-1","name":"Task","input":{"prompt":"scout"}}]}}),
            sidechain("sub-1", "task-call"),
            sidechain("sub-2", "sub-1"),
            json!({"type":"user","uuid":"task-result","parentUuid":"task-call","sessionId":"expected-session","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task-1","is_error":false,"content":"done"}]}}),
        ],
        "new",
    );
    let outcome = apply_claude_prompt_overlay(&req);
    assert!(
        outcome.is_ok(),
        "sidechain leaves must not make the overlay unresolvable: {outcome:?}"
    );
}

#[test]
fn harness_internal_records_are_skipped_but_corruption_stays_fatal() {
    // Current Claude transcripts interleave attachment, queue, title, and mode
    // records with conversation records; none of them may fail the overlay.
    let internal = |t: &str, id: &str| json!({"type":t,"uuid":id,"sessionId":"expected-session","timestamp":"2026-09-22T00:00:00.000Z"});
    let req = request(
        &[
            event("root", None),
            internal("attachment", "h1"),
            internal("queue-operation", "h2"),
            internal("ai-title", "h3"),
            internal("mode", "h4"),
            internal("permission-mode", "h5"),
            internal("atis-latch", "h6"),
            internal("last-prompt", "h7"),
            internal("file-history-snapshot", "h8"),
            event("a", Some("root")),
        ],
        "a",
    );
    assert!(
        apply_claude_prompt_overlay(&req).is_ok(),
        "harness-internal records must not fail the overlay"
    );

    // Honest counterpart: genuinely malformed JSON is still a fatal read error.
    static NEXT_BAD: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sr-overlay-corrupt-{}-{}",
        std::process::id(),
        NEXT_BAD.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("session.jsonl");
    fs::write(
        &path,
        "{\"type\":\"user\",\"uuid\":\"a\",\"sessionId\":\"expected-session\",\"message\":{\"content\":\"x\"}}\n{not-json}\n",
    )
    .unwrap();
    let hook = json!({"hook_event_name":"UserPromptSubmit","session_id":"expected-session","prompt_id":"new","prompt":"current request","transcript_path":path});
    let bad = ClaudeOverlayRequest {
        hook_input: ClaudeUserPromptSubmit::from_json(
            &serde_json::to_vec(&hook).unwrap(),
            UnknownFieldPolicy::RetainAdditive,
        )
        .unwrap(),
        transcript_path: Some(path),
        authorized_root: Some(root),
    };
    assert!(
        apply_claude_prompt_overlay(&bad).is_err(),
        "genuinely corrupt JSON must still fail the overlay"
    );
}

#[test]
fn pending_prompt_cannot_choose_a_sibling_by_file_order() {
    let good = request(&[event("root", None), event("a", Some("root"))], "new");
    assert!(apply_claude_prompt_overlay(&good).is_ok());
    let bad = request(
        &[
            event("root", None),
            event("a", Some("root")),
            event("b", Some("root")),
        ],
        "new",
    );
    assert!(apply_claude_prompt_overlay(&bad).is_err());
}
#[test]
fn recorded_prompt_selects_only_its_ancestor_lineage() {
    let req = request(
        &[
            event("root", None),
            event("a", Some("root")),
            event("b", Some("root")),
        ],
        "a",
    );
    let result = apply_claude_prompt_overlay(&req).unwrap();
    assert_eq!(result.events.len(), 2);
    assert!(
        !result
            .events
            .iter()
            .any(|e| e.event_id.as_ref().is_some_and(|id| id.as_str() == "b"))
    );
    assert_eq!(result.current_request.text.as_str(), "current request");
}
#[test]
fn duplicate_event_ids_are_not_last_writer_authority() {
    let req = request(&[event("a", None), event("a", None)], "new");
    assert!(apply_claude_prompt_overlay(&req).is_err());
}
#[test]
fn incomplete_tail_cannot_claim_complete_context() {
    let req = request(&[event("a", None)], "new");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(req.transcript_path.as_ref().unwrap())
        .unwrap();
    f.write_all(b"{\"type\":").unwrap();
    let result = apply_claude_prompt_overlay(&req).unwrap();
    assert_eq!(result.context_quality, ContextQuality::Partial);
}
#[test]
fn nonexistent_outside_path_does_not_bypass_root_authority() {
    let mut req = request(&[], "new");
    let root = req.authorized_root.as_ref().unwrap();
    req.transcript_path =
        Some(PathBuf::from(root.parent().unwrap()).join("not-authorized-missing.jsonl"));
    assert!(apply_claude_prompt_overlay(&req).is_err());
}

#[test]
fn distinct_native_roots_require_selection() {
    let req = request(&[event("a", None), event("b", None)], "new");
    assert!(apply_claude_prompt_overlay(&req).is_err());
    let selected = request(&[event("a", None), event("b", None)], "a");
    let result = apply_claude_prompt_overlay(&selected).unwrap();
    assert_eq!(result.events.len(), 1);
    assert_eq!(result.events[0].text.as_str(), "current request");
}

#[test]
fn record_limit_preserves_newest_lineage_and_reports_partial() {
    let records = (0..2_005)
        .map(|i| {
            let parent = (i > 0).then(|| format!("e{}", i - 1));
            event(&format!("e{i}"), parent.as_deref())
        })
        .collect::<Vec<_>>();
    let req = request(&records, "new");
    let result = apply_claude_prompt_overlay(&req).unwrap();
    assert_eq!(result.context_quality, ContextQuality::Partial);
    assert_eq!(result.events.len(), 2_001);
    assert_eq!(result.events[0].event_id.as_ref().unwrap().as_str(), "e5");
    assert_eq!(
        result.events.last().unwrap().text.as_str(),
        "current request"
    );
}

#[test]
fn byte_limit_does_not_parse_old_history_outside_the_tail() {
    let mut records = (0..30)
        .map(|i| {
            let parent = (i > 0).then(|| format!("e{}", i - 1));
            let mut record = event(&format!("e{i}"), parent.as_deref());
            record["message"]["content"] = json!("x".repeat(100_000));
            record
        })
        .collect::<Vec<_>>();
    records[0]["sessionId"] = json!("old-history-outside-snapshot");
    let req = request(&records, "new");
    let result = apply_claude_prompt_overlay(&req).unwrap();
    assert_eq!(result.context_quality, ContextQuality::Partial);
    assert!(result.events.len() < 30);
    assert_eq!(
        result.events.last().unwrap().text.as_str(),
        "current request"
    );
}

#[test]
fn expired_shared_deadline_refuses_even_prompt_only_output() {
    use skillranker::context::overlay::{OverlayError, apply_claude_prompt_overlay_before};
    use skillranker::{limits::DurationMillis, runtime::EntryClock};
    let mut req = request(&[], "new");
    req.transcript_path = None;
    req.hook_input.transcript_path = None;
    let clock = EntryClock::capture_with(
        DurationMillis::new("test-total", 2, 100).unwrap(),
        DurationMillis::new("test-cleanup", 1, 100).unwrap(),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    assert_eq!(
        apply_claude_prompt_overlay_before(&req, &clock).unwrap_err(),
        OverlayError::Deadline
    );
    assert!(apply_claude_prompt_overlay(&req).is_ok());
}

#[test]
fn directory_diagnostic_omits_private_paths() {
    let mut req = request(&[], "new");
    let path = req
        .authorized_root
        .as_ref()
        .unwrap()
        .join("private-directory-name");
    fs::create_dir(&path).unwrap();
    req.transcript_path = Some(path);
    let err = apply_claude_prompt_overlay(&req).unwrap_err();
    assert!(!err.to_string().contains("private-directory-name"));
}

#[test]
fn pending_prompt_without_event_id_is_retained_once_after_native_lineage() {
    let mut req = request(&[event("root", None), event("a", Some("root"))], "unused");
    req.hook_input.prompt_id = None;
    let result = apply_claude_prompt_overlay(&req).unwrap();
    assert_eq!(result.events.len(), 3);
    assert_eq!(
        result.events.last().unwrap().text.as_str(),
        "current request"
    );
    assert_eq!(result.active_branch.unwrap().events, result.events);
}

#[test]
fn prompt_identity_cannot_retype_an_assistant_event() {
    let mut assistant = event("a", None);
    assistant["type"] = json!("assistant");
    let req = request(&[assistant], "a");
    assert!(apply_claude_prompt_overlay(&req).is_err());
}

#[cfg(unix)]
#[test]
fn authorized_symlink_works_but_dangling_link_is_not_a_first_prompt() {
    let mut req = request(&[event("a", None)], "new");
    let root = req.authorized_root.as_ref().unwrap();
    let good = root.join("good-link");
    std::os::unix::fs::symlink("session.jsonl", &good).unwrap();
    req.transcript_path = Some(good);
    assert!(apply_claude_prompt_overlay(&req).is_ok());
    let bad = root.join("dangling-link");
    std::os::unix::fs::symlink("missing.jsonl", &bad).unwrap();
    req.transcript_path = Some(bad);
    assert!(apply_claude_prompt_overlay(&req).is_err());
}
