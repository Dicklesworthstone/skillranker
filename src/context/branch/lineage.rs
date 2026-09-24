//! Scope-aware event identity and parent-link selection. No filesystem effects.
//!
//! Explicit target qualifiers constrain the leaf even when an event ID is
//! supplied. Ancestors stay in that leaf's agent namespace. A parent first
//! resolves in the child's branch; a unique same-agent ancestor in another
//! branch can establish a fork, but an ambiguous bare parent ID cannot.

use super::{
    ActiveBranch, BranchResolution, BranchResolutionTarget, UnresolvedBranchReason, epoch_name,
};
use crate::context::{EventKind, NormalizedEvent, Role};
use crate::identity::{AgentId, BranchId, EventId, ToolCallId};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

type EventKey<'a> = (Option<&'a AgentId>, Option<&'a BranchId>, &'a EventId);
type AgentEventKey<'a> = (Option<&'a AgentId>, &'a EventId);

struct Index<'a> {
    events: BTreeMap<EventKey<'a>, &'a NormalizedEvent>,
    by_agent: BTreeMap<AgentEventKey<'a>, Vec<EventKey<'a>>>,
    conflicts: BTreeSet<EventKey<'a>>,
    /// Provider response each event is a fragment of, when the source says.
    responses: &'a BTreeMap<EventId, String>,
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

/// A system message with no tool payload: a harness notice (Claude's
/// informational, turn-duration and away-summary records) or the empty
/// placeholder the Claude overlay keeps for harness-internal records so parent
/// links stay intact. The user's conversation never continues from one.
fn carries_no_conversation(event: &NormalizedEvent) -> bool {
    event.role == Role::System && event.kind == EventKind::Message && event.tool.is_none()
}

/// A tool result whose parent is the invocation it answers, matched by call ID.
fn answers_own_call(result: &NormalizedEvent, call: Option<&NormalizedEvent>) -> bool {
    let Some(call) = call else {
        return false;
    };
    let call_id = |event: &NormalizedEvent| event.tool.as_ref().and_then(|t| t.call_id.clone());
    result.kind == EventKind::ToolResult
        && call.kind == EventKind::ToolInvocation
        && call_id(result).is_some()
        && call_id(result) == call_id(call)
}

/// What the subtree below a tool call of one provider response holds.
enum Batch<'a> {
    /// Only fragments of that response, results answering its calls, and
    /// content-free harness records, with every call answered: the subtree.
    Answered(Vec<EventKey<'a>>),
    /// A later response or another turn: the conversation goes on here.
    Beyond,
    /// A call still waiting for its result, or a result for another call.
    Incomplete,
}

