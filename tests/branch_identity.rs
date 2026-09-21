//! Parent links and event IDs resolve inside an agent/branch namespace, not
//! by input order. These exercise the production resolver and load extractor.

use skillranker::context::branch::{
    ActiveBranch, BranchResolution, BranchResolutionTarget, UnresolvedBranchReason,
    resolve_active_branch,
};
use skillranker::context::{
    EventKind, NormalizedEvent, PrivateText, Role, SimpleSkillResolver, SkillMatch, SkillUsageKind,
    ToolEvent, ToolStatus, extract_loaded_skill_records,
};
use skillranker::identity::{AgentId, BranchId, EventId, SkillId, ToolCallId};

fn event(
    id: &str,
    parent: Option<&str>,
    agent: Option<&str>,
    branch: Option<&str>,
) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new(id).unwrap()),
        parent_id: parent.map(|value| EventId::new(value).unwrap()),
        turn_id: None,
        agent_id: agent.map(|value| AgentId::new(value).unwrap()),
        branch_id: branch.map(|value| BranchId::new(value).unwrap()),
        role: Role::Assistant,
        kind: EventKind::Message,
        timestamp_unix_ms: None,
        text: PrivateText::new(""),
        tool: None,
    }
}

fn target(id: Option<&str>, agent: Option<&str>, branch: Option<&str>) -> BranchResolutionTarget {
    BranchResolutionTarget {
        target_event_id: id.map(|value| EventId::new(value).unwrap()),
        target_agent_id: agent.map(|value| AgentId::new(value).unwrap()),
        target_branch_id: branch.map(|value| BranchId::new(value).unwrap()),
    }
}

fn resolved(events: &[NormalizedEvent], target: &BranchResolutionTarget) -> ActiveBranch {
    match resolve_active_branch(events, target) {
        BranchResolution::Resolved(branch) => branch,
        BranchResolution::Unresolved(reason) => panic!("expected a resolved lineage: {reason:?}"),
    }
}

#[test]
fn explicit_agent_selects_the_correct_reused_event_and_ancestors_in_either_order() {
    let active = [
        event("root", None, Some("active"), None),
        event("leaf", Some("root"), Some("active"), None),
    ];
    let mut other = [
        event("root", None, Some("other"), None),
        event("leaf", Some("root"), Some("other"), None),
    ];
    other[0].kind = EventKind::Compaction;
    for events in [
        [&active[..], &other[..]].concat(),
        [&other[..], &active[..]].concat(),
    ] {
        let branch = resolved(&events, &target(Some("leaf"), Some("active"), None));
        assert_eq!(branch.events, active);
        assert_eq!(branch.compaction_count, 0);
        assert_eq!(branch.current_epoch.as_str(), "epoch-0");
    }
}

#[test]
fn an_explicit_branch_disambiguates_same_agent_event_ids() {
    let active = [
        event("root", None, Some("agent"), Some("active")),
        event("leaf", Some("root"), Some("agent"), Some("active")),
    ];
    let other = [
        event("root", None, Some("agent"), Some("other")),
        event("leaf", Some("root"), Some("agent"), Some("other")),
    ];
    for events in [
        [&active[..], &other[..]].concat(),
        [&other[..], &active[..]].concat(),
    ] {
        let branch = resolved(&events, &target(Some("leaf"), Some("agent"), Some("active")));
        assert_eq!(branch.events, active);
        assert!(!branch.ancestor_chain_truncated);
    }
}

#[test]
fn an_unqualified_reused_event_id_is_ambiguous_not_last_writer_wins() {
    for (left, right) in [
        (event("same", None, Some("a"), None), event("same", None, Some("b"), None)),
        (event("same", None, None, Some("a")), event("same", None, None, Some("b"))),
        (event("same", None, None, None), event("same", None, Some("a"), None)),
    ] {
        for events in [[left.clone(), right.clone()], [right.clone(), left.clone()]] {
            assert!(matches!(
                resolve_active_branch(&events, &target(Some("same"), None, None)),
                BranchResolution::Unresolved(UnresolvedBranchReason::AmbiguousEventIdentity { .. })
            ));
        }
    }
}

