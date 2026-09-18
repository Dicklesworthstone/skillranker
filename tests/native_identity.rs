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
