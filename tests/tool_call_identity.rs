//! Load confirmation must belong to one unambiguous invocation. These use the
//! production event join and loaded-record extractor, not a duplicate model.

use serde_json::json;
use skillranker::context::tool::{
    SimpleSkillResolver, SkillMatch, associate_tool_events, extract_loaded_skill_records,
};
use skillranker::context::{
    EventKind, LoadedSkillRecord, NormalizedEvent, PrivateText, Role, SkillUsageKind, ToolEvent,
    ToolStatus,
};
use skillranker::identity::{
    AgentId, BranchId, ContextEpoch, EventId, SkillId, ToolCallId, TurnId,
};

fn invocation(event: &str, call: Option<&str>, skill: &str) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new(event).unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: None,
        role: Role::Assistant,
        kind: EventKind::ToolInvocation,
        timestamp_unix_ms: None,
        text: PrivateText::new(""),
        tool: Some(ToolEvent {
            call_id: call.map(|id| ToolCallId::new(id).unwrap()),
            name: PrivateText::new("Skill"),
            status: ToolStatus::Attempted,
            arguments: Some(PrivateText::new(json!({"skill": skill}).to_string())),
            result: None,
        }),
    }
}

fn result(event: &str, call: Option<&str>) -> NormalizedEvent {
    let mut event = invocation(event, call, "alpha");
    event.role = Role::Tool;
    event.kind = EventKind::ToolResult;
    let tool = event.tool.as_mut().unwrap();
    tool.status = ToolStatus::Succeeded;
    tool.arguments = None;
    tool.result = Some(PrivateText::new("Loaded successfully"));
    event
}

fn loaded(events: &[NormalizedEvent]) -> Vec<LoadedSkillRecord> {
    let mut resolver = SimpleSkillResolver::new();
    for name in ["alpha", "beta", "gamma"] {
        resolver.register_tool(
            name,
            SkillMatch {
                skill_id: SkillId::new(name).unwrap(),
                usage_kind: SkillUsageKind::Workflow,
                source_content: None,
                rendered_content: None,
                has_dynamic_arguments: false,
                turn_scoped: false,
            },
        );
    }
    extract_loaded_skill_records(
        events,
        &resolver,
        None,
        &ContextEpoch::new("epoch-1").unwrap(),
    )
}

#[test]
fn overlapping_same_id_invocations_cannot_credit_either_skill() {
    for names in [["alpha", "beta"], ["beta", "alpha"]] {
        let mut reply = result("result-1", Some("duplicate"));
        // An ambiguous reply carrying its own arguments is not an escape hatch.
        reply.tool.as_mut().unwrap().arguments = Some(PrivateText::new(r#"{"skill":"gamma"}"#));
        let events = [
            invocation("call-1", Some("duplicate"), names[0]),
            invocation("call-2", Some("duplicate"), names[1]),
            reply,
            result("result-2", Some("duplicate")),
        ];
        assert!(loaded(&events).is_empty());
        let joined = associate_tool_events(&events, 200);
        assert!(joined.iter().all(|call| call.status == ToolStatus::Unknown));
        assert_eq!(
            joined.len(),
            4,
            "ambiguity must not drop the local evidence"
        );
    }
}

#[test]
fn a_reused_id_after_completion_invalidates_the_conflicting_evidence() {
    let events = [
        invocation("call-1", Some("reused"), "alpha"),
        result("result-1", Some("reused")),
        invocation("call-2", Some("reused"), "beta"),
        result("result-2", Some("reused")),
    ];
    assert_eq!(loaded(&events[..2]).len(), 1);
    assert!(loaded(&events).is_empty());
}

#[test]
fn duplicate_results_do_not_keep_an_earlier_success_or_create_an_orphan_load() {
    let mut second = result("result-2", Some("call"));
    second.tool.as_mut().unwrap().arguments = Some(PrivateText::new(r#"{"skill":"beta"}"#));
    for status in [ToolStatus::Succeeded, ToolStatus::Failed] {
        second.tool.as_mut().unwrap().status = status;
        let events = [
            invocation("call-1", Some("call"), "alpha"),
            result("result-1", Some("call")),
            second.clone(),
        ];
        assert!(loaded(&events).is_empty());
    }
}

#[test]
fn unique_interleaved_call_ids_still_resolve_their_own_arguments() {
    let events = [
        invocation("call-1", Some("a"), "alpha"),
        invocation("call-2", Some("b"), "beta"),
        result("result-2", Some("b")),
        result("result-1", Some("a")),
    ];
    let loaded = loaded(&events);
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].skill_id.as_str(), "alpha");
    assert_eq!(loaded[0].event_id.as_ref().unwrap().as_str(), "call-1");
    assert_eq!(loaded[1].skill_id.as_str(), "beta");
    assert_eq!(loaded[1].event_id.as_ref().unwrap().as_str(), "call-2");
}

#[test]
fn exact_event_redelivery_is_deduplicated_before_association() {
    let call = invocation("call-1", Some("a"), "alpha");
    let reply = result("result-1", Some("a"));
    let events = [call.clone(), call, reply.clone(), reply];
    assert_eq!(associate_tool_events(&events, 200).len(), 1);
    assert_eq!(loaded(&events).len(), 1);
}

#[test]
fn no_id_matching_is_sequential_not_last_writer_wins() {
    let sequential = [
        invocation("call-1", None, "alpha"),
        result("result-1", None),
        invocation("call-2", None, "beta"),
        result("result-2", None),
    ];
    let records = loaded(&sequential);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].skill_id.as_str(), "alpha");
    assert_eq!(records[1].skill_id.as_str(), "beta");
    let overlapping = [
        sequential[0].clone(),
        sequential[2].clone(),
        sequential[1].clone(),
        sequential[3].clone(),
    ];
    assert!(loaded(&overlapping).is_empty());
}