impl<'a> Index<'a> {
    fn new(events: &'a [NormalizedEvent], responses: &'a BTreeMap<EventId, String>) -> Self {
        let mut index = Self {
            events: BTreeMap::new(),
            by_agent: BTreeMap::new(),
            conflicts: BTreeSet::new(),
            responses,
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
        if let Some(event) =
            self.events
                .get(&(event.agent_id.as_ref(), event.branch_id.as_ref(), parent))
        {
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
            let first =
                candidates
                    .next()
                    .ok_or_else(|| UnresolvedBranchReason::TargetEventNotFound {
                        target: id.clone(),
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
            _ => {
                // Side branches are not where the conversation continues:
                // harness records (a PreToolUse hook's `hook_success`
                // attachment, a parentless informational notice) and a tool
                // result filed beside its own call while that call's message
                // goes on elsewhere. Pruning them is structural, not a
                // file-order guess: two conversation leaves remain a fork.
                let substantive = self.substantive_leaves(target);
                match substantive.as_slice() {
                    [leaf] => Ok(*leaf),
                    [] => Err(Self::forks(&leaves)),
                    _ => Err(Self::forks(&substantive)),
                }
            }
        }
    }

    fn forks(leaves: &[&NormalizedEvent]) -> UnresolvedBranchReason {
        UnresolvedBranchReason::AmbiguousSiblingForks {
            candidate_leaves: leaves
                .iter()
                .filter_map(|event| event.event_id.clone())
                .collect(),
        }
    }

    /// In-scope leaves left after removing side branches that no conversation
    /// continues from. First, system-message dead ends, repeatedly: a parent
    /// becomes a candidate once all of its children are pruned. Then a tool
    /// result filed beside its own call while that call's message goes on
    /// through another surviving child (Claude's layout for parallel tool
    /// calls and for calls a PreToolUse hook blocked). Last, a subtree of
    /// answered calls from a response that goes on elsewhere (see
    /// [`Self::answered_side_batches`]).
    fn substantive_leaves(&self, target: &BranchResolutionTarget) -> Vec<&'a NormalizedEvent> {
        let mut children: BTreeMap<EventKey<'a>, usize> = BTreeMap::new();
        let mut parent_of: BTreeMap<EventKey<'a>, EventKey<'a>> = BTreeMap::new();
        for event in self.events.values() {
            if let (Some(child), Ok(Some(parent))) = (key(event), self.parent(event))
                && let Some(parent) = key(parent)
            {
                *children.entry(parent).or_default() += 1;
                parent_of.insert(child, parent);
            }
        }
        let is_leaf = |children: &BTreeMap<EventKey<'a>, usize>, key: &EventKey<'a>| {
            children.get(key).is_none_or(|count| *count == 0)
        };
        let mut pruned = BTreeSet::new();
        let prune = |key: EventKey<'a>,
                     children: &mut BTreeMap<EventKey<'a>, usize>,
                     pruned: &mut BTreeSet<EventKey<'a>>| {
            pruned.insert(key);
            let parent = parent_of.get(&key).copied()?;
            let count = children.get_mut(&parent)?;
            *count -= 1;
            (*count == 0).then_some(parent)
        };

        let mut pending: Vec<EventKey<'a>> = self
            .events
            .keys()
            .filter(|key| !children.contains_key(*key))
            .copied()
            .collect();
        while let Some(candidate) = pending.pop() {
            let notice = self
                .events
                .get(&candidate)
                .is_some_and(|event| carries_no_conversation(event));
            if pruned.contains(&candidate) || !is_leaf(&children, &candidate) || !notice {
                continue;
            }
            if let Some(parent) = prune(candidate, &mut children, &mut pruned) {
                pending.push(parent);
            }
        }

        let side_results: Vec<EventKey<'a>> = self
            .events
            .iter()
            .filter(|(key, event)| {
                !pruned.contains(*key)
                    && is_leaf(&children, key)
                    && parent_of.get(*key).is_some_and(|parent| {
                        answers_own_call(event, self.events.get(parent).copied())
                    })
            })
            .map(|(key, _)| *key)
            .collect();
        let mut candidates_under: BTreeMap<EventKey<'a>, usize> = BTreeMap::new();
        for result in &side_results {
            *candidates_under.entry(parent_of[result]).or_default() += 1;
        }
        for result in side_results {
            let call = parent_of[&result];
            let surviving = children.get(&call).copied().unwrap_or(0);
            if surviving > candidates_under[&call] {
                prune(result, &mut children, &mut pruned);
            }
        }
        // A tool result whose call precedes a truncated window (sr-l1nr). A
        // bounded tail read can begin between a response's parallel calls and
        // their results, stranding the side result filed beside its own call
        // without that call to prove it. The conversation never continues from
        // such a fragment, since its newest records are all in the window. An
        // orphan of any other kind is left alone and still counts as a fork.
        for (&key, event) in &self.events {
            if !pruned.contains(&key)
                && is_leaf(&children, &key)
                && event.kind == EventKind::ToolResult
                && event.parent_id.is_some()
                && matches!(self.parent(event), Ok(None))
            {
                pruned.insert(key);
            }
        }

        if !self.responses.is_empty() {
            let mut kids: BTreeMap<EventKey<'a>, Vec<EventKey<'a>>> = BTreeMap::new();
            for (&child, &parent) in &parent_of {
                kids.entry(parent).or_default().push(child);
            }
            // Decided on the whole graph, not on what earlier pruning left, so
            // the outcome does not depend on visiting order.
            let side_batches: Vec<(EventKey<'a>, Vec<EventKey<'a>>)> = kids
                .keys()
                .flat_map(|&fork| self.answered_side_batches(fork, &kids))
                .collect();
            for (head, subtree) in side_batches {
                if !pruned.contains(&head) {
                    prune(head, &mut children, &mut pruned);
                }
                pruned.extend(subtree);
            }
        }
        self.events
            .iter()
            .filter(|(key, event)| {
                matches_target(event, target)
                    && !pruned.contains(*key)
                    && children.get(*key).is_none_or(|count| *count == 0)
            })
            .map(|(_, event)| *event)
            .collect()
    }

    /// Children of `fork` that head an answered batch of parallel tool calls,
    /// with their subtrees, when the response goes on elsewhere. `fork` must be
    /// a tool call of a known response. A sibling that goes on, either another
    /// fragment of that response or the result answering `fork`'s call, must
    /// reach a later response or turn. When every child is a finished batch,
    /// nothing shows which one continues, so the fork stands.
    fn answered_side_batches(
        &self,
        fork: EventKey<'a>,
        kids: &BTreeMap<EventKey<'a>, Vec<EventKey<'a>>>,
    ) -> Vec<(EventKey<'a>, Vec<EventKey<'a>>)> {
        let (Some(fork_event), Some(response), Some(children)) = (
            self.events.get(&fork),
            self.responses.get(fork.2),
            kids.get(&fork),
        ) else {
            return Vec::new();
        };
        if fork_event.kind != EventKind::ToolInvocation || children.len() < 2 {
            return Vec::new();
        }
        let own_call = fork_event.tool.as_ref().and_then(|t| t.call_id.as_ref());
        let batches: Vec<_> = children
            .iter()
            .map(|&child| (child, self.batch(child, response, own_call, kids)))
            .collect();
        let goes_on = batches.iter().any(|(child, batch)| {
            matches!(batch, Batch::Beyond)
                && self.events.get(child).is_some_and(|event| {
                    self.responses.get(child.2) == Some(response)
                        || answers_own_call(event, Some(fork_event))
                })
        });
        if !goes_on {
            return Vec::new();
        }
        batches
            .into_iter()
            .filter_map(|(child, batch)| match batch {
                Batch::Answered(subtree) => Some((child, subtree)),
                Batch::Beyond | Batch::Incomplete => None,
            })
            .collect()
    }

    /// Classify the subtree at `head` below a tool call of `response`.
    fn batch(
        &self,
        head: EventKey<'a>,
        response: &str,
        own_call: Option<&ToolCallId>,
        kids: &BTreeMap<EventKey<'a>, Vec<EventKey<'a>>>,
    ) -> Batch<'a> {
        let mut subtree = vec![head];
        let mut calls = BTreeSet::new();
        let mut answers = Vec::new();
        let mut next = 0;
        while let Some(&key) = subtree.get(next) {
            next += 1;
            let Some(event) = self.events.get(&key) else {
                return Batch::Incomplete;
            };
            let call_id = event.tool.as_ref().and_then(|t| t.call_id.as_ref());
            match (event.role, event.kind) {
                (Role::Assistant, EventKind::ToolInvocation | EventKind::Message)
                    if self.responses.get(key.2).is_some_and(|r| r == response) =>
                {
                    if event.kind == EventKind::ToolInvocation {
                        let Some(call_id) = call_id else {
                            return Batch::Incomplete;
                        };
                        calls.insert(call_id);
                    }
                }
                (_, EventKind::ToolResult) => {
                    let Some(call_id) = call_id else {
                        return Batch::Incomplete;
                    };
                    answers.push(call_id);
                }
                _ if carries_no_conversation(event) => {}
                _ => return Batch::Beyond,
            }
            subtree.extend(kids.get(&key).into_iter().flatten().copied());
        }
        let answered: BTreeSet<_> = answers.iter().copied().collect();
        let complete = calls.iter().all(|call| answered.contains(call))
            && answers
                .iter()
                .all(|answer| calls.contains(answer) || own_call == Some(*answer));
        if complete {
            Batch::Answered(subtree)
        } else {
            Batch::Incomplete
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
            if let (Some(expected), Some(observed)) = (&target.target_branch_id, &current.branch_id)
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
            branch_id: target.target_branch_id.clone().or_else(|| {
                lineage
                    .iter()
                    .rev()
                    .find_map(|event| event.branch_id.clone())
            }),
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
    responses: &BTreeMap<EventId, String>,
) -> BranchResolution {
    if events.is_empty() {
        return BranchResolution::Unresolved(UnresolvedBranchReason::EmptyHistory);
    }
    let index = Index::new(events, responses);
    match index
        .select(events, target)
        .and_then(|leaf| index.trace(leaf, target))
    {
        Ok(branch) => BranchResolution::Resolved(branch),
        Err(reason) => BranchResolution::Unresolved(reason),
    }
}
