//! Native parser identities must remain usable for branch isolation.
use serde_json::json;
use skillranker::context::jsonl::{SkipKind, parse_line};
use skillranker::context::{BranchResolutionTarget, resolve_active_branch};
use skillranker::identity::{AgentId, BranchId};

fn parse(value: serde_json::Value) -> Result<skillranker::context::NormalizedEvent, SkipKind> {
    parse_line(&serde_json::to_vec(&value).unwrap())
}

#[test]
fn malformed_native_identities_are_not_silently_missing() {
    for field in ["uuid", "parentUuid", "turn_id", "agent_id", "branch_id"] {
        for invalid in [
            json!(""),
            json!("private bad id"),
            json!(17),
            json!({}),
            json!("x".repeat(513)),
        ] {
            let mut record = json!({"type":"user","uuid":"valid","text":"hello"});
            record[field] = invalid;
            assert_eq!(parse(record), Err(SkipKind::Corrupt), "field {field}");
        }
        let mut good = json!({"type":"user","uuid":"valid","text":"hello"});
        good[field] = json!("valid-id");
        assert!(parse(good).is_ok(), "field {field}");
    }
}

#[test]
fn branch_and_agent_selectors_resolve_the_declared_native_leaf() {
    let a = parse(
        json!({"type":"user","uuid":"a","branch_id":"branch-a","agent_id":"agent-a","text":"a"}),
    )
    .unwrap();
    let b = parse(
        json!({"type":"user","uuid":"b","branch_id":"branch-b","agent_id":"agent-b","text":"b"}),
    )
    .unwrap();
    let result = resolve_active_branch(
        &[a, b],
        &BranchResolutionTarget {
            target_event_id: None,
            target_branch_id: Some(BranchId::new("branch-a").unwrap()),
            target_agent_id: Some(AgentId::new("agent-a").unwrap()),
        },
    );
    let branch = result
        .active_branch()
        .expect("declared attribution must select the correct leaf");
    assert_eq!(branch.events.len(), 1);
    assert_eq!(branch.events[0].text.as_str(), "a");
}

#[test]
fn conflicting_native_identity_aliases_are_rejected() {
    for (left, right) in [("event_id", "uuid"), ("parent_id", "parentUuid")] {
        let mut record = json!({"type":"user","text":"hello"});
        record[left] = json!("first");
        record[right] = json!("other");
        assert_eq!(parse(record.clone()), Err(SkipKind::Corrupt));
        record[right] = json!("first");
        assert!(parse(record).is_ok());
    }
}

#[test]
fn invalid_or_conflicting_tool_call_ids_do_not_lose_associations() {
    for invalid in [json!(""), json!("bad call"), json!(32)] {
        assert_eq!(
            parse(json!({"type":"tool_use","call_id":invalid,"name":"Read"})),
            Err(SkipKind::Corrupt)
        );
    }
    assert_eq!(
        parse(json!({"type":"tool_result","call_id":"a","tool_use_id":"b"})),
        Err(SkipKind::Corrupt)
    );
    let event = parse(json!({"type":"tool_result","call_id":"a","tool_use_id":"a"})).unwrap();
    assert_eq!(event.tool.unwrap().call_id.unwrap().as_str(), "a");
}

#[test]
fn omitted_and_null_optional_ids_remain_unknown_not_invented() {
    for record in [
        json!({"type":"user","text":"hello"}),
        json!({"type":"user","uuid":null,"parentUuid":null,"turn_id":null,"agent_id":null,"branch_id":null,"text":"hello"}),
    ] {
        let event = parse(record).unwrap();
        assert!(event.event_id.is_none());
        assert!(event.parent_id.is_none());
        assert!(event.turn_id.is_none());
        assert!(event.agent_id.is_none());
        assert!(event.branch_id.is_none());
    }
    assert_eq!(
        parse(json!({"type":"user","event_id":null,"uuid":"present"})),
        Err(SkipKind::Corrupt)
    );
}

