//! Local normalized records, kept separate from a future allowlisted provider
//! payload. Decoding, byte/depth limits and native adapter conformance live at
//! the input boundary; serializing this entire envelope to a provider is invalid.

pub mod anchor;
pub mod branch;
#[cfg(unix)]
pub mod cass;
pub mod jsonl;
pub mod overlay;
pub mod render;
pub mod signals;
pub mod source;
pub mod tool;

pub use anchor::{
    AnchorDirective, AnchorDirectiveKind, AnchorError, AnchorProvenance, AnchorResolution,
    TaskAnchor, is_terse_continuation, resolve_task_anchor,
};
pub use branch::{
    ActiveBranch, BranchAdvice, BranchResolution, BranchResolutionTarget, LoadedSkillRecord,
    ResolvedWorktree, SkillSuppressionVerdict, SkillUsageKind, UnresolvedBranchReason,
    WorktreeError, evaluate_loaded_skill_eligibility, resolve_active_branch, resolve_worktree,
};
pub use overlay::{
    ClaudeOverlayRequest, ClaudeOverlayResult, OverlayError, apply_claude_prompt_overlay,
};
pub use render::{
    IMAGE_OMISSION_MARKER, MEDIA_OMISSION_MARKER, RenderContextError, RenderContextOptions,
    RenderedContextPayload, RenderedLoadedReference, RenderedMessage, RenderedProjectSignals,
    RenderedSessionState, detect_languages_from_markers, is_sr_advisory_text, render_context,
    render_context_and_receipt, sanitize_media_data, strip_advisory_from_non_user,
    strip_thinking_blocks,
};
pub use tool::{
    AssociatedToolCall, DEFAULT_TOOL_EXCERPT_CHARS, SimpleSkillResolver, SkillEvidenceResolver,
    SkillMatch, associate_tool_events, extract_load_observations, extract_loaded_skill_records,
    filter_events_for_provider, head_tail_truncate, summarize_tool_arguments,
    summarize_tool_result,
};

