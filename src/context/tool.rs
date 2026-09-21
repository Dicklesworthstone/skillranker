//! Tool and result association, argument allowlisting, head/tail excerpting,
//! remote provider context filtering, and local load evidence extraction.
//!
//! Enforces boundary `p3_tool_associations`:
//! - Bounded argument summaries using allowlisted fields.
//! - Bounded head/tail excerpts for results, preserving error lines.
//! - Association of tool invocations with tool results by `call_id` or sequential turn.
//! - Extraction of local skill load observations (`LoadState::Attempted`, `LoadState::ObservedLoaded`).
//! - Path-only file read evidence leaves content hashes unknown (`None`); current file hash
//!   is NEVER used as consumed historical version.
//! - Sibling fork/branch isolation: tool events in forks do not contaminate other branches.
//! - Local evidence is fully retained even when `--no-tools` strips tool data from remote context.
//! - Idempotent deduplication by event identity.

use crate::context::branch::{ActiveBranch, LoadedSkillRecord, SkillUsageKind};
use crate::context::{
    EventKind, LoadObservation, LoadState, NormalizedEvent, PrivateText, Role, ToolStatus,
};
use crate::identity::{
    AgentId, BranchId, ContentHash, ContextEpoch, EventId, SessionIdentity, SkillId, ToolCallId,
    TurnId,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

mod association;
mod dispatch;

/// Default maximum characters for tool argument or result excerpts.
pub const DEFAULT_TOOL_EXCERPT_CHARS: usize = 200;

/// Allowlisted field keys in JSON tool arguments.
pub const ALLOWLISTED_TOOL_ARGUMENT_KEYS: &[&str] = &[
    "command",
    "path",
    "file_path",
    "query",
    "skill",
    "skill_name",
    "name",
    "pattern",
    "target",
    "args",
    "subcommand",
];

/// Known error line indicators to preserve in tool summaries.
const ERROR_INDICATORS: &[&str] = &[
    "error:",
    "error[",
    "failed:",
    "fatal:",
    "panic:",
    "exception:",
    "exit code:",
    "exit status:",
    "[error]",
    "err:",
    "stderr:",
];

/// Head/tail excerpt truncation preserving bounded total length.
pub fn head_tail_truncate(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }

    // Measure marker size
    let placeholder_digits = char_count.to_string().len() + 1;
    let sample_marker = format!(
        " ... [{} chars omitted] ... ",
        "0".repeat(placeholder_digits)
    );
    let marker_len = sample_marker.chars().count();

    if max_chars <= marker_len {
        return text.chars().take(max_chars).collect();
    }

    let budget = max_chars - marker_len;
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let omitted = char_count.saturating_sub(head_chars + tail_chars);
    let marker = format!(" ... [{} chars omitted] ... ", omitted);

    let head: String = text.chars().take(head_chars).collect();
    let tail: String = text
        .chars()
        .skip(char_count.saturating_sub(tail_chars))
        .collect();

    format!("{}{}{}", head, marker, tail)
}

/// Summarize tool arguments by retaining only allowlisted JSON keys,
/// then applying bounded head/tail excerpting.
pub fn summarize_tool_arguments(raw_args: &str, max_chars: usize) -> String {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw_args) {
        let mut filtered = serde_json::Map::new();
        for (k, v) in map {
            if ALLOWLISTED_TOOL_ARGUMENT_KEYS.contains(&k.as_str()) {
                filtered.insert(k, v);
            } else {
                filtered.insert(k, Value::String("<omitted>".to_string()));
            }
        }
        let serialized = Value::Object(filtered).to_string();
        head_tail_truncate(&serialized, max_chars)
    } else {
        head_tail_truncate(raw_args, max_chars)
    }
}

/// Summarize tool results by detecting error lines and applying bounded head/tail excerpting.
pub fn summarize_tool_result(raw_result: &str, max_chars: usize) -> (String, Vec<String>) {
    let mut error_lines = Vec::new();
    for line in raw_result.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if ERROR_INDICATORS.iter().any(|ind| lower.contains(ind)) {
            error_lines.push(trimmed.to_string());
        }
    }

    let excerpt = head_tail_truncate(raw_result, max_chars);
    (excerpt, error_lines)
}