#[test]
fn identical_call_ids_remain_independent_across_agent_and_branch_scopes() {
    for by_agent in [false, true] {
        let mut first = invocation("call-1", Some("shared"), "alpha");
        let mut second = invocation("call-2", Some("shared"), "beta");
        let mut first_result = result("result-1", Some("shared"));
        let mut second_result = result("result-2", Some("shared"));
        if by_agent {
            first.agent_id = Some(AgentId::new("agent-a").unwrap());
            second.agent_id = Some(AgentId::new("agent-b").unwrap());
        } else {
            first.branch_id = Some(BranchId::new("branch-a").unwrap());
            second.branch_id = Some(BranchId::new("branch-b").unwrap());
        }
        first_result.agent_id = first.agent_id.clone();
        first_result.branch_id = first.branch_id.clone();
        second_result.agent_id = second.agent_id.clone();
        second_result.branch_id = second.branch_id.clone();
        assert_eq!(
            loaded(&[first, second, second_result, first_result]).len(),
            2
        );
    }
}

#[test]
fn ambiguity_does_not_poison_an_unrelated_identified_call() {
    let events = [
        invocation("bad-1", Some("duplicate"), "alpha"),
        invocation("good", Some("unique"), "gamma"),
        invocation("bad-2", Some("duplicate"), "beta"),
        result("bad-result", Some("duplicate")),
        result("good-result", Some("unique")),
    ];
    let records = loaded(&events);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].skill_id.as_str(), "gamma");
}

#[test]
fn inline_result_errors_receive_the_same_checks_as_separate_results() {
    let mut event = invocation("inline", Some("call"), "alpha");
    let tool = event.tool.as_mut().unwrap();
    tool.status = ToolStatus::Succeeded;
    tool.result = Some(PrivateText::new("error: skill failed to load"));
    assert!(loaded(std::slice::from_ref(&event)).is_empty());
    let joined = associate_tool_events(std::slice::from_ref(&event), 200);
    assert!(!joined[0].error_lines.is_empty());
    assert!(joined[0].result_summary.is_some());
    event.tool.as_mut().unwrap().result = Some(PrivateText::new("Loaded successfully"));
    assert_eq!(loaded(&[event]).len(), 1);
}

#[test]
fn inline_no_id_calls_are_complete_pairs_not_overlapping_attempts() {
    let mut first = invocation("inline-1", None, "alpha");
    let mut second = invocation("inline-2", None, "beta");
    for event in [&mut first, &mut second] {
        let tool = event.tool.as_mut().unwrap();
        tool.status = ToolStatus::Succeeded;
        tool.result = Some(PrivateText::new("Loaded successfully"));
    }
    let records = loaded(&[first, second]);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].skill_id.as_str(), "alpha");
    assert_eq!(records[1].skill_id.as_str(), "beta");
}

#[test]
fn no_id_ambiguity_is_scoped_to_its_turn() {
    let mut next = invocation("next-turn", None, "gamma");
    let mut reply = result("next-result", None);
    next.turn_id = Some(TurnId::new("turn-2").unwrap());
    reply.turn_id = next.turn_id.clone();
    let records = loaded(&[
        invocation("bad-1", None, "alpha"),
        invocation("bad-2", None, "beta"),
        result("bad-result", None),
        next,
        reply,
    ]);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].skill_id.as_str(), "gamma");
}
