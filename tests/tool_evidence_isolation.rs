//! Exercise the public context library through association, extraction and eligibility.
use skillranker::context::branch::{
    BranchResolutionTarget, SkillUsageKind, evaluate_loaded_skill_eligibility,
    resolve_active_branch,
};
use skillranker::context::tool::{SimpleSkillResolver, SkillMatch};
use skillranker::context::{
    EventKind, LoadState, NormalizedEvent, PrivateText, Role, ToolEvent, ToolStatus,
    associate_tool_events, extract_load_observations, extract_loaded_skill_records,
};
use skillranker::identity::{
    AgentId, BranchId, ContentHash, ContextEpoch, EventId, HarnessId, SessionIdentity, SkillId,
    SourceProvenance, ToolCallId, TurnId,
};

fn event(id: &str, parent: Option<&str>, kind: EventKind) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new(id).unwrap()),
        parent_id: parent.map(|id| EventId::new(id).unwrap()),
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: None,
        role: Role::Tool,
        kind,
        timestamp_unix_ms: None,
        text: PrivateText::new(""),
        tool: matches!(kind, EventKind::ToolInvocation | EventKind::ToolResult).then(|| {
            ToolEvent {
                call_id: Some(ToolCallId::new("reused-call").unwrap()),
                name: PrivateText::new("load-reference"),
                status: if kind == EventKind::ToolResult {
                    ToolStatus::Succeeded
                } else {
                    ToolStatus::Attempted
                },
                arguments: None,
                result: (kind == EventKind::ToolResult)
                    .then(|| PrivateText::new("reference loaded")),
            }
        }),
    }
}

fn resolver() -> SimpleSkillResolver {
    let mut resolver = SimpleSkillResolver::new();
    let hash = ContentHash::from_bytes(b"reference fixture");
    resolver.register_tool(
        "load-reference",
        SkillMatch {
            skill_id: SkillId::new("reference").unwrap(),
            usage_kind: SkillUsageKind::Reference,
            source_content: Some(hash.clone()),
            rendered_content: Some(hash),
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );
    resolver
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        source: SourceProvenance::Normalized {
            producer: None,
            harness: HarnessId::new("fixture").unwrap(),
            schema_version: 1,
        },
        workspace: None,
        session: None,
        agent: None,
        branch: None,
        epoch: Some(ContextEpoch::new("epoch-0").unwrap()),
    }
}

#[test]
fn colliding_call_ids_and_names_stay_with_their_agent_and_branch() {
    for by_name in [false, true] {
        for by_agent in [false, true] {
            let mut events = vec![
                event("invoke-a", None, EventKind::ToolInvocation),
                event("invoke-b", None, EventKind::ToolInvocation),
                event("result-a", Some("invoke-a"), EventKind::ToolResult),
                event("result-b", Some("invoke-b"), EventKind::ToolResult),
            ];
            for (index, ev) in events.iter_mut().enumerate() {
                let scope = if index % 2 == 0 { "a" } else { "b" };
                if by_agent {
                    ev.agent_id = Some(AgentId::new(scope).unwrap());
                } else {
                    ev.branch_id = Some(BranchId::new(scope).unwrap());
                }
                if by_name {
                    ev.tool.as_mut().unwrap().call_id = None;
                }
            }
            events[3].tool.as_mut().unwrap().status = ToolStatus::Failed;
            let calls = associate_tool_events(&events, 200);
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].status, ToolStatus::Succeeded);
            assert_eq!(
                calls[0].result_event_id.as_ref().unwrap().as_str(),
                "result-a"
            );
            assert_eq!(calls[1].status, ToolStatus::Failed);
            assert_eq!(
                calls[1].result_event_id.as_ref().unwrap().as_str(),
                "result-b"
            );
        }
    }
}

#[test]
fn name_fallback_does_not_cross_user_turns() {
    let mut call = event("call", None, EventKind::ToolInvocation);
    call.tool.as_mut().unwrap().call_id = None;
    let mut result = event("result", None, EventKind::ToolResult);
    result.tool.as_mut().unwrap().call_id = None;
    result.turn_id = Some(TurnId::new("turn-2").unwrap());
    let calls = associate_tool_events(&[call, result], 200);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].status, ToolStatus::Attempted);
    assert!(calls[0].result_event_id.is_none());
}

