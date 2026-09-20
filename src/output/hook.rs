//! Claude hook output protocol renderer and validation.
//!
//! Satisfies the Claude UserPromptSubmit hook contract (P6, sr-roadmap-l1i.7.2):
//! - Dedicated protocol envelope with `hookSpecificOutput`.
//! - Suggests at most one skill for ranked decisions.
//! - Formats explicit directives as user requests.
//! - Bounds `additionalContext` to 1,024 Unicode scalar values.
//! - Forbids dangerous ASCII/Unicode control characters.
//! - Drops model errors, paths, and raw descriptions from injected text.
//! - Supports quiet fallback on limit violations or malformed content.

use super::{Decision, OutputDocument, OutputKind};
use crate::adapter::additional_context_allowed;
use crate::limits::HOOK_ADDITIONAL_CONTEXT_SCALARS;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const HOOK_EVENT_NAME: &str = "UserPromptSubmit";
pub const MAX_ADDITIONAL_CONTEXT_CHARS: usize = HOOK_ADDITIONAL_CONTEXT_SCALARS.max();

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeHookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeHookEnvelope {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: ClaudeHookSpecificOutput,
}

impl ClaudeHookEnvelope {
    pub fn new(additional_context: String) -> Self {
        Self {
            hook_specific_output: ClaudeHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_string(),
                additional_context,
            },
        }
    }

    pub fn to_json(&self) -> Result<String, HookRenderError> {
        serde_json::to_string(self).map_err(|e| HookRenderError::Serialization(e.to_string()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookRenderError {
    OutputLimitExceeded,
    UnsafeControlText,
    EmptySuggestion,
    Serialization(String),
}

impl fmt::Display for HookRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLimitExceeded => write!(
                f,
                "hook additionalContext exceeds {MAX_ADDITIONAL_CONTEXT_CHARS} characters"
            ),
            Self::UnsafeControlText => write!(f, "hook text contains forbidden control characters"),
            Self::EmptySuggestion => write!(f, "no valid suggestion to render"),
            Self::Serialization(msg) => write!(f, "serialization error: {msg}"),
        }
    }
}

impl std::error::Error for HookRenderError {}

/// Sanitize invocation name: verify no control characters.
fn validate_clean_name(name: &str) -> Result<&str, HookRenderError> {
    if name.is_empty() {
        return Err(HookRenderError::EmptySuggestion);
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(HookRenderError::UnsafeControlText);
    }
    Ok(name)
}

/// Renders a Claude hook advisory envelope from an OutputDocument.
///
/// Returns `Ok(Some(envelope))` when advice is eligible and successfully formatted.
/// Returns `Ok(None)` for decisions that are quiet by default (e.g. normal abstentions, unavailable errors).
/// Returns `Err(HookRenderError)` on limit violations or unsafe text.
pub fn render_claude_hook_advice(
    doc: &OutputDocument,
    abstention_message_enabled: bool,
) -> Result<Option<ClaudeHookEnvelope>, HookRenderError> {
    match doc.kind() {
        OutputKind::Decision(Decision::Ranked) => {
            let empty = Vec::new();
            let skills = doc.as_value()["skills"].as_array().unwrap_or(&empty);
            if skills.is_empty() {
                return Ok(None);
            }
            // Suggest at most one skill
            let top_skill = &skills[0];
            let raw_name = top_skill["invocation_name"]
                .as_str()
                .or_else(|| top_skill["name"].as_str())
                .unwrap_or("");
            let clean_name = validate_clean_name(raw_name)?;

            let text = format!(
                "Suggested skill for the next step: {clean_name}. Use it only if it fits the user's request and current instructions."
            );

            additional_context_allowed(&text, 1).map_err(|e| match e {
                crate::adapter::AdapterError::UnsafeHookText => HookRenderError::UnsafeControlText,
                _ => HookRenderError::OutputLimitExceeded,
            })?;

            Ok(Some(ClaudeHookEnvelope::new(text)))
        }
        OutputKind::Decision(Decision::Explicit) => {
            let empty = Vec::new();
            let skills = doc.as_value()["skills"].as_array().unwrap_or(&empty);
            if skills.is_empty() {
                return Ok(None);
            }

            let mut names = Vec::new();
            for s in skills {
                let raw_name = s["invocation_name"]
                    .as_str()
                    .or_else(|| s["name"].as_str())
                    .unwrap_or("");
                let clean_name = validate_clean_name(raw_name)?;
                names.push(clean_name);
            }

            let text = if names.len() == 1 {
                format!(
                    "User requested skill: {}. Follow explicit skill requests and applicable instructions.",
                    names[0]
                )
            } else {
                format!(
                    "User requested skills: {}. Follow explicit skill requests and applicable instructions.",
                    names.join(", ")
                )
            };

            // Bounds check
            HOOK_ADDITIONAL_CONTEXT_SCALARS
                .check_unicode_scalars(&text)
                .map_err(|_| HookRenderError::OutputLimitExceeded)?;

            if text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
            {
                return Err(HookRenderError::UnsafeControlText);
            }

            Ok(Some(ClaudeHookEnvelope::new(text)))
        }
        OutputKind::Decision(Decision::Abstain) => {
            if abstention_message_enabled {
                let text = "No additional skill is suggested for this step; follow explicit skill requests and applicable instructions.".to_string();
                HOOK_ADDITIONAL_CONTEXT_SCALARS
                    .check_unicode_scalars(&text)
                    .map_err(|_| HookRenderError::OutputLimitExceeded)?;
                Ok(Some(ClaudeHookEnvelope::new(text)))
            } else {
                Ok(None)
            }
        }
        OutputKind::Decision(Decision::Unavailable) | OutputKind::Artifact(_) => Ok(None),
    }
}
