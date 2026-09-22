//! Real-file adversarial checks of the production Claude overlay boundary.
use serde_json::json;
use skillranker::adapter::{ClaudeUserPromptSubmit, UnknownFieldPolicy};
use skillranker::context::overlay::{
    ClaudeOverlayRequest, OverlayError, apply_claude_prompt_overlay,
};
use skillranker::output::ContextQuality;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn request(records: &[serde_json::Value], prompt_id: &str) -> ClaudeOverlayRequest {
    let lines: Vec<String> = records.iter().map(ToString::to_string).collect();
    request_lines(&lines, prompt_id)
}

/// Like [`request`], from raw JSONL lines, for records `json!` cannot build.
fn request_lines(lines: &[String], prompt_id: &str) -> ClaudeOverlayRequest {
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
    let bytes = lines.iter().map(|l| format!("{l}\n")).collect::<String>();
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
/// The real shape from live sessions: a PreToolUse hook (RCH intercepting a
/// cargo command) writes an `attachment/hook_success` record as a dead-end
/// child of a node whose conversation continues elsewhere.
fn hook_success(id: &str, parent: &str) -> serde_json::Value {
    json!({"type":"attachment","uuid":id,"parentUuid":parent,"isSidechain":false,"sessionId":"expected-session",
           "attachment":{"type":"hook_success","hookName":"PreToolUse:Bash","hookEvent":"PreToolUse","exitCode":0,"content":"","stdout":"","stderr":""}})
}
fn tool_call(id: &str, parent: &str) -> serde_json::Value {
    json!({"type":"assistant","uuid":id,"parentUuid":parent,"sessionId":"expected-session",
           "message":{"role":"assistant","content":[{"type":"tool_use","id":format!("call-{id}"),"name":"Bash","input":{"command":"cargo test"}}]}})
}
fn tool_result(id: &str, parent: &str) -> serde_json::Value {
    json!({"type":"user","uuid":id,"parentUuid":parent,"sessionId":"expected-session",
           "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":format!("call-{parent}"),"is_error":false,"content":"ok"}]}})
}

#[test]
fn hook_success_dead_ends_do_not_make_a_pending_prompt_unresolvable() {
    let mut records = vec![event("root", None)];
    let mut tip = "root".to_owned();
    for n in 0..4 {
        let call = format!("call{n}");
        let result = format!("result{n}");
        records.push(tool_call(&call, &tip));
        records.push(hook_success(&format!("hook{n}"), &call));
        records.push(tool_result(&result, &call));
        tip = result;
    }
    // A chain of two content-free records is also a dead end.
    records.push(hook_success("hook-a", "result1"));
    records.push(hook_success("hook-b", "hook-a"));
    // A blocked command's hook error, and the parentless informational
    // record Claude writes when it loads instructions at session start.
    records.push(json!({"type":"attachment","uuid":"hook-err","parentUuid":"call2","sessionId":"expected-session",
        "attachment":{"type":"hook_non_blocking_error","hookName":"PreToolUse:Bash","stderr":"BLOCKED"}}));
    records.push(
        json!({"type":"system","subtype":"informational","uuid":"info","parentUuid":null,
        "sessionId":"expected-session","content":"AGENTS.md loaded","level":"info"}),
    );
    let result = apply_claude_prompt_overlay(&request(&records, "new"))
        .expect("content-free dead ends must not block the pending prompt");
    let ids: Vec<&str> = result
        .events
        .iter()
        .filter_map(|e| e.event_id.as_ref().map(|id| id.as_str()))
        .collect();
    assert!(
        ids.contains(&"result3"),
        "the real tip is on the branch: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.starts_with("hook")),
        "dead ends are not part of the resolved lineage: {ids:?}"
    );
    assert_eq!(result.current_request.text.as_str(), "current request");
}

#[test]
fn a_real_fork_stays_ambiguous_even_beside_hook_dead_ends() {
    // Pruning removes only content-free leaves. Two leaves that carry
    // conversation remain a fork, with or without dead ends present.
    let fork = [
        event("root", None),
        tool_call("call", "root"),
        hook_success("hook", "call"),
        event("a", Some("call")),
        event("b", Some("call")),
    ];
    assert!(apply_claude_prompt_overlay(&request(&fork, "new")).is_err());
    let plain_fork = [
        event("root", None),
        event("a", Some("root")),
        event("b", Some("root")),
    ];
    assert!(apply_claude_prompt_overlay(&request(&plain_fork, "new")).is_err());
}