#[test]
fn prior_parser_cursor_rebuilds_so_dropped_attribution_is_recovered() {
    use skillranker::context::jsonl::{CursorKind, PARSER_VERSION, snapshot_jsonl};
    use skillranker::runtime::ProcessInvocation;
    let path = std::env::temp_dir().join(format!(
        "sr-native-identity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(b"{\"type\":\"user\",\"uuid\":\"a\",\"agent_id\":\"agent-a\",\"branch_id\":\"branch-a\",\"text\":\"hello\"}\n").unwrap();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let first = snapshot_jsonl(&invocation, &cx, &path, None, CursorKind::Ranking).unwrap();
    let continued = snapshot_jsonl(
        &invocation,
        &cx,
        &path,
        Some(&first.cursor),
        CursorKind::Ranking,
    )
    .unwrap();
    assert!(!continued.rebuilt);
    assert!(continued.events.is_empty());
    let mut old = first.cursor;
    old.parser_version = 1;
    let rebuilt = snapshot_jsonl(&invocation, &cx, &path, Some(&old), CursorKind::Ranking).unwrap();
    assert!(rebuilt.rebuilt);
    assert!(rebuilt.cursor.generation > old.generation);
    assert_eq!(rebuilt.cursor.parser_version, PARSER_VERSION);
    assert_eq!(
        rebuilt.events[0].agent_id.as_ref().unwrap().as_str(),
        "agent-a"
    );
    assert_eq!(
        rebuilt.events[0].branch_id.as_ref().unwrap().as_str(),
        "branch-a"
    );
    assert!(invocation.shutdown());
}

#[test]
fn rejected_records_share_the_record_budget_and_leave_a_resumable_cursor() {
    use skillranker::context::jsonl::{CursorKind, snapshot_jsonl};
    use skillranker::runtime::ProcessInvocation;
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "sr-native-record-cap-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all("x\n".repeat(2_100).as_bytes()).unwrap();
    file.write_all(b"{\"type\":\"user\",\"uuid\":\"good\",\"text\":\"preserved\"}\n")
        .unwrap();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let first = snapshot_jsonl(&invocation, &cx, &path, None, CursorKind::Observation).unwrap();
    assert_eq!(first.skipped.len(), 2_000);
    assert!(first.events.is_empty());
    assert!(first.unread_backlog);
    assert!(!first.incomplete_tail);
    assert_eq!(first.cursor.byte_offset, 4_000);
    let second = snapshot_jsonl(
        &invocation,
        &cx,
        &path,
        Some(&first.cursor),
        CursorKind::Observation,
    )
    .unwrap();
    assert_eq!(second.skipped.len(), 100);
    assert_eq!(second.events.len(), 1);
    assert_eq!(second.events[0].text.as_str(), "preserved");
    assert!(!second.unread_backlog);
    assert!(invocation.shutdown());
}

#[test]
fn supported_native_block_shapes_preserve_text_and_tool_associations() {
    use serde_json::json;
    use skillranker::context::{EventKind, Role, ToolStatus};

    // 1. Assistant message with content array: text + tool_use
    let assistant_record = json!({
        "type": "assistant",
        "uuid": "ast-01",
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "I will examine the codebase."
                },
                {
                    "type": "tool_use",
                    "id": "call-read-01",
                    "name": "read_file",
                    "input": { "path": "src/main.rs" }
                }
            ]
        }
    });
    let ev = parse(assistant_record).expect("assistant message with blocks must parse");
    assert_eq!(ev.text.as_str(), "I will examine the codebase.");
    assert_eq!(ev.kind, EventKind::ToolInvocation);
    assert_eq!(ev.role, Role::Assistant);
    let tool = ev.tool.expect("tool event must be extracted");
    assert_eq!(tool.call_id.unwrap().as_str(), "call-read-01");
    assert_eq!(tool.name.as_str(), "read_file");
    assert_eq!(tool.status, ToolStatus::Attempted);
    assert_eq!(
        tool.arguments.unwrap().as_str(),
        "{\"path\":\"src/main.rs\"}"
    );

    // 2. User message with content array: tool_result
    let tool_res_record = json!({
        "type": "user",
        "uuid": "usr-01",
        "parentUuid": "ast-01",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "call-read-01",
                    "content": "fn main() {}",
                    "is_error": false
                }
            ]
        }
    });
    let ev = parse(tool_res_record).expect("tool_result block must parse");
    assert_eq!(ev.kind, EventKind::ToolResult);
    assert_eq!(ev.role, Role::Tool);
    let tool = ev.tool.expect("tool event must be extracted");
    assert_eq!(tool.call_id.unwrap().as_str(), "call-read-01");
    assert_eq!(tool.status, ToolStatus::Succeeded);
    assert_eq!(tool.result.unwrap().as_str(), "fn main() {}");

    // 3. Tool result with is_error: true
    let error_record = json!({
        "type": "tool_result",
        "uuid": "err-01",
        "call_id": "call-fail-01",
        "content": "file not found",
        "is_error": true
    });
    let ev = parse(error_record).expect("tool_result with is_error must parse");
    assert_eq!(ev.kind, EventKind::ToolResult);
    let tool = ev.tool.expect("tool event must be extracted");
    assert_eq!(tool.status, ToolStatus::Failed);
    assert_eq!(tool.result.unwrap().as_str(), "file not found");

    // 4. Nested blocks inside tool_result content
    let nested_res_record = json!({
        "type": "user",
        "uuid": "usr-02",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "call-nested-01",
                    "content": [
                        { "type": "text", "text": "chunk 1" },
                        { "type": "text", "text": "chunk 2" }
                    ],
                    "is_error": false
                }
            ]
        }
    });
    let ev = parse(nested_res_record).expect("nested tool_result blocks must parse");
    let tool = ev.tool.expect("tool event must be extracted");
    assert_eq!(tool.result.unwrap().as_str(), "chunk 1\nchunk 2");

    // 5. Multiple text blocks concatenated
    let multi_text_record = json!({
        "type": "user",
        "uuid": "usr-03",
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": "first paragraph" },
                { "type": "text", "text": "second paragraph" }
            ]
        }
    });
    let ev = parse(multi_text_record).expect("multi-text blocks must parse");
    assert_eq!(ev.text.as_str(), "first paragraph\nsecond paragraph");

    // 6. Thinking and image blocks are dropped while preserving text
    let cot_record = json!({
        "type": "assistant",
        "uuid": "ast-02",
        "message": {
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "internal secret reasoning" },
                { "type": "text", "text": "clean user-facing answer" },
                { "type": "image", "source": { "type": "base64", "data": "binary" } }
            ]
        }
    });
    let ev = parse(cot_record).expect("thinking/image blocks must drop cleanly");
    assert_eq!(ev.text.as_str(), "clean user-facing answer");
    assert!(!ev.text.as_str().contains("reasoning"));

    // 7. Dedicated top-level tool_use record
    let dedicated_tool_use = json!({
        "type": "tool_use",
        "uuid": "tu-01",
        "call_id": "call-ded-01",
        "name": "bash",
        "input": { "cmd": "cargo test" }
    });
    let ev = parse(dedicated_tool_use).expect("dedicated tool_use record must parse");
    assert_eq!(ev.kind, EventKind::ToolInvocation);
    let tool = ev.tool.expect("tool event must be present");
    assert_eq!(tool.call_id.unwrap().as_str(), "call-ded-01");
    assert_eq!(tool.name.as_str(), "bash");
    assert_eq!(tool.arguments.unwrap().as_str(), "{\"cmd\":\"cargo test\"}");
}