/// An associated tool invocation and its matching result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssociatedToolCall {
    pub call_id: Option<ToolCallId>,
    pub invocation_event_id: Option<EventId>,
    pub result_event_id: Option<EventId>,
    pub turn_id: Option<TurnId>,
    pub agent_id: Option<AgentId>,
    pub branch_id: Option<BranchId>,
    pub tool_name: PrivateText,
    pub status: ToolStatus,
    pub raw_arguments: Option<PrivateText>,
    pub raw_result: Option<PrivateText>,
    pub arguments_summary: Option<PrivateText>,
    pub result_summary: Option<PrivateText>,
    pub error_lines: Vec<String>,
}

/// Associate tool invocations and results from a sequence of normalized events.
pub fn associate_tool_events(
    events: &[NormalizedEvent],
    max_excerpt_chars: usize,
) -> Vec<AssociatedToolCall> {
    associate_tool_event_iter(events.iter(), max_excerpt_chars)
}

fn associate_tool_event_iter<'a>(
    events: impl Iterator<Item = &'a NormalizedEvent>,
    max_excerpt_chars: usize,
) -> Vec<AssociatedToolCall> {
    let mut associated = Vec::new();
    let mut pending_by_call_id = BTreeMap::new();
    let mut pending_by_name = BTreeMap::new();
    let mut seen_event_ids = HashSet::new();

    for event in events {
        // IDs belong to an agent/branch namespace, not the whole input slice.
        let scope = (event.agent_id.as_ref(), event.branch_id.as_ref());
        if let Some(eid) = &event.event_id
            && !seen_event_ids.insert((scope, eid))
        {
            continue;
        }

        let Some(tool) = &event.tool else {
            continue;
        };

        let tool_name_str = tool.name.as_str();

        match event.kind {
            EventKind::ToolInvocation => {
                let (args_summary, _) = tool
                    .arguments
                    .as_ref()
                    .map(|a| {
                        (
                            Some(summarize_tool_arguments(a.as_str(), max_excerpt_chars)),
                            (),
                        )
                    })
                    .unwrap_or((None, ()));

                // Combined invocation/result records need the same error
                // checks as separately delivered results. Ignoring an inline
                // error would manufacture a successful load.
                let (res_summary, error_lines) = tool
                    .result
                    .as_ref()
                    .map(|r| {
                        let (summary, errors) = summarize_tool_result(r.as_str(), max_excerpt_chars);
                        (Some(summary), errors)
                    })
                    .unwrap_or((None, Vec::new()));
                let idx = associated.len();
                associated.push(AssociatedToolCall {
                    call_id: tool.call_id.clone(),
                    invocation_event_id: event.event_id.clone(),
                    result_event_id: None,
                    turn_id: event.turn_id.clone(),
                    agent_id: event.agent_id.clone(),
                    branch_id: event.branch_id.clone(),
                    tool_name: tool.name.clone(),
                    status: tool.status,
                    raw_arguments: tool.arguments.clone(),
                    raw_result: tool.result.clone(),
                    arguments_summary: args_summary.map(PrivateText::new),
                    result_summary: res_summary.map(PrivateText::new),
                    error_lines,
                });

                let completed = tool.result.is_some()
                    && matches!(tool.status, ToolStatus::Succeeded | ToolStatus::Failed);
                if let Some(cid) = &tool.call_id {
                    association::register(
                        &mut pending_by_call_id,
                        (scope, cid),
                        idx,
                        &mut associated,
                        false,
                        completed,
                    );
                } else {
                    // A name is only a sequential fallback within this turn.
                    association::register(
                        &mut pending_by_name,
                        (scope, event.turn_id.as_ref(), tool_name_str),
                        idx,
                        &mut associated,
                        true,
                        completed,
                    );
                }
            }
            EventKind::ToolResult => {
                let (res_summary, error_lines) = tool
                    .result
                    .as_ref()
                    .map(|r| {
                        let (s, errs) = summarize_tool_result(r.as_str(), max_excerpt_chars);
                        (Some(s), errs)
                    })
                    .unwrap_or((None, Vec::new()));

                let (matched_idx, ambiguous) = if let Some(cid) = &tool.call_id {
                    association::take(&mut pending_by_call_id, &(scope, cid), &mut associated)
                } else {
                    association::take(
                        &mut pending_by_name,
                        &(scope, event.turn_id.as_ref(), tool_name_str),
                        &mut associated,
                    )
                };

                if let Some(idx) = matched_idx {
                    let call = &mut associated[idx];
                    call.result_event_id = event.event_id.clone();
                    call.status = tool.status;
                    call.raw_result = tool.result.clone();
                    call.result_summary = res_summary.map(PrivateText::new);
                    call.error_lines = error_lines;
                    if call.turn_id.is_none() {
                        call.turn_id = event.turn_id.clone();
                    }
                } else {
                    // Orphan tool result
                    associated.push(AssociatedToolCall {
                        call_id: tool.call_id.clone(),
                        invocation_event_id: None,
                        result_event_id: event.event_id.clone(),
                        turn_id: event.turn_id.clone(),
                        agent_id: event.agent_id.clone(),
                        branch_id: event.branch_id.clone(),
                        tool_name: tool.name.clone(),
                        status: if ambiguous {
                            ToolStatus::Unknown
                        } else {
                            tool.status
                        },
                        raw_arguments: tool.arguments.clone(),
                        raw_result: tool.result.clone(),
                        arguments_summary: None,
                        result_summary: res_summary.map(PrivateText::new),
                        error_lines,
                    });
                }
            }
            _ => {
                // Event contains tool call and/or result directly
                let args_summary = tool
                    .arguments
                    .as_ref()
                    .map(|a| summarize_tool_arguments(a.as_str(), max_excerpt_chars));
                let (res_summary, error_lines) = tool
                    .result
                    .as_ref()
                    .map(|r| {
                        let (s, errs) = summarize_tool_result(r.as_str(), max_excerpt_chars);
                        (Some(s), errs)
                    })
                    .unwrap_or((None, Vec::new()));

                associated.push(AssociatedToolCall {
                    call_id: tool.call_id.clone(),
                    invocation_event_id: event.event_id.clone(),
                    result_event_id: event.event_id.clone(),
                    turn_id: event.turn_id.clone(),
                    agent_id: event.agent_id.clone(),
                    branch_id: event.branch_id.clone(),
                    tool_name: tool.name.clone(),
                    status: tool.status,
                    raw_arguments: tool.arguments.clone(),
                    raw_result: tool.result.clone(),
                    arguments_summary: args_summary.map(PrivateText::new),
                    result_summary: res_summary.map(PrivateText::new),
                    error_lines,
                });
            }
        }
    }

    associated
}