#[test]
fn parallel_tool_results_filed_beside_their_calls_are_side_branches() {
    // Claude's layout for one message with three parallel calls: each call's
    // result is a child of that call, while the next call continues the
    // chain. Only the last result carries the conversation onward. A call a
    // PreToolUse hook blocked adds an error attachment under its result.
    let blocked = json!({"type":"attachment","uuid":"blocked","parentUuid":"b","sessionId":"expected-session",
        "attachment":{"type":"hook_non_blocking_error","hookName":"PreToolUse:Bash","stderr":"BLOCKED"}});
    let records = [
        event("root", None),
        tool_call("a", "root"),
        tool_call("b", "a"),
        tool_result("result-a", "a"),
        tool_call("c", "b"),
        tool_result("result-b", "b"),
        blocked,
        tool_result("result-c", "c"),
        json!({"type":"assistant","uuid":"reply","parentUuid":"result-c","sessionId":"expected-session",
               "message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
    ];
    let result = apply_claude_prompt_overlay(&request(&records, "new"))
        .expect("parallel tool results must not make the pending prompt unresolvable");
    let ids: Vec<&str> = result
        .events
        .iter()
        .filter_map(|e| e.event_id.as_ref().map(|id| id.as_str()))
        .collect();
    assert!(
        ids.contains(&"reply"),
        "the real tip is on the branch: {ids:?}"
    );
    assert!(!ids.contains(&"result-a") && !ids.contains(&"result-b"));
}

/// A tool call written as one fragment of the API response `response`.
fn batch_call(id: &str, parent: &str, response: &str) -> serde_json::Value {
    let mut call = tool_call(id, parent);
    call["message"]["id"] = json!(response);
    call
}

/// Claude's layout when tools of one response run concurrently: the response
/// goes on through `b`, while call `a`'s result starts a side branch that
/// carries three more calls of the same response, each answered.
fn interleaved_batches() -> Vec<serde_json::Value> {
    vec![
        event("root", None),
        batch_call("a", "root", "msg-1"),
        batch_call("b", "a", "msg-1"),
        tool_result("result-a", "a"),
        hook_success("hook-a", "result-a"),
        batch_call("c", "hook-a", "msg-1"),
        batch_call("d", "c", "msg-1"),
        batch_call("e", "d", "msg-1"),
        tool_result("result-b", "b"),
        hook_success("hook-b", "result-b"),
        batch_call("f", "hook-b", "msg-1"),
        tool_result("result-c", "c"),
        hook_success("hook-c", "result-c"),
        tool_result("result-d", "d"),
        tool_result("result-e", "e"),
        hook_success("hook-e", "result-e"),
        tool_result("result-f", "f"),
        json!({"type":"assistant","uuid":"reply","parentUuid":"result-f","sessionId":"expected-session",
               "message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"done"}]}}),
    ]
}

fn record<'r>(records: &'r mut [serde_json::Value], uuid: &str) -> &'r mut serde_json::Value {
    records.iter_mut().find(|r| r["uuid"] == uuid).unwrap()
}

#[test]
fn an_answered_parallel_batch_of_the_continuing_response_is_a_side_branch() {
    let result = apply_claude_prompt_overlay(&request(&interleaved_batches(), "new"))
        .expect("the response continues through b; its answered side batch is no fork");
    let ids: Vec<_> = result
        .events
        .iter()
        .filter_map(|e| e.event_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();
    assert_eq!(
        ids,
        [
            "root", "a", "b", "result-b", "hook-b", "f", "result-f", "reply", "new"
        ]
    );
}

/// The response goes on through call `a`'s own result, while call `b` heads a
/// side batch whose calls fork again: `b`'s result and call `c` each end the
/// nested fork answered, so neither of them continues.
fn nested_batches() -> Vec<serde_json::Value> {
    vec![
        event("root", None),
        batch_call("a", "root", "msg-1"),
        batch_call("b", "a", "msg-1"),
        tool_result("result-a", "a"),
        batch_call("c", "b", "msg-1"),
        tool_result("result-b", "b"),
        hook_success("hook-b", "result-b"),
        tool_result("result-c", "c"),
        json!({"type":"assistant","uuid":"reply","parentUuid":"result-a","sessionId":"expected-session",
               "message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"done"}]}}),
    ]
}

#[test]
fn a_nested_side_batch_is_pruned_whole_when_the_calls_own_result_goes_on() {
    let result = apply_claude_prompt_overlay(&request(&nested_batches(), "new"))
        .expect("the response continues through a's result; b's batch is finished");
    let ids: Vec<_> = result
        .events
        .iter()
        .filter_map(|e| e.event_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();
    assert_eq!(ids, ["root", "a", "result-a", "reply", "new"]);
    // Response identity is what separates this layout from a rewind.
    let mut records = nested_batches();
    for record in &mut records {
        if let Some(message) = record["message"].as_object_mut() {
            message.remove("id");
        }
    }
    assert!(apply_claude_prompt_overlay(&request(&records, "new")).is_err());
    // With no later response, both branches are finished batches and neither
    // shows where the conversation goes.
    let finished = [
        event("root", None),
        batch_call("a", "root", "msg-1"),
        batch_call("b", "a", "msg-1"),
        tool_result("result-b", "b"),
        tool_result("result-a", "a"),
        hook_success("hook-a", "result-a"),
        batch_call("d", "hook-a", "msg-1"),
        tool_result("result-d", "d"),
    ];
    assert!(apply_claude_prompt_overlay(&request(&finished, "new")).is_err());
}

#[test]
fn an_interleaved_batch_forks_unless_it_belongs_to_the_continuing_response() {
    // Each twin breaks one condition of the side-batch layout; each must fail.
    let mut twins = Vec::new();
    // The side branch holds a fragment of a different response.
    let mut records = interleaved_batches();
    record(&mut records, "d")["message"]["id"] = json!("msg-other");
    twins.push(("foreign response", records));
    // The response does not continue through the fork's other child.
    let mut records = interleaved_batches();
    record(&mut records, "b")["message"]["id"] = json!("msg-other");
    twins.push(("different continuing response", records));
    // Without response identity the layout is indistinguishable from a rewind.
    let mut records = interleaved_batches();
    for record in &mut records {
        if let Some(message) = record["message"].as_object_mut() {
            message.remove("id");
        }
    }
    twins.push(("no response identity", records));
    // A call of the side branch is still unanswered.
    let mut records = interleaved_batches();
    records.retain(|r| r["uuid"] != "result-d");
    twins.push(("unanswered call", records));
    // A user message on the side branch is conversation, not a tool batch.
    let mut records = interleaved_batches();
    records.push(event("typed", Some("hook-e")));
    twins.push(("user turn", records));
    // A result answering a call outside the batch.
    let mut records = interleaved_batches();
    record(&mut records, "result-e")["message"]["content"][0]["tool_use_id"] =
        json!("call-elsewhere");
    twins.push(("foreign result", records));
    for (name, records) in twins {
        assert!(
            apply_claude_prompt_overlay(&request(&records, "new")).is_err(),
            "{name} must stay an ambiguous fork"
        );
    }
}

/// Claude's automatic compaction: the post-compaction chain starts at a
/// parentless boundary that names the pre-compaction tip only as its
/// logical parent.
fn compacted() -> Vec<serde_json::Value> {
    vec![
        event("root", None),
        tool_call("old-call", "root"),
        tool_result("old-result", "old-call"),
        hook_success("old-tip", "old-result"),
        json!({"type":"system","subtype":"compact_boundary","uuid":"boundary","parentUuid":null,
               "logicalParentUuid":"old-tip","sessionId":"expected-session","content":"Conversation compacted",
               "compactMetadata":{"trigger":"auto","preservedSegment":{"headUuid":"old-call","anchorUuid":"summary","tailUuid":"old-tip"}}}),
        json!({"type":"user","uuid":"summary","parentUuid":"boundary","isCompactSummary":true,"sessionId":"expected-session",
               "message":{"role":"user","content":"summary of the earlier conversation"}}),
        json!({"type":"assistant","uuid":"reply","parentUuid":"summary","sessionId":"expected-session",
               "message":{"role":"assistant","content":[{"type":"text","text":"continuing"}]}}),
    ]
}

#[test]
fn a_compaction_boundary_continues_the_pre_compaction_tip() {
    let result = apply_claude_prompt_overlay(&request(&compacted(), "new"))
        .expect("the boundary's logical parent is the old tip, not a second leaf");
    let branch = result.active_branch.expect("resolved branch");
    let ids: Vec<_> = branch
        .events
        .iter()
        .filter_map(|e| e.event_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();
    assert_eq!(
        ids,
        [
            "root",
            "old-call",
            "old-result",
            "old-tip",
            "boundary",
            "summary",
            "reply",
            "new"
        ]
    );
    assert_eq!(branch.compaction_count, 1);
    // Without the logical link the boundary is a second root, and the old
    // tip stays a leaf the pending prompt cannot choose against.
    let mut records = compacted();
    records[4]
        .as_object_mut()
        .unwrap()
        .remove("logicalParentUuid");
    assert!(apply_claude_prompt_overlay(&request(&records, "new")).is_err());
}

/// A chain whose middle tool result is over the 256 KiB record limit.
fn oversized_result_chain() -> Vec<serde_json::Value> {
    let mut big = tool_result("big-result", "call");
    big["toolUseResult"] = json!({ "stdout": "x".repeat(300_000) });
    // Real Claude records carry both session key spellings.
    big["session_id"] = json!("expected-session");
    vec![
        event("root", None),
        tool_call("call", "root"),
        big,
        json!({"type":"assistant","uuid":"reply","parentUuid":"big-result","sessionId":"expected-session",
               "message":{"role":"assistant","content":[{"type":"text","text":"read it"}]}}),
    ]
}

#[test]
fn an_oversized_record_keeps_its_lineage_and_reports_partial_context() {
    let result = apply_claude_prompt_overlay(&request(&oversized_result_chain(), "new"))
        .expect("an oversized tool result is over the parse limit, not malformed");
    let ids: Vec<_> = result
        .events
        .iter()
        .filter_map(|e| e.event_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();
    assert_eq!(ids, ["root", "call", "big-result", "reply", "new"]);
    assert_eq!(result.context_quality, ContextQuality::Partial);
    let big = &result.events[2];
    assert!(big.text.as_str().is_empty() && big.tool.is_none());
    // The same chain under the limit is complete: partial comes from the drop.
    let mut small = oversized_result_chain();
    small[2]["toolUseResult"] = json!({ "stdout": "x" });
    let result = apply_claude_prompt_overlay(&request(&small, "new")).unwrap();
    assert_eq!(result.context_quality, ContextQuality::Complete);
}

#[test]
fn an_oversized_record_still_proves_its_session_and_identity() {
    // Another session's record stays a cross-session read, however large,
    // under either session key.
    let mut foreign = oversized_result_chain();
    foreign[2]["session_id"] = json!("foreign-session");
    assert!(matches!(
        apply_claude_prompt_overlay(&request(&foreign, "new")),
        Err(OverlayError::SessionMismatch { .. })
    ));
    let lines: Vec<String> = oversized_result_chain()
        .iter()
        .map(ToString::to_string)
        .collect();
    // A repeated identity key is not a record this reader can trust.
    let mut duplicate = lines.clone();
    duplicate[2] = duplicate[2].replacen("{", "{\"uuid\":\"other\",", 1);
    assert!(apply_claude_prompt_overlay(&request_lines(&duplicate, "new")).is_err());
    // Truncated JSON is corruption, not an oversized record.
    let mut truncated = lines;
    let cut = truncated[2].len() - 2;
    truncated[2].truncate(cut);
    assert!(apply_claude_prompt_overlay(&request_lines(&truncated, "new")).is_err());
}

#[test]
fn an_injected_document_record_is_context_but_a_submitted_one_is_not_dropped() {
    let with_middle = |middle: serde_json::Value| {
        vec![
            event("root", None),
            middle,
            json!({"type":"assistant","uuid":"reply","parentUuid":"doc","sessionId":"expected-session",
                   "message":{"role":"assistant","content":[{"type":"text","text":"read the PDF"}]}}),
        ]
    };
    let document = json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}});
    // Claude injects a document the agent read as a meta user record.
    let injected = json!({"type":"user","uuid":"doc","parentUuid":"root","isMeta":true,"sessionId":"expected-session",
                          "message":{"role":"user","content":[document.clone()]}});
    let result = apply_claude_prompt_overlay(&request(&with_middle(injected), "new"))
        .expect("an injected document record is dropped media, not corruption");
    let doc = &result.events[1];
    assert_eq!(doc.role, skillranker::context::Role::System);
    assert!(doc.text.as_str().is_empty());
    // A prompt the user submitted keeps its text when a document is dropped...
    let submitted = json!({"type":"user","uuid":"doc","parentUuid":"root","sessionId":"expected-session",
                           "message":{"role":"user","content":[{"type":"text","text":"summarize this"}, document.clone()]}});
    let result = apply_claude_prompt_overlay(&request(&with_middle(submitted), "new")).unwrap();
    assert_eq!(result.events[1].text.as_str(), "summarize this");
    // ...but a submitted prompt with nothing usable, or with content this
    // build does not model, cannot be silently dropped.
    for content in [
        json!([document]),
        json!([{"type":"text","text":"do this"}, {"type":"future_block"}]),
    ] {
        let submitted = json!({"type":"user","uuid":"doc","parentUuid":"root","sessionId":"expected-session",
                               "message":{"role":"user","content":content}});
        assert!(apply_claude_prompt_overlay(&request(&with_middle(submitted), "new")).is_err());
    }
}

#[test]
fn a_rewound_tool_exchange_is_still_a_real_fork() {
    // After a rewind, the abandoned branch can end in a tool result whose
    // call has no other child. That branch is not a side record of an
    // ongoing message, so the pending prompt must not pick a side.
    let rewind = [
        event("root", None),
        tool_call("call", "root"),
        tool_result("abandoned", "call"),
        event("edited-prompt", Some("root")),
    ];
    assert!(apply_claude_prompt_overlay(&request(&rewind, "new")).is_err());
    // A result that answers a different call is not its call's side record.
    let mut foreign = tool_result("foreign", "call");
    foreign["message"]["content"][0]["tool_use_id"] = json!("call-elsewhere");
    let mismatched = [
        event("root", None),
        tool_call("call", "root"),
        foreign,
        tool_call("next", "call"),
    ];
    assert!(apply_claude_prompt_overlay(&request(&mismatched, "new")).is_err());
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
