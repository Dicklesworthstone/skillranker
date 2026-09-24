//! Bounded request-first context windowing, privacy sanitization, and Jev provider payload rendering.
//!
//! Enforces boundary `p3_request_context`:
//! - Drop reasoning/thinking blocks and embedded binary/media data.
//! - Strip prior `sr` advisory blocks only when harness provenance identifies them as `sr` output;
//!   ordinary user quotes of advice markers are preserved.
//! - Preserve omission markers for images, files, and other non-text inputs.
//! - Essential missing attachments yield `unavailable / unsupported-context`.
//! - Request-first ordering: latest request appears once in `latest_user_request`; older context in `recent_messages`.
//! - Context budget: 12 logical messages / 12,000 Unicode scalars with deterministic head/tail omissions.
//! - Tools become structured summaries: tool name, allowlisted arguments, exit/error status,
//!   and redacted head/tail excerpts (default 200 chars), with error lines preserved.
//! - `--no-tools` removes tool arguments and results from remote context while retaining local observations.
//! - Whole-field redaction before truncation, followed by full-payload inspection.

use crate::context::signals::ProjectSignals;
use crate::context::tool::{
    DEFAULT_TOOL_EXCERPT_CHARS, allowlist_tool_arguments, head_tail_truncate, summarize_tool_result,
};
use crate::context::{EventKind, NormalizedContext, Role};
use crate::limits::{RECENT_NORMALIZED_MESSAGES, RENDERED_CONTEXT_SCALARS};
use crate::output::ContextQuality;
use crate::privacy::redaction::{RedactionError, Redactor};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::LazyLock;

/// Standard omission marker for dropped image data.
pub const IMAGE_OMISSION_MARKER: &str = "[image omitted]";
/// Standard omission marker for dropped arbitrary binary/media data.
pub const MEDIA_OMISSION_MARKER: &str = "[media omitted]";

// Thinking/reasoning block patterns to remove from model outputs.
static THINKING_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<thinking>.*?</thinking>|<thought>.*?</thought>|```thinking\s*.*?```")
        .expect("static thinking regex")
});

static UNTERMINATED_THINKING_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<thinking>.*$|<thought>.*$").expect("static unterminated thinking regex")
});

// Data URI pattern for embedded images or media.
static DATA_URI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"data:image/[a-zA-Z0-9.+/-]+;base64,[A-Za-z0-9+/=]+")
        .expect("static data URI regex")
});

static DATA_BINARY_URI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"data:application/[a-zA-Z0-9.+/-]+;base64,[A-Za-z0-9+/=]+")
        .expect("static data binary URI regex")
});

// Prior advisory markers emitted by SkillRanker.
static ADVISORY_MARKERS: &[&str] = &[
    "Suggested skill for the next step:",
    "Use it only if it fits the user's request and current instructions.",
    "[SkillRanker]",
    "[sr] Suggested skill:",
    "<!-- skillranker:advisory -->",
    "SkillRanker suggestion:",
];

/// Strips reasoning/thinking tags and blocks from text while preserving ordinary conclusions.
pub fn strip_thinking_blocks(text: &str) -> String {
    let stripped = THINKING_TAG.replace_all(text, "");
    let final_clean = UNTERMINATED_THINKING_TAG.replace_all(&stripped, "");
    final_clean.trim().to_string()
}

/// Drops embedded binary/base64 data while preserving standard omission markers.
pub fn sanitize_media_data(text: &str) -> String {
    let replaced_img = DATA_URI.replace_all(text, IMAGE_OMISSION_MARKER);
    let replaced_bin = DATA_BINARY_URI.replace_all(&replaced_img, MEDIA_OMISSION_MARKER);
    replaced_bin.to_string()
}

/// Checks if an event represents or contains a prior SkillRanker advisory block.
pub fn is_sr_advisory_text(text: &str) -> bool {
    ADVISORY_MARKERS.iter().any(|marker| text.contains(marker))
}