/// Metadata describing a skill match from a tool invocation or file read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMatch {
    pub skill_id: SkillId,
    pub usage_kind: SkillUsageKind,
    pub source_content: Option<ContentHash>,
    pub rendered_content: Option<ContentHash>,
    pub has_dynamic_arguments: bool,
    pub turn_scoped: bool,
}

/// Resolver interface for identifying skill loads from tool events.
pub trait SkillEvidenceResolver {
    /// Resolve a tool name and optional arguments JSON to a known skill match.
    fn resolve_tool(&self, tool_name: &str, arguments_json: Option<&str>) -> Option<SkillMatch>;

    /// Resolve a file read path to a known skill match.
    fn resolve_file_read(&self, path: &str) -> Option<SkillMatch>;
}

/// Simple in-memory skill evidence resolver.
#[derive(Clone, Debug, Default)]
pub struct SimpleSkillResolver {
    pub tool_skills: BTreeMap<String, SkillMatch>,
    pub path_skills: BTreeMap<String, SkillMatch>,
}

impl SimpleSkillResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_tool(&mut self, name: impl Into<String>, skill: SkillMatch) {
        self.tool_skills.insert(name.into(), skill);
    }

    pub fn register_path(&mut self, path: impl Into<String>, skill: SkillMatch) {
        self.path_skills.insert(path.into(), skill);
    }
}