#[test]
fn unsupported_essential_shapes_are_rejected_not_silently_accepted() {
    use serde_json::json;

    let corrupt_cases = [
        // Empty object
        json!({}),
        // Message with null message
        json!({ "type": "user", "message": null }),
        // Message with integer content
        json!({ "type": "user", "message": { "content": 42 } }),
        // Message with boolean content
        json!({ "type": "user", "message": { "content": true } }),
        // Message with empty content array
        json!({ "type": "user", "message": { "content": [] } }),
        // Message with content block missing type
        json!({ "type": "user", "message": { "content": [{ "text": "missing type" }] } }),
        // Message with unsupported block type
        json!({ "type": "user", "message": { "content": [{ "type": "future_block_kind", "val": 1 }] } }),
        // Text block with non-string text
        json!({ "type": "user", "message": { "content": [{ "type": "text", "text": 123 }] } }),
        // Tool use missing tool name
        json!({ "type": "tool_use", "call_id": "call-1" }),
        // Tool use missing call_id
        json!({ "type": "tool_use", "name": "bash" }),
        // Tool result missing call_id / tool_use_id
        json!({ "type": "tool_result", "content": "output" }),
        // User record with empty text
        json!({ "type": "user", "text": "" }),
        // User record with no text, message, content, or tool
        json!({ "type": "user", "uuid": "u-empty" }),
        // Assistant record with empty content object
        json!({ "type": "assistant", "message": {} }),
    ];

    for case in corrupt_cases {
        assert_eq!(
            parse(case.clone()),
            Err(SkipKind::Corrupt),
            "case must be rejected as corrupt: {case}"
        );
    }
}