/// Strips prior advisory text from non-user messages. User messages are NEVER stripped.
pub fn strip_advisory_from_non_user(role: Role, text: &str) -> String {
    if role == Role::User {
        return text.to_string();
    }

    // Split into lines and filter out advisory lines
    let mut cleaned_lines = Vec::new();
    let mut skipping_block = false;

    for line in text.lines() {
        let is_marker_line = ADVISORY_MARKERS.iter().any(|m| line.contains(m));
        if is_marker_line {
            // Check if this starts a multi-line advisory block
            if line.contains("Suggested skill for the next step:") {
                skipping_block = true;
                continue;
            }
            continue;
        }

        if skipping_block {
            if line.contains("Use it only if it fits the user's request") {
                skipping_block = false;
                continue;
            }
            // If empty line right after advisory, skip
            if line.trim().is_empty() {
                continue;
            }
            skipping_block = false;
        }

        cleaned_lines.push(line);
    }

    cleaned_lines.join("\n").trim().to_string()
}

/// Derives language names from manifest markers in ProjectSignals.
pub fn detect_languages_from_markers(filenames: &[&str]) -> Vec<String> {
    let mut langs = BTreeSet::new();
    for name in filenames {
        match *name {
            "Cargo.toml" => {
                langs.insert("rust".to_string());
            }
            "lakefile.lean" | "lakefile.toml" => {
                langs.insert("lean".to_string());
            }
            "go.mod" => {
                langs.insert("go".to_string());
            }
            "package.json" => {
                langs.insert("javascript".to_string());
            }
            "pyproject.toml" => {
                langs.insert("python".to_string());
            }
            "Makefile" => {
                langs.insert("make".to_string());
            }
            "CMakeLists.txt" => {
                langs.insert("cmake".to_string());
            }
            "Gemfile" => {
                langs.insert("ruby".to_string());
            }
            "pom.xml" | "build.gradle" | "build.gradle.kts" => {
                langs.insert("java".to_string());
            }
            _ => {}
        }
    }
    langs.into_iter().collect()
}

/// Loaded reference summary included in session state sent to Jev.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedLoadedReference {
    pub name: String,
    pub summary: String,
}

/// Project signals sent to Jev.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedProjectSignals {
    pub languages: Vec<String>,
    pub tools_on_path: Vec<String>,
    pub dirty_paths: Vec<String>,
    pub dirty_paths_truncated: bool,
}

/// Session state sent to Jev.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedSessionState {
    pub loaded_references: Vec<RenderedLoadedReference>,
    pub loaded_state: String,
    pub explicit_exclusions: Vec<String>,
}

/// Message element within `recent_messages`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenderedMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The bounded internal state payload sent to Jev.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedContextPayload {
    pub schema_version: u32,
    #[serde(default)]
    pub context_profile: ContextProfile,
    pub harness: String,
    pub context_quality: ContextQuality,
    pub project_signals: RenderedProjectSignals,
    pub session_state: RenderedSessionState,
    pub recent_messages: Vec<RenderedMessage>,
    pub latest_user_request: String,
}

impl RenderedContextPayload {
    /// Serializes payload to compact JSON bytes after final inspection.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, RenderContextError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| RenderContextError::Serialization(e.to_string()))?;
        Redactor::default().inspect_payload(&bytes)?;
        Ok(bytes)
    }

    /// Converts payload to serde_json::Value.
    pub fn to_value(&self) -> Result<serde_json::Value, RenderContextError> {
        serde_json::to_value(self).map_err(|e| RenderContextError::Serialization(e.to_string()))
    }

    /// Returns true if context was deemed unsupported (e.g. missing essential attachments).
    pub fn is_unsupported_context(&self) -> bool {
        self.context_quality == ContextQuality::Insufficient
    }

    /// Returns the total size of the serialized payload in bytes.
    pub fn disclosed_bytes(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }

    /// Total Unicode scalar count across latest request and all recent messages.
    pub fn total_message_scalars(&self) -> usize {
        let req_scalars = self.latest_user_request.chars().count();
        let msg_scalars: usize = self
            .recent_messages
            .iter()
            .map(|m| {
                m.text.as_deref().unwrap_or("").chars().count()
                    + m.summary.as_deref().unwrap_or("").chars().count()
                    + m.tool.as_deref().unwrap_or("").chars().count()
                    + m.status.as_deref().unwrap_or("").chars().count()
            })
            .sum();
        req_scalars + msg_scalars
    }
}

use crate::privacy::{
    CategoryReceipt, ContextProfile, DisclosureReceipt, SourceCategory, is_essential_tool_reference,
};