impl SkillEvidenceResolver for SimpleSkillResolver {
    fn resolve_tool(&self, tool_name: &str, arguments_json: Option<&str>) -> Option<SkillMatch> {
        dispatch::resolve_tool(self, tool_name, arguments_json)
    }

    fn resolve_file_read(&self, path: &str) -> Option<SkillMatch> {
        // A registered current revision is not evidence of the bytes consumed
        // by a historical file read. Enforce this for every caller, not just
        // the pipeline's roster-to-resolver registration helper.
        let mut matched = self.path_skills.get(path)?.clone();
        matched.source_content = None;
        matched.rendered_content = None;
        Some(matched)
    }
}

/// Extract local skill load observations from normalized events.
///
/// Guarantees:
/// - Sibling fork/branch isolation: tool events in sibling branches are strictly excluded.
/// - Unsuccessful invocations (or failed results) become `LoadState::Attempted`.
/// - Successful invocations become `LoadState::ObservedLoaded`.
/// - Path-only file reads retain unknown versions (`source_content: None, rendered_content: None`);
///   current file hash on disk is NEVER used as historical evidence.
/// - Deduplicated by event identity.
pub fn extract_load_observations(
    events: &[NormalizedEvent],
    session_identity: &SessionIdentity,
    resolver: &impl SkillEvidenceResolver,
    active_branch: Option<&ActiveBranch>,
) -> Vec<LoadObservation> {
    // Resolve the lineage before joining calls. A sibling result must not
    // upgrade an active invocation even when branch labels are absent.
    let active_ids = active_branch.map(|b| b.event_id_set());
    let associated = associate_tool_event_iter(
        events.iter().filter(|event| {
            active_ids
                .as_ref()
                .is_none_or(|ids| event.event_id.as_ref().is_some_and(|id| ids.contains(id)))
        }),
        DEFAULT_TOOL_EXCERPT_CHARS,
    );
    let mut observations = Vec::new();

    for call in associated {
        // Enforce branch DAG isolation
        if let Some(branch_id) = &call.branch_id
            && let Some(active) = active_branch
            && active.branch_id.as_ref() != Some(branch_id)
        {
            continue; // Sibling fork/branch
        }

        if let Some(active_set) = &active_ids {
            let inv_in_branch = call
                .invocation_event_id
                .as_ref()
                .map(|id| active_set.contains(id))
                .unwrap_or(false);
            let res_in_branch = call
                .result_event_id
                .as_ref()
                .map(|id| active_set.contains(id))
                .unwrap_or(false);

            if !inv_in_branch && !res_in_branch {
                continue; // Neither invocation nor result is on the active branch
            }
        }

        let args_str = call.raw_arguments.as_ref().map(|a| a.as_str());
        let skill_match = resolver.resolve_tool(call.tool_name.as_str(), args_str);

        let Some(matched) = skill_match else {
            continue; // Not a recognized skill tool
        };

        // Determine load state
        let state = match call.status {
            ToolStatus::Succeeded => {
                if !call.error_lines.is_empty() {
                    LoadState::Attempted
                } else {
                    LoadState::ObservedLoaded
                }
            }
            ToolStatus::Failed | ToolStatus::Attempted | ToolStatus::Unknown => {
                LoadState::Attempted
            }
        };

        let event_id = call.invocation_event_id.or(call.result_event_id);
        // Association already deduplicates event delivery. Distinct loads of
        // the same skill are separate observations, even with equal outcomes.
        observations.push(LoadObservation {
            event_id,
            skill_id: matched.skill_id,
            state,
            source_content: matched.source_content,
            rendered_content: matched.rendered_content,
            identity: session_identity.clone(),
        });
    }

    observations
}

