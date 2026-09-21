//! The active lineage selects qualified event identities, not bare event IDs.
//! Both observation attribution and loaded-reference extraction use this path.

use serde_json::json;
use skillranker::context::tool::{
    SimpleSkillResolver, SkillMatch, extract_load_observations, extract_loaded_skill_records,
};
use skillranker::context::{
    ActiveBranch, EventKind, LoadObservation, LoadState, LoadedSkillRecord, NormalizedEvent,
    PrivateText, Role, SkillUsageKind, ToolEvent, ToolStatus,
};
use skillranker::identity::{
    AgentId, BranchId, ContentHash, ContextEpoch, EventId, HarnessId, ProducerId, SessionId,
    SessionIdentity, SourceProvenance, ToolCallId, TurnId, WorkspaceId,
};

fn pair(agent: Option<&str>, branch: Option<&str>, skill: &str) -> Vec<NormalizedEvent> {
    let invocation = NormalizedEvent {
        event_id: Some(EventId::new("invoke").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn").unwrap()),
        agent_id: agent.map(|id| AgentId::new(id).unwrap()),
        branch_id: branch.map(|id| BranchId::new(id).unwrap()),
        role: Role::Assistant,
        kind: EventKind::ToolInvocation,
        timestamp_unix_ms: None,
        text: PrivateText::new(""),
        tool: Some(ToolEvent {
            call_id: Some(ToolCallId::new("call").unwrap()),
            name: PrivateText::new("Skill"),
            status: ToolStatus::Attempted,
            arguments: Some(PrivateText::new(json!({"skill": skill}).to_string())),
            result: None,
        }),
    };
    let mut result = invocation.clone();
    result.event_id = Some(EventId::new("result").unwrap());
    result.parent_id = invocation.event_id.clone();
    result.kind = EventKind::ToolResult;
    result.role = Role::Tool;
    let tool = result.tool.as_mut().unwrap();
    tool.status = ToolStatus::Succeeded;
    tool.arguments = None;
    tool.result = Some(PrivateText::new("Loaded"));
    vec![invocation, result]
}

fn branch(events: &[NormalizedEvent]) -> ActiveBranch {
    ActiveBranch {
        branch_id: events.first().and_then(|event| event.branch_id.clone()),
        leaf_event_id: events.last().and_then(|event| event.event_id.clone()),
        events: events.to_vec(),
        current_epoch: ContextEpoch::new("epoch-0").unwrap(),
        compaction_count: 0,
        task_boundary_count: 0,
        ancestor_chain_truncated: false,
    }
}

fn extract(
    events: &[NormalizedEvent],
    active: &ActiveBranch,
) -> (Vec<LoadObservation>, Vec<LoadedSkillRecord>) {
    let mut resolver = SimpleSkillResolver::new();
    for name in ["alpha", "foreign"] {
        resolver.register_tool(
            name,
            SkillMatch {
                skill_id: skillranker::identity::SkillId::new(name).unwrap(),
                usage_kind: SkillUsageKind::Reference,
                source_content: Some(ContentHash::from_bytes(name.as_bytes())),
                rendered_content: Some(ContentHash::from_bytes(name.as_bytes())),
                has_dynamic_arguments: false,
                turn_scoped: false,
            },
        );
    }
    let identity = SessionIdentity {
        source: SourceProvenance::Normalized {
            producer: Some(ProducerId::new("scope-test").unwrap()),
            harness: HarnessId::new("claude_code").unwrap(),
            schema_version: 1,
        },
        workspace: Some(WorkspaceId::new("workspace").unwrap()),
        session: Some(SessionId::new("session").unwrap()),
        agent: active
            .events
            .first()
            .and_then(|event| event.agent_id.clone()),
        branch: active.branch_id.clone(),
        epoch: Some(active.current_epoch.clone()),
    };
    (
        extract_load_observations(events, &identity, &resolver, Some(active)),
        extract_loaded_skill_records(events, &resolver, Some(active), &active.current_epoch),
    )
}

fn assert_only_active(active_events: &[NormalizedEvent], other: &[NormalizedEvent]) {
    let active = branch(active_events);
    for events in [
        [active_events, other].concat(),
        [other, active_events].concat(),
    ] {
        let (observations, loaded) = extract(&events, &active);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].skill_id.as_str(), "alpha");
        assert_eq!(observations[0].state, LoadState::ObservedLoaded);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].skill_id.as_str(), "alpha");
        assert_eq!(loaded[0].epoch.as_str(), "epoch-0");
        assert_eq!(
            loaded[0].rendered_content,
            Some(ContentHash::from_bytes(b"alpha"))
        );
    }
}

#[test]
fn another_agent_cannot_borrow_active_event_ids() {
    assert_only_active(
        &pair(Some("active"), Some("main"), "alpha"),
        &pair(Some("other"), Some("main"), "foreign"),
    );
}

#[test]
fn agent_isolation_also_holds_when_branch_labels_are_absent() {
    assert_only_active(
        &pair(Some("active"), None, "alpha"),
        &pair(Some("other"), None, "foreign"),
    );
}

#[test]
fn an_unassigned_agent_is_not_a_wildcard() {
    for (active, other) in [(None, Some("other")), (Some("active"), None)] {
        assert_only_active(&pair(active, None, "alpha"), &pair(other, None, "foreign"));
    }
}

#[test]
fn an_unassigned_branch_cannot_enter_a_named_lineage_by_id_alone() {
    assert_only_active(
        &pair(Some("active"), Some("main"), "alpha"),
        &pair(Some("active"), None, "foreign"),
    );
}

#[test]
fn orphan_foreign_results_with_arguments_cannot_create_active_loads() {
    let active_events = pair(Some("active"), None, "alpha");
    let mut reply = pair(Some("other"), None, "foreign").pop().unwrap();
    reply.tool.as_mut().unwrap().arguments = Some(PrivateText::new(r#"{"skill":"foreign"}"#));
    // The real active invocation is still pending. A foreign result carrying
    // the same result ID must not be attributed to the active session.
    let events = [active_events[0].clone(), reply];
    let (observations, loaded) = extract(&events, &branch(&active_events));
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].skill_id.as_str(), "alpha");
    assert_eq!(observations[0].state, LoadState::Attempted);
    assert!(loaded.is_empty());
}

#[test]
fn a_foreign_only_slice_cannot_reconstruct_missing_active_evidence() {
    let active_events = pair(Some("active"), None, "alpha");
    let other = pair(Some("other"), None, "foreign");
    let (observations, loaded) = extract(&other, &branch(&active_events));
    assert!(observations.is_empty());
    assert!(loaded.is_empty());
}

#[test]
fn legitimate_unlabeled_ancestors_and_redelivery_still_work() {
    let events = pair(Some("active"), None, "alpha");
    let mut active = branch(&events);
    // A lineage can retain a genuinely unlabeled ancestor. Selection must use
    // each event's own scope, not stamp the leaf's branch onto every ancestor.
    active.branch_id = Some(BranchId::new("main").unwrap());
    let repeated = [&events[..], &events[..]].concat();
    let (observations, loaded) = extract(&repeated, &active);
    assert_eq!(observations.len(), 1);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].skill_id.as_str(), "alpha");
}

#[test]
fn unidentified_events_do_not_acquire_active_membership() {
    let active_events = pair(Some("active"), None, "alpha");
    let mut other = pair(Some("active"), None, "foreign");
    for event in &mut other {
        event.event_id = None;
        event.tool.as_mut().unwrap().call_id = Some(ToolCallId::new("unidentified").unwrap());
    }
    assert_only_active(&active_events, &other);
}
