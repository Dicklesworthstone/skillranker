//! Scope-aware event identity and parent-link selection. No filesystem effects.
//!
//! Explicit target qualifiers constrain the leaf even when an event ID is
//! supplied. Ancestors stay in that leaf's agent namespace. A parent first
//! resolves in the child's branch; a unique same-agent ancestor in another
//! branch can establish a fork, but an ambiguous bare parent ID cannot.

use super::{
    ActiveBranch, BranchResolution, BranchResolutionTarget, UnresolvedBranchReason, epoch_name,
};
use crate::context::{EventKind, NormalizedEvent};
use crate::identity::{AgentId, BranchId, EventId};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

type EventKey<'a> = (Option<&'a AgentId>, Option<&'a BranchId>, &'a EventId);
type AgentEventKey<'a> = (Option<&'a AgentId>, &'a EventId);

struct Index<'a> {
    events: BTreeMap<EventKey<'a>, &'a NormalizedEvent>,
    by_agent: BTreeMap<AgentEventKey<'a>, Vec<EventKey<'a>>>,
    conflicts: BTreeSet<EventKey<'a>>,
}

fn key(event: &NormalizedEvent) -> Option<EventKey<'_>> {
    Some((
        event.agent_id.as_ref(),
        event.branch_id.as_ref(),
        event.event_id.as_ref()?,
    ))
}

fn matches_target(event: &NormalizedEvent, target: &BranchResolutionTarget) -> bool {
    target
        .target_agent_id
        .as_ref()
        .is_none_or(|agent| event.agent_id.as_ref() == Some(agent))
        && target
            .target_branch_id
            .as_ref()
            .is_none_or(|branch| event.branch_id.as_ref() == Some(branch))
}

impl<'a> Index<'a> {
    fn new(events: &'a [NormalizedEvent]) -> Self {
        let mut index = Self {
            events: BTreeMap::new(),
            by_agent: BTreeMap::new(),
            conflicts: BTreeSet::new(),
        };
        for event in events {
            let Some(key) = key(event) else {
                continue;
            };
            match index.events.entry(key) {
                Entry::Vacant(slot) => {
                    slot.insert(event);
                    index.by_agent.entry((key.0, key.2)).or_default().push(key);
                }
                Entry::Occupied(slot) if *slot.get() != event => {
                    index.conflicts.insert(key);
                }
                Entry::Occupied(_) => {} // Identical redelivery, not another definition.
            }
        }
        index
    }