#[test]
fn a_present_bare_id_does_not_override_an_explicit_scope_mismatch() {
    let events = [event("leaf", None, Some("other"), Some("other"))];
    for request in [
        target(Some("leaf"), Some("active"), None),
        target(Some("leaf"), None, Some("active")),
    ] {
        assert!(matches!(
            resolve_active_branch(&events, &request),
            BranchResolution::Unresolved(UnresolvedBranchReason::TargetEventNotFound { .. })
        ));
    }
}

#[test]
fn a_foreign_singleton_is_not_a_fallback_for_an_empty_selected_scope() {
    for identified in [true, false] {
        let mut foreign = event("leaf", None, Some("other"), Some("other"));
        if !identified {
            foreign.event_id = None;
        }
        for request in [target(None, Some("active"), None), target(None, None, Some("active"))] {
            assert_eq!(
                resolve_active_branch(std::slice::from_ref(&foreign), &request),
                BranchResolution::Unresolved(UnresolvedBranchReason::NoMatchingEvents)
            );
        }
        // Honest counterpart: the matching singleton is still usable.
        let branch = resolved(std::slice::from_ref(&foreign), &target(None, Some("other"), None));
        assert_eq!(branch.events, [foreign]);
    }
}

#[test]
fn a_foreign_parent_is_a_missing_ancestor_not_an_active_compaction() {
    for agent in [None, Some("active")] {
        let leaf = event("leaf", Some("root"), agent, None);
        let mut parent = event("root", None, Some("other"), None);
        parent.kind = EventKind::Compaction;
        let branch = resolved(&[parent, leaf.clone()], &target(Some("leaf"), agent, None));
        assert_eq!(branch.events, [leaf]);
        assert!(branch.ancestor_chain_truncated);
        assert_eq!(branch.compaction_count, 0);
    }
}

#[test]
fn another_agents_parent_reference_cannot_hide_the_selected_leaf() {
    let active = event("leaf", None, Some("active"), None);
    let foreign = event("foreign", Some("leaf"), Some("other"), None);
    let branch = resolved(&[active.clone(), foreign], &target(None, Some("active"), None));
    assert_eq!(branch.events, [active]);
}

#[test]
fn exact_redelivery_deduplicates_but_conflicting_definitions_are_rejected() {
    let root = event("root", None, Some("active"), None);
    let leaf = event("leaf", Some("root"), Some("active"), None);
    let request = target(Some("leaf"), Some("active"), None);
    let branch = resolved(&[root.clone(), leaf.clone(), root.clone(), leaf.clone()], &request);
    assert_eq!(branch.events.len(), 2);
    let mut conflict = root.clone();
    conflict.kind = EventKind::Compaction;
    for events in [
        [root.clone(), conflict.clone(), leaf.clone()],
        [conflict.clone(), root.clone(), leaf.clone()],
    ] {
        assert!(matches!(
            resolve_active_branch(&events, &request),
            BranchResolution::Unresolved(UnresolvedBranchReason::ConflictingEventDefinitions { .. })
        ));
    }
    // An unrelated agent's conflict does not erase an explicitly selected lineage.
    let mut other = conflict.clone();
    other.agent_id = Some(AgentId::new("other").unwrap());
    conflict.agent_id = other.agent_id.clone();
    conflict.text = PrivateText::new("different definition");
    assert_eq!(resolved(&[root, leaf, other, conflict], &request).events.len(), 2);
}

#[test]
fn a_parent_with_multiple_out_of_branch_definitions_is_not_guessed() {
    let events = [
        event("root", None, Some("agent"), Some("one")),
        event("root", None, Some("agent"), Some("two")),
        event("leaf", Some("root"), Some("agent"), Some("three")),
    ];
    assert!(matches!(
        resolve_active_branch(&events, &target(Some("leaf"), Some("agent"), None)),
        BranchResolution::Unresolved(UnresolvedBranchReason::AmbiguousEventIdentity { .. })
    ));
}

#[test]
fn unique_shared_fork_ancestors_and_explicit_branch_conflicts_are_preserved() {
    let root = event("root", None, Some("agent"), Some("main"));
    let leaf = event("leaf", Some("root"), Some("agent"), Some("feature"));
    let sibling = event("sibling", Some("root"), Some("agent"), Some("other"));
    let events = [root.clone(), sibling, leaf.clone()];
    let branch = resolved(&events, &target(Some("leaf"), Some("agent"), None));
    assert_eq!(branch.events, [root, leaf]);
    assert!(matches!(
        resolve_active_branch(&events, &target(Some("leaf"), Some("agent"), Some("feature"))),
        BranchResolution::Unresolved(UnresolvedBranchReason::ConflictingBranchIdentities { .. })
    ));
}