#[test]
fn sibling_result_without_branch_labels_cannot_certify_an_active_load() {
    let root = event("root", None, EventKind::Message);
    let call = event("call", Some("root"), EventKind::ToolInvocation);
    let sibling = event("sibling-result", Some("root"), EventKind::ToolResult);
    let leaf = event("leaf", Some("call"), EventKind::Message);
    let mut events = vec![root, call, sibling, leaf];
    for honest_result in [false, true] {
        if honest_result {
            events[3].parent_id = Some(EventId::new("honest-result").unwrap());
            events.insert(
                3,
                event("honest-result", Some("call"), EventKind::ToolResult),
            );
        }
        let resolved = resolve_active_branch(
            &events,
            &BranchResolutionTarget {
                target_event_id: Some(EventId::new("leaf").unwrap()),
                ..Default::default()
            },
        );
        let branch = resolved.active_branch().unwrap();
        let observations =
            extract_load_observations(&events, &identity(), &resolver(), Some(branch));
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].state,
            if honest_result {
                LoadState::ObservedLoaded
            } else {
                LoadState::Attempted
            }
        );
        let records =
            extract_loaded_skill_records(&events, &resolver(), Some(branch), &branch.current_epoch);
        assert_eq!(records.len(), usize::from(honest_result));
    }
}

#[test]
fn reload_after_compaction_preserves_both_observations_and_current_eligibility() {
    let mut events = vec![
        event("first-call", None, EventKind::ToolInvocation),
        event("first-result", Some("first-call"), EventKind::ToolResult),
        event("compact", Some("first-result"), EventKind::Compaction),
        event("second-call", Some("compact"), EventKind::ToolInvocation),
        event("second-result", Some("second-call"), EventKind::ToolResult),
    ];
    // Real redelivery stays idempotent; a distinct second load does not.
    events.push(events[0].clone());
    let resolved = resolve_active_branch(
        &events[..5],
        &BranchResolutionTarget {
            target_event_id: Some(EventId::new("second-result").unwrap()),
            ..Default::default()
        },
    );
    let branch = resolved.active_branch().unwrap();
    assert_eq!(associate_tool_events(&events, 200).len(), 2);
    let observations = extract_load_observations(&events, &identity(), &resolver(), Some(branch));
    assert_eq!(
        observations.len(),
        2,
        "distinct successful loads are distinct observations"
    );
    let records =
        extract_loaded_skill_records(&events, &resolver(), Some(branch), &branch.current_epoch);
    assert_eq!(
        records.len(),
        2,
        "old evidence must not erase a post-compaction reload"
    );
    let hash = ContentHash::from_bytes(b"reference fixture");
    let verdict = evaluate_loaded_skill_eligibility(
        &SkillId::new("reference").unwrap(),
        SkillUsageKind::Reference,
        Some(&hash),
        Some(&hash),
        Some(branch),
        &records,
    );
    assert!(verdict.is_suppressed());
    assert!(
        evaluate_loaded_skill_eligibility(
            &SkillId::new("reference").unwrap(),
            SkillUsageKind::Reference,
            Some(&hash),
            Some(&hash),
            Some(branch),
            &records[..1],
        )
        .is_eligible(),
        "the pre-compaction load alone proves no current presence"
    );

    // A full branch proves lineage, not permission to import events outside
    // the supplied observation delta a second time.
    let delta = &events[3..5];
    let observations = extract_load_observations(delta, &identity(), &resolver(), Some(branch));
    assert_eq!(observations.len(), 1);
    assert_eq!(
        observations[0].event_id.as_ref().unwrap().as_str(),
        "second-call"
    );
    let records =
        extract_loaded_skill_records(delta, &resolver(), Some(branch), &branch.current_epoch);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].epoch, branch.current_epoch);
}