/// Options controlling context windowing, budget, redaction, and tool filtering.
#[derive(Clone, Debug)]
pub struct RenderContextOptions<'a> {
    pub no_tools: bool,
    pub context_profile: ContextProfile,
    pub max_messages: usize,
    pub max_total_scalars: usize,
    pub tool_excerpt_chars: usize,
    pub redactor: Redactor,
    pub project_signals: Option<&'a ProjectSignals>,
    pub loaded_references: Vec<RenderedLoadedReference>,
    pub loaded_state: String,
    pub explicit_exclusions: Vec<String>,
    pub fail_on_unsupported_context: bool,
    pub essential_tool_missing: bool,
}

impl<'a> Default for RenderContextOptions<'a> {
    fn default() -> Self {
        Self {
            no_tools: false,
            context_profile: ContextProfile::Standard,
            max_messages: RECENT_NORMALIZED_MESSAGES.max(),
            max_total_scalars: RENDERED_CONTEXT_SCALARS.max(),
            tool_excerpt_chars: DEFAULT_TOOL_EXCERPT_CHARS,
            redactor: Redactor::default(),
            project_signals: None,
            loaded_references: Vec::new(),
            loaded_state: "observed".to_string(),
            explicit_exclusions: Vec::new(),
            fail_on_unsupported_context: false,
            essential_tool_missing: false,
        }
    }
}

/// Errors produced during context rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderContextError {
    Redaction(RedactionError),
    UnsupportedContext(String),
    Serialization(String),
    SecretsDetected(usize),
}

impl fmt::Display for RenderContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redaction(e) => write!(f, "redaction error: {e}"),
            Self::UnsupportedContext(msg) => write!(f, "unsupported context: {msg}"),
            Self::Serialization(msg) => write!(f, "failed to serialize rendered context: {msg}"),
            Self::SecretsDetected(count) => {
                write!(f, "payload inspection detected {count} secret(s)")
            }
        }
    }
}

impl std::error::Error for RenderContextError {}

impl From<RedactionError> for RenderContextError {
    fn from(err: RedactionError) -> Self {
        match err {
            RedactionError::SecretsDetected { count } => Self::SecretsDetected(count),
            other => Self::Redaction(other),
        }
    }
}

