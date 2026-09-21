//! One local history selection for directive extraction and provider rendering.
//!
//! Parent-linked inputs use the qualified branch resolver, never vector order
//! or timestamps. Flat normalized inputs retain their supplied order only within
//! the envelope's exact agent/branch namespace. No path or provider effects.

use crate::context::branch::{BranchResolution, BranchResolutionTarget, resolve_active_branch};
use crate::context::{EventKind, NormalizedContext, NormalizedEvent, Role};
use std::collections::BTreeMap;

const AMBIGUOUS: &str = "active request history cannot be resolved unambiguously";
const CONFLICT: &str = "active request history contains conflicting event definitions";
const WRONG_REQUEST: &str = "current request event does not match its declared scope or role";

/// Selected earlier events and whether their parent chain is incomplete.
/// The authoritative current prompt is excluded by identity, never equal text.
/// Selection precedes directive parsing, redaction, and message windowing.
pub(crate) fn select(
    context: &NormalizedContext,
) -> Result<(Vec<NormalizedEvent>, bool), &'static str> {
    let in_scope = |event: &&NormalizedEvent| {
        event.agent_id == context.agent_id && event.branch_id == context.branch_id
    };
    let current = context.current_request.event_id.as_ref();
    let has_current = current.is_some_and(|id| {
        context
            .events
            .iter()
            .filter(in_scope)
            .any(|event| event.event_id.as_ref() == Some(id))
    });
    if !has_current
        && current.is_some_and(|id| {
            context.events.iter().any(|event| event.event_id.as_ref() == Some(id))
        })
    {
        // A missing scoped event is not permission to select a foreign event
        // with that ID or to reinterpret some other leaf as the request.
        return Err(WRONG_REQUEST);
    }
    if has_current
        && context.events.iter().filter(in_scope).any(|event| {
            event.event_id.as_ref() == current
                && (event.role != Role::User || event.kind != EventKind::Message)
        })
    {
        return Err(WRONG_REQUEST);
    }

    // The absence of parent links is the existing flat normalized-input
    // contract, not evidence for merging branches. Explicit labels still scope
    // that sequence. Graph-bearing histories must never use this fallback.
    if !context
        .events
        .iter()
        .filter(in_scope)
        .any(|event| event.parent_id.is_some())
    {
        let mut selected = Vec::new();
        let mut seen = BTreeMap::new();
        let mut past_current = false;
        for event in context.events.iter().filter(in_scope) {
            if let Some(id) = &event.event_id {
                if let Some(previous) = seen.insert(id, event) {
                    if previous != event {
                        return Err(CONFLICT);
                    }
                    continue;
                }
                if Some(id) == current {
                    past_current = true;
                }
            }
            if !past_current {
                selected.push(event.clone());
            }
        }
        return Ok((selected, false));
    }

    // None is the unassigned agent namespace, not a wildcard. The branch
    // resolver can retain real same-agent ancestors but cannot import agents.
    let events: Vec<_> = context
        .events
        .iter()
        .filter(|event| event.agent_id == context.agent_id)
        .cloned()
        .collect();
    let target = BranchResolutionTarget {
        target_event_id: if has_current { current.cloned() } else { None },
        target_branch_id: context.branch_id.clone(),
        target_agent_id: context.agent_id.clone(),
    };
    let BranchResolution::Resolved(mut branch) = resolve_active_branch(&events, &target) else {
        return Err(AMBIGUOUS);
    };
    // A graph entry without identity cannot be placed on the selected chain.
    // Count its omission as incomplete, rather than interpreting its prose.
    let incomplete = branch.ancestor_chain_truncated
        || context
            .events
            .iter()
            .filter(in_scope)
            .any(|event| event.event_id.is_none());
    if has_current {
        branch
            .events
            .retain(|event| event.event_id.as_ref() != current);
    }
    Ok((branch.events, incomplete))
}