    fn parent(
        &self,
        event: &'a NormalizedEvent,
    ) -> Result<Option<&'a NormalizedEvent>, UnresolvedBranchReason> {
        let Some(parent) = event.parent_id.as_ref() else {
            return Ok(None);
        };
        if let Some(event) = self.events.get(&(
            event.agent_id.as_ref(),
            event.branch_id.as_ref(),
            parent,
        )) {
            return Ok(Some(*event));
        }
        match self.by_agent.get(&(event.agent_id.as_ref(), parent)) {
            None => Ok(None),
            Some(candidates) if candidates.len() == 1 => {
                Ok(self.events.get(&candidates[0]).copied())
            }
            Some(_) => Err(UnresolvedBranchReason::AmbiguousEventIdentity {
                event: parent.clone(),
            }),
        }
    }

    fn select(
        &self,
        events: &'a [NormalizedEvent],
        target: &BranchResolutionTarget,
    ) -> Result<&'a NormalizedEvent, UnresolvedBranchReason> {
        if let Some(id) = target.target_event_id.as_ref() {
            let mut candidates = self.events.values().copied().filter(|event| {
                event.event_id.as_ref() == Some(id) && matches_target(event, target)
            });
            let first = candidates.next().ok_or_else(|| {
                UnresolvedBranchReason::TargetEventNotFound { target: id.clone() }
            })?;
            if candidates.next().is_some() {
                return Err(UnresolvedBranchReason::AmbiguousEventIdentity { event: id.clone() });
            }
            return Ok(first);
        }

        // Only a resolved parent edge removes a candidate leaf. An unrelated
        // agent referencing the same bare ID must not hide this agent's leaf.
        let mut referenced = BTreeSet::new();
        for event in self.events.values() {
            if let Ok(Some(parent)) = self.parent(event)
                && let Some(key) = key(parent)
            {
                referenced.insert(key);
            }
        }
        let mut first_in_scope = None;
        let mut leaves = Vec::new();
        for (&key, &event) in &self.events {
            if !matches_target(event, target) {
                continue;
            }
            if self.conflicts.contains(&key) {
                return Err(UnresolvedBranchReason::ConflictingEventDefinitions {
                    event: key.2.clone(),
                });
            }
            first_in_scope.get_or_insert(event);
            if !referenced.contains(&key) {
                leaves.push(event);
            }
        }
        match leaves.as_slice() {
            [leaf] => Ok(*leaf),
            [] => {
                if events.len() == 1
                    && events[0].event_id.is_none()
                    && matches_target(&events[0], target)
                {
                    return Ok(&events[0]);
                }
                // Preserve cycle diagnostics for an actual cycle, not for a
                // requested scope that happens to contain no candidate leaf.
                if let Some(event) = first_in_scope {
                    self.trace(event, target)?;
                }
                Err(UnresolvedBranchReason::NoMatchingEvents)
            }
            _ => Err(UnresolvedBranchReason::AmbiguousSiblingForks {
                candidate_leaves: leaves
                    .iter()
                    .filter_map(|event| event.event_id.clone())
                    .collect(),
            }),
        }
    }

    fn trace(
        &self,
        leaf: &'a NormalizedEvent,
        target: &BranchResolutionTarget,
    ) -> Result<ActiveBranch, UnresolvedBranchReason> {
        let mut lineage = Vec::new();
        let mut visited = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut current = leaf;
        let mut truncated = false;
        loop {
            if let Some(key) = key(current) {
                if self.conflicts.contains(&key) {
                    return Err(UnresolvedBranchReason::ConflictingEventDefinitions {
                        event: key.2.clone(),
                    });
                }
                if !visited.insert(key) {
                    return Err(UnresolvedBranchReason::CycleDetected {
                        at_event: key.2.clone(),
                    });
                }
                // Public lineage cursors and epochs retain bare event IDs.
                // Do not construct a lineage they cannot represent uniquely.
                if !ids.insert(key.2) {
                    return Err(UnresolvedBranchReason::AmbiguousEventIdentity {
                        event: key.2.clone(),
                    });
                }
            }
            if let (Some(expected), Some(observed)) =
                (&target.target_branch_id, &current.branch_id)
                && expected != observed
            {
                return Err(UnresolvedBranchReason::ConflictingBranchIdentities {
                    expected: expected.clone(),
                    observed: observed.clone(),
                });
            }
            lineage.push(current.clone());
            if current.parent_id.is_none() {
                break;
            }
            match self.parent(current)? {
                Some(parent) => current = parent,
                None => {
                    truncated = true;
                    break;
                }
            }
        }
        lineage.reverse();
        let compaction_count = lineage
            .iter()
            .filter(|e| e.kind == EventKind::Compaction)
            .count();
        let task_boundary_count = lineage
            .iter()
            .filter(|e| e.kind == EventKind::TaskBoundary)
            .count();
        Ok(ActiveBranch {
            branch_id: target
                .target_branch_id
                .clone()
                .or_else(|| lineage.iter().rev().find_map(|event| event.branch_id.clone())),
            leaf_event_id: leaf.event_id.clone(),
            events: lineage,
            current_epoch: epoch_name(compaction_count as u64),
            compaction_count,
            task_boundary_count,
            ancestor_chain_truncated: truncated,
        })
    }
}

pub(super) fn resolve(
    events: &[NormalizedEvent],
    target: &BranchResolutionTarget,
) -> BranchResolution {
    if events.is_empty() {
        return BranchResolution::Unresolved(UnresolvedBranchReason::EmptyHistory);
    }
    let index = Index::new(events);
    match index
        .select(events, target)
        .and_then(|leaf| index.trace(leaf, target))
    {
        Ok(branch) => BranchResolution::Resolved(branch),
        Err(reason) => BranchResolution::Unresolved(reason),
    }
}