/// Renders a bounded, redacted, request-first context payload from normalized input,
/// along with a bounded field-level disclosure receipt.
///
/// Steps:
/// 1. Validate whether essential attachments are missing. If so, flags `Insufficient` context
///    (and errors if `fail_on_unsupported_context` is set).
/// 2. Redacts the latest request in full BEFORE any truncation.
/// 3. Reserves space for `latest_user_request` within `max_total_scalars`, truncating with
///    head/tail omission markers if it exceeds the scalar limit.
/// 4. Filters candidate events:
///    - Skips latest user request event (preventing duplication).
///    - If `--no-tools` is enabled, drops tool arguments and results.
///    - Strips reasoning/thinking tags from model messages.
///    - Strips prior `sr` advisory blocks from non-user messages (preserving user quotes).
///    - Sanitizes data URIs to standard omission markers.
///    - Redacts fields before length truncation.
/// 5. Windows to the most recent `max_messages` events.
/// 6. Enforces remaining scalar budget backwards from newest to oldest recent messages,
///    applying deterministic head/tail truncation where appropriate.
/// 7. Determines final `ContextQuality`.
/// 8. Assembles and runs full-payload secret inspection.
/// 9. Computes field-level category receipts and payload verification totals.
pub fn render_context_and_receipt(
    context: &NormalizedContext,
    options: &RenderContextOptions<'_>,
) -> Result<(RenderedContextPayload, DisclosureReceipt), RenderContextError> {
    let redactor = options.redactor;

    // 1. Check essential attachments and tool results
    let tool_bodies_omitted =
        options.no_tools || options.context_profile == ContextProfile::Minimal;
    let had_tool_events = context
        .events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ToolInvocation | EventKind::ToolResult));
    let essential_tool_indicated = options.essential_tool_missing
        || (had_tool_events && is_essential_tool_reference(context.current_request.text.as_str()));
    let essential_tool_omitted = essential_tool_indicated && tool_bodies_omitted;
    let essential_missing =
        context.current_request.essential_attachment_missing || essential_tool_omitted;

    if essential_missing && options.fail_on_unsupported_context {
        let reason = if context.current_request.essential_attachment_missing {
            "request meaning depends on omitted attachment"
        } else {
            "request meaning depends on omitted tool result"
        };
        return Err(RenderContextError::UnsupportedContext(reason.to_string()));
    }

    // 2. Redact full latest request text before any truncation
    let raw_req = context.current_request.text.as_str();
    let sanitized_req = sanitize_media_data(raw_req);
    let redacted_req = redactor.redact_field(&sanitized_req)?;
    let req_redactions = redacted_req.redaction_count();
    let mut req_text = redacted_req.into_string();

    let mut history_truncated = false;
    let mut req_truncated = false;

    // 3. Reserve room for latest_user_request first within max_total_scalars
    let req_scalars = req_text.chars().count();
    let remaining_budget = if req_scalars > options.max_total_scalars {
        req_text = head_tail_truncate(&req_text, options.max_total_scalars);
        req_truncated = true;
        0
    } else {
        options.max_total_scalars - req_scalars
    };

    let latest_event_id = context.current_request.event_id.as_ref();
    let mut total_messages_seen: usize = 0;
    let mut total_tools_seen: usize = 0;
    for event in &context.events {
        if let (Some(ev_id), Some(cur_id)) = (&event.event_id, latest_event_id)
            && ev_id == cur_id
        {
            continue;
        }
        match event.kind {
            EventKind::ToolInvocation | EventKind::ToolResult => total_tools_seen += 1,
            EventKind::Message => {
                if event.role == Role::Tool {
                    total_tools_seen += 1;
                } else {
                    total_messages_seen += 1;
                }
            }
            _ => {}
        }
    }

    let mut history_redactions = 0;
    let mut tool_redactions = 0;
    let mut tool_truncations = 0;
    let mut history_truncations = 0;

    // 4. Process older events into candidate RenderedMessage items
    let mut candidates: Vec<RenderedMessage> = Vec::new();

    // In Minimal profile, optional history and tool bodies are omitted entirely
    if options.context_profile != ContextProfile::Minimal {
        for event in &context.events {
            // Do not duplicate latest request in recent_messages
            if let (Some(ev_id), Some(cur_id)) = (&event.event_id, latest_event_id)
                && ev_id == cur_id
            {
                continue;
            }

            match event.kind {
                EventKind::ToolInvocation | EventKind::ToolResult => {
                    if options.no_tools {
                        continue;
                    }
                    if let Some(tool_ev) = &event.tool {
                        let tool_name = tool_ev.name.as_str().to_string();
                        let status_str = match tool_ev.status {
                            crate::context::ToolStatus::Succeeded => "succeeded",
                            crate::context::ToolStatus::Failed => "failed",
                            crate::context::ToolStatus::Attempted => "attempted",
                            crate::context::ToolStatus::Unknown => "unknown",
                        };

                        let mut summary_parts = Vec::new();
                        if let Some(args) = &tool_ev.arguments {
                            // Allowlist, then redact, then truncate: redacted
                            // JSON may no longer parse, and the allowlist must
                            // not depend on it.
                            let allowlisted = allowlist_tool_arguments(args.as_str());
                            let redacted_args = redactor.redact_field(&allowlisted)?;
                            tool_redactions += redacted_args.redaction_count();
                            let raw_args_str = redacted_args.into_string();
                            let arg_summary =
                                head_tail_truncate(&raw_args_str, options.tool_excerpt_chars / 2);
                            if raw_args_str.chars().count() > options.tool_excerpt_chars / 2 {
                                tool_truncations += 1;
                            }
                            if !arg_summary.is_empty() && arg_summary != "{}" {
                                summary_parts.push(arg_summary);
                            }
                        }
                        if let Some(res) = &tool_ev.result {
                            let sanitized_res = sanitize_media_data(res.as_str());
                            let redacted_res = redactor.redact_field(&sanitized_res)?;
                            tool_redactions += redacted_res.redaction_count();
                            let raw_res_str = redacted_res.into_string();
                            let (res_summary, _error_lines) =
                                summarize_tool_result(&raw_res_str, options.tool_excerpt_chars);
                            if raw_res_str.chars().count() > options.tool_excerpt_chars {
                                tool_truncations += 1;
                            }
                            if !res_summary.is_empty() {
                                summary_parts.push(res_summary);
                            }
                        }

                        let summary = if summary_parts.is_empty() {
                            None
                        } else {
                            Some(summary_parts.join(": "))
                        };

                        candidates.push(RenderedMessage {
                            role: "tool".to_string(),
                            tool: Some(tool_name),
                            status: Some(status_str.to_string()),
                            summary,
                            text: None,
                        });
                    }
                }
                EventKind::Message => {
                    let role_str = match event.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => {
                            if options.no_tools {
                                continue;
                            }
                            "tool"
                        }
                        Role::System => "system",
                    };

                    // Strip thinking/reasoning blocks for assistant/system
                    let cleaned_text = if event.role != Role::User {
                        let no_thinking = strip_thinking_blocks(event.text.as_str());
                        strip_advisory_from_non_user(event.role, &no_thinking)
                    } else {
                        event.text.as_str().to_string()
                    };

                    let media_clean = sanitize_media_data(&cleaned_text);
                    if media_clean.is_empty() {
                        continue;
                    }

                    let redacted_text = redactor.redact_field(&media_clean)?;
                    history_redactions += redacted_text.redaction_count();
                    let redacted_str = redacted_text.into_string();
                    if redacted_str.is_empty() {
                        continue;
                    }

                    candidates.push(RenderedMessage {
                        role: role_str.to_string(),
                        tool: None,
                        status: None,
                        summary: None,
                        text: Some(redacted_str),
                    });
                }
                EventKind::TaskBoundary
                | EventKind::Compaction
                | EventKind::Resume
                | EventKind::SessionEnd => {
                    // Non-message lifecycle events are not rendered in provider prompt context
                }
            }
        }
    }

    // 5. Window to max_messages
    if candidates.len() > options.max_messages {
        let drop_count = candidates.len() - options.max_messages;
        candidates.drain(0..drop_count);
        history_truncated = true;
    }

    // 6. Enforce scalar budget across recent_messages working backwards
    let mut selected_messages: Vec<RenderedMessage> = Vec::new();
    let mut budget_left = remaining_budget;

    for msg in candidates.into_iter().rev() {
        let msg_len = msg.text.as_deref().unwrap_or("").chars().count()
            + msg.summary.as_deref().unwrap_or("").chars().count()
            + msg.tool.as_deref().unwrap_or("").chars().count()
            + msg.status.as_deref().unwrap_or("").chars().count();

        if msg_len <= budget_left {
            budget_left -= msg_len;
            selected_messages.push(msg);
        } else {
            let overhead = msg.tool.as_deref().unwrap_or("").chars().count()
                + msg.status.as_deref().unwrap_or("").chars().count();
            if budget_left >= overhead + 30 {
                let avail = budget_left - overhead;
                let mut truncated_msg = msg;
                if let Some(txt) = truncated_msg.text.take() {
                    truncated_msg.text = Some(head_tail_truncate(&txt, avail));
                    history_truncations += 1;
                    selected_messages.push(truncated_msg);
                } else if let Some(sum) = truncated_msg.summary.take() {
                    truncated_msg.summary = Some(head_tail_truncate(&sum, avail));
                    tool_truncations += 1;
                    selected_messages.push(truncated_msg);
                }
            }
            history_truncated = true;
            break;
        }
    }

    selected_messages.reverse();

    // 7. Determine ContextQuality
    let context_quality = if essential_missing {
        ContextQuality::Insufficient
    } else if req_truncated || history_truncated {
        ContextQuality::Partial
    } else if selected_messages.is_empty() && context.events.is_empty() {
        ContextQuality::PromptOnly
    } else {
        ContextQuality::Complete
    };

    // Build project signals
    let mut signals_redactions = 0;
    let mut signals_omitted_dirty_paths = 0;

    let project_signals = if let Some(signals) = options.project_signals {
        let languages = detect_languages_from_markers(&signals.filenames);
        let tools_on_path = signals
            .tools_on_path
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (dirty_paths, dirty_paths_truncated) =
            if options.context_profile == ContextProfile::Minimal {
                if let Some(dp) = &signals.dirty_paths {
                    signals_omitted_dirty_paths = dp.paths.len();
                }
                (Vec::new(), false)
            } else if let Some(dp) = &signals.dirty_paths {
                let mut sanitized_paths = Vec::new();
                for p in &dp.paths {
                    let redacted = redactor.redact_field(p.as_str())?;
                    signals_redactions += redacted.redaction_count();
                    sanitized_paths.push(redacted.into_string());
                }
                (sanitized_paths, dp.truncated)
            } else {
                (Vec::new(), false)
            };

        RenderedProjectSignals {
            languages,
            tools_on_path,
            dirty_paths,
            dirty_paths_truncated,
        }
    } else {
        RenderedProjectSignals::default()
    };

    let session_state = RenderedSessionState {
        loaded_references: options.loaded_references.clone(),
        loaded_state: options.loaded_state.clone(),
        explicit_exclusions: options.explicit_exclusions.clone(),
    };

    let payload = RenderedContextPayload {
        schema_version: 1,
        context_profile: options.context_profile,
        harness: context.harness.as_str().to_string(),
        context_quality,
        project_signals,
        session_state,
        recent_messages: selected_messages,
        latest_user_request: req_text,
    };

    // 8. Final full-payload inspection
    let serialized = serde_json::to_vec(&payload)
        .map_err(|e| RenderContextError::Serialization(e.to_string()))?;
    redactor.inspect_payload(&serialized)?;

    // 9. Build DisclosureReceipt
    let messages_included = payload
        .recent_messages
        .iter()
        .filter(|m| m.role != "tool")
        .count();
    let tools_included = payload
        .recent_messages
        .iter()
        .filter(|m| m.role == "tool")
        .count();
    let messages_omitted = total_messages_seen.saturating_sub(messages_included);
    let tools_omitted = total_tools_seen.saturating_sub(tools_included);

    let signals_included = payload.project_signals.languages.len()
        + payload.project_signals.tools_on_path.len()
        + payload.project_signals.dirty_paths.len();
    let signals_truncated = if payload.project_signals.dirty_paths_truncated {
        1
    } else {
        0
    };

    let session_included = payload.session_state.loaded_references.len()
        + payload.session_state.explicit_exclusions.len();

    let req_included = if payload.latest_user_request.is_empty() {
        0
    } else {
        1
    };
    let req_omitted = if context.current_request.essential_attachment_missing {
        1
    } else {
        0
    };
    let req_truncated_count = if req_truncated { 1 } else { 0 };

    let cat_request = CategoryReceipt {
        category: SourceCategory::UserRequest,
        included_count: req_included,
        omitted_count: req_omitted,
        truncated_count: req_truncated_count,
        redaction_count: req_redactions,
    };
    let cat_history = CategoryReceipt {
        category: SourceCategory::MessageHistory,
        included_count: messages_included,
        omitted_count: messages_omitted,
        truncated_count: history_truncations,
        redaction_count: history_redactions,
    };
    let cat_tools = CategoryReceipt {
        category: SourceCategory::ToolEvents,
        included_count: tools_included,
        omitted_count: tools_omitted,
        truncated_count: tool_truncations,
        redaction_count: tool_redactions,
    };
    let cat_signals = CategoryReceipt {
        category: SourceCategory::ProjectSignals,
        included_count: signals_included,
        omitted_count: signals_omitted_dirty_paths,
        truncated_count: signals_truncated,
        redaction_count: signals_redactions,
    };
    let cat_session = CategoryReceipt {
        category: SourceCategory::SessionState,
        included_count: session_included,
        omitted_count: 0,
        truncated_count: 0,
        redaction_count: 0,
    };

    let categories = vec![
        cat_request,
        cat_history,
        cat_tools,
        cat_signals,
        cat_session,
    ];

    let total_included = categories.iter().map(|c| c.included_count).sum();
    let total_omitted = categories.iter().map(|c| c.omitted_count).sum();
    let total_truncated = categories.iter().map(|c| c.truncated_count).sum();
    let total_redactions = categories.iter().map(|c| c.redaction_count).sum();

    let receipt = DisclosureReceipt {
        schema_version: 1,
        context_profile: options.context_profile,
        context_quality,
        no_tools: options.no_tools,
        categories,
        total_included,
        total_omitted,
        total_truncated,
        total_redactions,
        disclosed_bytes: payload.disclosed_bytes(),
        disclosed_scalars: payload.total_message_scalars(),
    };

    Ok((payload, receipt))
}

/// Renders a bounded, redacted, request-first context payload from normalized input.
///
/// Convenience wrapper delegating to [`render_context_and_receipt`] and returning
/// solely the payload.
pub fn render_context(
    context: &NormalizedContext,
    options: &RenderContextOptions<'_>,
) -> Result<RenderedContextPayload, RenderContextError> {
    render_context_and_receipt(context, options).map(|(payload, _)| payload)
}