/// Extract verified `LoadedSkillRecord`s for eligibility and suppression evaluation.
pub fn extract_loaded_skill_records(
    events: &[NormalizedEvent],
    resolver: &impl SkillEvidenceResolver,
    active_branch: Option<&ActiveBranch>,
    current_epoch: &ContextEpoch,
) -> Vec<LoadedSkillRecord> {
    let active_ids = active_branch.map(|b| b.event_id_set());
    let associated = associate_tool_event_iter(
        events.iter().filter(|event| {
            active_ids
                .as_ref()
                .is_none_or(|ids| event.event_id.as_ref().is_some_and(|id| ids.contains(id)))
        }),
        DEFAULT_TOOL_EXCERPT_CHARS,
    );
    let epochs = active_branch.map(super::branch::event_epochs);
    let mut records = Vec::new();

    for call in associated {
        // Enforce branch DAG isolation
        if let Some(branch_id) = &call.branch_id
            && let Some(active) = active_branch
            && active.branch_id.as_ref() != Some(branch_id)
        {
            continue; // Sibling fork/branch
        }

        if let Some(active_set) = &active_ids {
            let inv_in_branch = call
                .invocation_event_id
                .as_ref()
                .map(|id| active_set.contains(id))
                .unwrap_or(false);
            let res_in_branch = call
                .result_event_id
                .as_ref()
                .map(|id| active_set.contains(id))
                .unwrap_or(false);

            if !inv_in_branch && !res_in_branch {
                continue;
            }
        }

        let args_str = call.raw_arguments.as_ref().map(|a| a.as_str());
        let skill_match = resolver.resolve_tool(call.tool_name.as_str(), args_str);

        let Some(matched) = skill_match else {
            continue;
        };

        // Only successful loads without errors count as proven loaded records
        if call.status != ToolStatus::Succeeded || !call.error_lines.is_empty() {
            continue;
        }

        let event_id = call.invocation_event_id.or(call.result_event_id);
        // A load keeps the epoch of its own event; a later compaction must be
        // able to invalidate it. Without a branch no suppression is possible.
        let epoch = match (&epochs, &event_id) {
            (Some(epochs), Some(id)) => match epochs.get(id) {
                Some(epoch) => epoch.clone(),
                None => continue,
            },
            (Some(_), None) => continue,
            (None, _) => current_epoch.clone(),
        };
        // Retain each identified load: an earlier epoch or content revision
        // cannot erase later evidence that the reference is present again.
        records.push(LoadedSkillRecord {
            skill_id: matched.skill_id,
            event_id,
            turn_id: call.turn_id,
            usage_kind: matched.usage_kind,
            epoch,
            source_content: matched.source_content,
            rendered_content: matched.rendered_content,
            has_dynamic_arguments: matched.has_dynamic_arguments,
            turn_scoped: matched.turn_scoped,
        });
    }

    records
}

/// Filter events for remote provider context.
///
/// When `no_tools` is true:
/// - Tool arguments and results are stripped from all events.
/// - Tool role messages have text omitted.
///
/// When `no_tools` is false:
/// - Arguments and results are summarized to bounded length.
pub fn filter_events_for_provider(
    events: &[NormalizedEvent],
    no_tools: bool,
    max_excerpt_chars: usize,
) -> Vec<NormalizedEvent> {
    events
        .iter()
        .map(|event| {
            let mut filtered = event.clone();
            if let Some(tool) = &mut filtered.tool {
                if no_tools {
                    tool.arguments = None;
                    tool.result = None;
                } else {
                    if let Some(args) = &tool.arguments {
                        tool.arguments = Some(PrivateText::new(summarize_tool_arguments(
                            args.as_str(),
                            max_excerpt_chars,
                        )));
                    }
                    if let Some(res) = &tool.result {
                        let (excerpt, _) = summarize_tool_result(res.as_str(), max_excerpt_chars);
                        tool.result = Some(PrivateText::new(excerpt));
                    }
                }
            }

            if no_tools && filtered.role == Role::Tool {
                filtered.text = PrivateText::new("");
            }

            filtered
        })
        .collect()
}