use crate::identity::{
    AgentId, BranchId, ContentHash, ContextEpoch, EventId, HarnessId, ProducerId, SessionId,
    SessionIdentity, SkillId, SourceProvenance, ToolCallId, TurnId, WorkspaceId,
    validate_definition_ids,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Private input text must never appear accidentally in Debug/error output.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrivateText(String);

impl PrivateText {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PrivateText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PrivateText(<{} bytes>)", self.0.len())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Message,
    ToolInvocation,
    ToolResult,
    TaskBoundary,
    Compaction,
    Resume,
    SessionEnd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Attempted,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEvent {
    pub call_id: Option<ToolCallId>,
    pub name: PrivateText,
    pub status: ToolStatus,
    pub arguments: Option<PrivateText>,
    pub result: Option<PrivateText>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedEvent {
    pub event_id: Option<EventId>,
    pub parent_id: Option<EventId>,
    pub turn_id: Option<TurnId>,
    pub agent_id: Option<AgentId>,
    pub branch_id: Option<BranchId>,
    pub role: Role,
    pub kind: EventKind,
    /// Source timestamp, never a substitute for native branch/event order.
    pub timestamp_unix_ms: Option<i64>,
    pub text: PrivateText,
    pub tool: Option<ToolEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentRequest {
    pub event_id: Option<EventId>,
    pub text: PrivateText,
    pub attachments_omitted: bool,
    pub essential_attachment_missing: bool,
}

/// Imports can state claims, never claim independent observation or delivery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppliedLoadClaim {
    pub skill_id: SkillId,
    pub source_content: Option<ContentHash>,
    pub rendered_content: Option<ContentHash>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOrigin {
    Observed,
    Supplied,
    Unknown,
}

impl SuppliedLoadClaim {
    pub fn evidence_origin(&self) -> EvidenceOrigin {
        EvidenceOrigin::Supplied
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    Attempted,
    ObservedLoaded,
    NotObserved,
    Unobservable,
    Censored,
}

/// An adapter-produced observation. This type is not deserializable from the
/// normalized input format; a successful path-only read retains unknown hashes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadObservation {
    pub event_id: Option<EventId>,
    pub skill_id: SkillId,
    pub state: LoadState,
    pub source_content: Option<ContentHash>,
    pub rendered_content: Option<ContentHash>,
    pub identity: SessionIdentity,
}

/// Version 1 local input. Unknown IDs serialize as null. Paths/text here confer
/// no read, network, tool or delivery authority. Source readers validate these
/// declarations against independently resolved local authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedContext {
    pub schema_version: u32,
    pub harness: HarnessId,
    pub producer_id: Option<ProducerId>,
    pub workspace_root: PrivateText,
    pub session_id: Option<SessionId>,
    pub agent_id: Option<AgentId>,
    pub branch_id: Option<BranchId>,
    pub context_epoch: Option<ContextEpoch>,
    pub current_request: CurrentRequest,
    pub events: Vec<NormalizedEvent>,
    #[serde(default)]
    pub explicit_skill_references: Vec<SkillId>,
    #[serde(default)]
    pub supplied_loads: Vec<SuppliedLoadClaim>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextError {
    InvalidJson,
    InvalidField,
    DuplicateKey,
    LimitExceeded,
    UnsupportedSchema,
    DuplicateEvent,
    DuplicateLoadDefinition,
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidJson => "invalid normalized context JSON",
            Self::InvalidField => "invalid normalized context field",
            Self::DuplicateKey => "duplicate normalized context key",
            Self::LimitExceeded => "normalized context limit exceeded",
            Self::UnsupportedSchema => "unsupported normalized context schema",
            Self::DuplicateEvent => "duplicate normalized event definition",
            Self::DuplicateLoadDefinition => "duplicate supplied load definition",
        })
    }
}

impl std::error::Error for ContextError {}

/// Decode bounded local input, rejecting duplicate keys before deserialization.
/// Paths and identities remain declarations, never filesystem or network authority.
pub fn parse_normalized_context(bytes: &[u8]) -> Result<NormalizedContext, ContextError> {
    use crate::adapter::AdapterError;
    let value =
        crate::adapter::decode_json(bytes, crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max())
            .map_err(|error| match error {
                AdapterError::DuplicateKey => ContextError::DuplicateKey,
                AdapterError::LimitExceeded => ContextError::LimitExceeded,
                _ => ContextError::InvalidJson,
            })?;
    let context: NormalizedContext =
        serde_json::from_value(value).map_err(|_| ContextError::InvalidField)?;
    context.validate_definitions()?;
    Ok(context)
}

impl NormalizedContext {
    pub fn validate_definitions(&self) -> Result<(), ContextError> {
        if self.schema_version != 1 {
            return Err(ContextError::UnsupportedSchema);
        }
        validate_definition_ids(self.events.iter().filter_map(|e| e.event_id.as_ref()))
            .map_err(|_| ContextError::DuplicateEvent)?;
        validate_definition_ids(self.supplied_loads.iter().map(|e| &e.skill_id))
            .map_err(|_| ContextError::DuplicateLoadDefinition)
    }

    /// `workspace` is resolved by the caller's trusted input boundary, not from
    /// workspace_root in this envelope. Even harness=claude_code stays normalized.
    pub fn session_identity(
        &self,
        workspace: Option<WorkspaceId>,
    ) -> Result<SessionIdentity, ContextError> {
        self.validate_definitions()?;
        Ok(SessionIdentity {
            source: SourceProvenance::Normalized {
                producer: self.producer_id.clone(),
                harness: self.harness.clone(),
                schema_version: self.schema_version,
            },
            workspace,
            session: self.session_id.clone(),
            agent: self.agent_id.clone(),
            branch: self.branch_id.clone(),
            epoch: self.context_epoch.clone(),
        })
    }
}