#[test]
fn cycles_are_rejected_with_and_without_an_explicit_leaf() {
    for events in [
        vec![event("one", Some("one"), None, None)],
        vec![event("one", Some("two"), None, None), event("two", Some("one"), None, None)],
    ] {
        for request in [target(None, None, None), target(Some("one"), None, None)] {
            assert!(matches!(
                resolve_active_branch(&events, &request),
                BranchResolution::Unresolved(UnresolvedBranchReason::CycleDetected { .. })
            ));
        }
    }
}

#[test]
fn timestamp_order_does_not_change_compaction_and_task_boundary_lineage() {
    let mut root = event("root", None, Some("agent"), None);
    root.kind = EventKind::TaskBoundary;
    root.timestamp_unix_ms = Some(300);
    let mut compact = event("compact", Some("root"), Some("agent"), None);
    compact.kind = EventKind::Compaction;
    compact.timestamp_unix_ms = Some(200);
    let mut leaf = event("leaf", Some("compact"), Some("agent"), None);
    leaf.timestamp_unix_ms = Some(100);
    let branch = resolved(
        &[leaf.clone(), compact.clone(), root.clone()],
        &target(Some("leaf"), Some("agent"), None),
    );
    assert_eq!(branch.events, [root, compact, leaf]);
    assert_eq!(branch.current_epoch.as_str(), "epoch-1");
    assert_eq!(branch.compaction_count, 1);
    assert_eq!(branch.task_boundary_count, 1);
}

#[test]
fn selected_lineage_and_production_load_extraction_agree_on_agent_identity() {
    let mut active = [
        event("invoke", None, Some("active"), None),
        event("reply", Some("invoke"), Some("active"), None),
    ];
    active[0].kind = EventKind::ToolInvocation;
    active[0].tool = Some(ToolEvent {
        call_id: Some(ToolCallId::new("shared-call").unwrap()),
        name: PrivateText::new("Skill"),
        arguments: Some(PrivateText::new(r#"{"skill":"alpha"}"#)),
        status: ToolStatus::Attempted,
        result: None,
    });
    active[1].kind = EventKind::ToolResult;
    active[1].role = Role::Tool;
    active[1].tool = Some(ToolEvent {
        call_id: Some(ToolCallId::new("shared-call").unwrap()),
        name: PrivateText::new("Skill"),
        arguments: None,
        status: ToolStatus::Succeeded,
        result: Some(PrivateText::new("Loaded")),
    });
    let mut other = active.clone();
    for event in &mut other {
        event.agent_id = Some(AgentId::new("other").unwrap());
    }
    other[0].tool.as_mut().unwrap().arguments = Some(PrivateText::new(r#"{"skill":"foreign"}"#));
    let mut resolver = SimpleSkillResolver::new();
    for name in ["alpha", "foreign"] {
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
    for events in [
        [&active[..], &other[..]].concat(),
        [&other[..], &active[..]].concat(),
    ] {
        let branch = resolved(&events, &target(Some("reply"), Some("active"), None));
        let loaded = extract_loaded_skill_records(
            &events,
            &resolver,
            Some(&branch),
            &branch.current_epoch,
        );
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].skill_id.as_str(), "alpha");
    }
}

#[test]
fn unique_unlabeled_ancestors_keep_their_own_scope_and_epoch() {
    let mut ancestor = event("compact", None, Some("agent"), None);
    ancestor.kind = EventKind::Compaction;
    let leaf = event("leaf", Some("compact"), Some("agent"), Some("main"));
    let branch = resolved(
        &[ancestor.clone(), leaf.clone()],
        &target(Some("leaf"), Some("agent"), Some("main")),
    );
    assert_eq!(branch.events, [ancestor, leaf]);
    assert!(branch.events[0].branch_id.is_none());
    assert_eq!(branch.current_epoch.as_str(), "epoch-1");
    assert!(!branch.ancestor_chain_truncated);
}