#[test]
fn descriptor_bound_snapshot_detects_path_traversal() {
    use skillranker::context::jsonl::{CursorKind, JsonlError, snapshot_jsonl};
    use skillranker::runtime::ProcessInvocation;
    use std::path::Path;

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let traversal_path = Path::new("some/dir/../../etc/passwd");
    let err =
        snapshot_jsonl(&invocation, &cx, traversal_path, None, CursorKind::Ranking).unwrap_err();
    assert_eq!(err, JsonlError::UnsafePath);
    assert!(invocation.shutdown());
}

#[test]
fn claude_records_the_user_did_not_submit_are_context_not_requests() {
    use skillranker::context::{EventKind, Role};
    let user = |extra: serde_json::Value, text: &str| {
        let mut record = json!({"type": "user", "uuid": "u-1",
                                "message": {"role": "user", "content": text}});
        record
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        parse(record).unwrap()
    };
    for (extra, text) in [
        (
            json!({"isMeta": true}),
            "Base directory for this skill: ...",
        ),
        (
            json!({"isCompactSummary": true}),
            "This session is being continued",
        ),
        (
            json!({"promptSource": "system"}),
            "A background task finished.",
        ),
        (
            json!({"origin": {"kind": "task-notification"}}),
            "Task done.",
        ),
        (
            json!({"origin": {"kind": "auto-continuation"}}),
            "Continue.",
        ),
        (json!({}), "[Request interrupted by user]"),
        (
            json!({}),
            "<local-command-stdout>Login successful</local-command-stdout>",
        ),
    ] {
        let event = user(extra.clone(), text);
        assert_eq!(event.role, Role::System, "{extra} {text}");
        assert_eq!(event.kind, EventKind::Message);
        assert_eq!(event.text.as_str(), text, "the context is kept");
    }
    // Submitted prompts, and older records without marks, stay user messages.
    for extra in [
        json!({"promptSource": "typed", "origin": {"kind": "human"}}),
        json!({"promptSource": "queued"}),
        json!({"isMeta": false}),
        json!({}),
    ] {
        assert_eq!(
            user(extra.clone(), "Fix the test.").role,
            Role::User,
            "{extra}"
        );
    }
    // Explicit human provenance outranks the text heuristics: a typed prompt
    // may quote an interrupt marker or command output.
    for text in [
        "[Request interrupted by user] happens when I press Esc; why?",
        "<local-command-stdout>Login successful</local-command-stdout> means what?",
    ] {
        let typed = json!({"promptSource": "typed", "origin": {"kind": "human"}});
        assert_eq!(user(typed, text).role, Role::User, "{text}");
    }
}
