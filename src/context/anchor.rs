//! Task anchor preservation, terse continuation antecedent recovery, and pre-windowing directive extraction.
//!
//! Enforces boundary `p3_task_anchors`:
//! - Extract full bounded local directives before privacy/window transformations.
//! - Retain latest usable instruction and event provenance for terse continuation ("continue", "go on", "proceed", etc.).
//! - Classify missing antecedent for terse continuation as `unavailable / missing-task-context` (not empty/conversational).
//! - Accept supplied task summary only with verifiable provenance; never invent a summary using an unspecified LLM.
//! - Preserve explicit directives and task anchors even when the originating turn falls outside the rendered 12-message cutoff.
//! - Reject oversized uninspectable requests rather than resolving only a prefix.
//! - Detect contradictory directives across turns as `conflicting-directives`.

use crate::context::{EventKind, NormalizedContext, NormalizedEvent, Role};
use crate::identity::EventId;
use crate::limits::HOOK_STDIN_BYTES;
use crate::privacy::redaction::{RedactionError, Redactor};
use crate::roster::explicit::{DirectiveKind, ParsedDirective, parse_prompt_directives};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

pub(crate) mod history;

/// Standard terse continuation keywords/phrases.
const TERSE_CONTINUATION_PHRASES: &[&str] = &[
    "continue",
    "go on",
    "go ahead",
    "proceed",
    "next",
    "next step",
    "more",
    "keep going",
    "carry on",
    "yes",
    "yep",
    "yeah",
    "ok",
    "okay",
    "sure",
    "do it",
    "run it",
    "apply",
    "try it",
    "resume",
];

/// Checks whether a prompt is a terse continuation lacking an independent substantive instruction.
pub fn is_terse_continuation(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }

    // Normalize: lowercase and strip surrounding punctuation
    let normalized = trimmed
        .to_lowercase()
        .trim_matches(['.', '!', '?', ',', ';', ':', ' ', '-', '_'])
        .to_string();

    if TERSE_CONTINUATION_PHRASES.contains(&normalized.as_str()) {
        return true;
    }

    // Clean all punctuation to whitespace for word matching
    let cleaned: String = normalized
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.is_empty() {
        return true;
    }

    // Allow up to 3 words if all constituent words or sub-phrases are terse continuation terms
    // e.g. "ok, next step", "ok, continue", "yes, please proceed"
    if words.len() <= 3 {
        let joined = words.join(" ");
        if TERSE_CONTINUATION_PHRASES.contains(&joined.as_str()) {
            return true;
        }

        let is_all_terse_words = words.iter().all(|w| {
            TERSE_CONTINUATION_PHRASES.contains(w)
                || matches!(
                    *w,
                    "step"
                        | "ahead"
                        | "on"
                        | "going"
                        | "onward"
                        | "please"
                        | "then"
                        | "now"
                        | "it"
                        | "all"
                )
        });
        if is_all_terse_words {
            return true;
        }
    }

    false
}

/// Kind of explicit directive preserved on a task anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorDirectiveKind {
    Require,
    Exclude,
}

/// An explicit directive preserved before windowing or scalar budget truncation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnchorDirective {
    pub target: String,
    pub kind: AnchorDirectiveKind,
}

impl From<ParsedDirective> for AnchorDirective {
    fn from(d: ParsedDirective) -> Self {
        Self {
            target: d.target,
            kind: match d.kind {
                DirectiveKind::Require => AnchorDirectiveKind::Require,
                DirectiveKind::Exclude => AnchorDirectiveKind::Exclude,
            },
        }
    }
}

/// Provenance of a recovered or supplied task anchor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnchorProvenance {
    /// Derived from the current user request (substantive, not terse).
    CurrentRequest,
    /// Recovered from an earlier user message in the active session history.
    HistoricalEvent { event_id: EventId },
    /// Explicitly supplied task summary with external provenance.
    SuppliedSummary { provenance: String },
}

/// A preserved task anchor representing the active user objective.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskAnchor {
    /// Bounded, redacted text of the substantive instruction.
    pub text: String,
    /// Event identity of the source instruction, if known.
    pub source_event_id: Option<EventId>,
    /// How the task anchor was derived.
    pub provenance: AnchorProvenance,
    /// Whether the current user request was a terse continuation.
    pub is_terse: bool,
    /// Explicit skill directives preserved before windowing or truncation.
    pub directives: Vec<AnchorDirective>,
}

/// Result of attempting to resolve an active task anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnchorResolution {
    /// An active task anchor was successfully established.
    Established(TaskAnchor),
    /// The request is a terse continuation, but no antecedent instruction exists in history.
    MissingTaskContext { reason: &'static str },
    /// Input request is too large to inspect safely.
    OversizedInput { bytes: usize, max: usize },
    /// Conflicting explicit directives detected across historical or current turns.
    ConflictingDirectives { detail: String },
}

impl AnchorResolution {
    pub fn is_established(&self) -> bool {
        matches!(self, Self::Established(_))
    }

    pub fn anchor(&self) -> Option<&TaskAnchor> {
        match self {
            Self::Established(a) => Some(a),
            _ => None,
        }
    }

    pub fn into_result(self) -> Result<TaskAnchor, AnchorError> {
        match self {
            Self::Established(a) => Ok(a),
            Self::MissingTaskContext { reason } => Err(AnchorError::MissingTaskContext(reason)),
            Self::OversizedInput { bytes, max } => Err(AnchorError::OversizedInput { bytes, max }),
            Self::ConflictingDirectives { detail } => {
                Err(AnchorError::ConflictingDirectives(detail))
            }
        }
    }
}

/// Errors occurring during task anchor resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnchorError {
    MissingTaskContext(&'static str),
    OversizedInput { bytes: usize, max: usize },
    ConflictingDirectives(String),
    Redaction(RedactionError),
}

impl fmt::Display for AnchorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTaskContext(r) => write!(f, "missing task context: {r}"),
            Self::OversizedInput { bytes, max } => {
                write!(f, "request input {bytes} bytes exceeds safe limit {max}")
            }
            Self::ConflictingDirectives(d) => write!(f, "conflicting directives: {d}"),
            Self::Redaction(e) => write!(f, "redaction error: {e}"),
        }
    }
}

impl std::error::Error for AnchorError {}

impl From<RedactionError> for AnchorError {
    fn from(err: RedactionError) -> Self {
        Self::Redaction(err)
    }
}

/// Resolves the active task anchor and preserves explicit directives before any windowing.
///
/// Invariants:
/// 1. If `current_request.text` exceeds `HOOK_STDIN_BYTES` (1 MiB), rejects immediately as `OversizedInput`.
/// 2. Selects active history before parsing directives; sibling turns never gain authority.
/// 3. Detects any contradictory require/exclude directives across history and the current prompt.
/// 4. If the current request is substantive (not terse), it becomes the anchor with `AnchorProvenance::CurrentRequest`.
/// 5. If the current request is a terse continuation ("continue", "proceed", etc.):
///    - If a supplied summary is provided with valid provenance, accepts it as `AnchorProvenance::SuppliedSummary`.
///    - Otherwise, scans history backwards for the most recent substantive user turn before any task boundary.
///    - If found, that event becomes the anchor with `AnchorProvenance::HistoricalEvent`.
///    - If NOT found, returns `AnchorResolution::MissingTaskContext`.
pub fn resolve_task_anchor(
    context: &NormalizedContext,
    supplied_anchor: Option<(&str, &str)>, // (summary_text, provenance)
    redactor: &Redactor,
) -> AnchorResolution {
    let req_raw = context.current_request.text.as_str();

    // 1. Guard against oversized uninspectable request
    if req_raw.len() > HOOK_STDIN_BYTES.max() {
        return AnchorResolution::OversizedInput {
            bytes: req_raw.len(),
            max: HOOK_STDIN_BYTES.max(),
        };
    }

    let (history, _) = match history::select(context) {
        Ok(history) => history,
        Err(reason) => return AnchorResolution::MissingTaskContext { reason },
    };

    // 2. Extract and check directives across active history before any windowing
    let mut all_directives: Vec<AnchorDirective> = Vec::new();
    let mut positive_skills: HashSet<String> = HashSet::new();
    let mut negative_skills: HashSet<String> = HashSet::new();

    let mut check_directive = |d: &ParsedDirective| -> Result<(), AnchorResolution> {
        let key = d.target.to_lowercase();
        match d.kind {
            DirectiveKind::Require => {
                if negative_skills.contains(&key) {
                    return Err(AnchorResolution::ConflictingDirectives {
                        detail: format!("skill '{}' is both required and excluded", d.target),
                    });
                }
                positive_skills.insert(key);
            }
            DirectiveKind::Exclude => {
                if positive_skills.contains(&key) {
                    return Err(AnchorResolution::ConflictingDirectives {
                        detail: format!("skill '{}' is both excluded and required", d.target),
                    });
                }
                negative_skills.insert(key);
            }
        }
        all_directives.push(AnchorDirective::from(d.clone()));
        Ok(())
    };

    // Scan history in chronological order
    for ev in &history {
        if ev.role == Role::User && ev.kind == EventKind::Message {
            let hist_directives = parse_prompt_directives(ev.text.as_str());
            for d in &hist_directives {
                if let Err(conflict) = check_directive(d) {
                    return conflict;
                }
            }
        }
    }

    // Add current request directives and check for conflicts
    let current_directives = parse_prompt_directives(req_raw);
    for d in &current_directives {
        if let Err(conflict) = check_directive(d) {
            return conflict;
        }
    }

    // 3. If current request is substantive (not terse), it serves as the active task anchor
    if !is_terse_continuation(req_raw) {
        let redacted = match redactor.redact_field(req_raw) {
            Ok(r) => r.into_string(),
            Err(_) => {
                return AnchorResolution::OversizedInput {
                    bytes: req_raw.len(),
                    max: HOOK_STDIN_BYTES.max(),
                };
            }
        };
        return AnchorResolution::Established(TaskAnchor {
            text: redacted,
            source_event_id: context.current_request.event_id.clone(),
            provenance: AnchorProvenance::CurrentRequest,
            is_terse: false,
            directives: all_directives,
        });
    }

    // 4. Current request is a terse continuation:
    // First, check for supplied summary with non-empty, verifiable provenance
    if let Some((summary, prov)) =
        supplied_anchor.filter(|(s, p)| !s.trim().is_empty() && !p.trim().is_empty())
    {
        let redacted = match redactor.redact_field(summary) {
            Ok(r) => r.into_string(),
            Err(_) => {
                return AnchorResolution::OversizedInput {
                    bytes: summary.len(),
                    max: HOOK_STDIN_BYTES.max(),
                };
            }
        };
        return AnchorResolution::Established(TaskAnchor {
            text: redacted,
            source_event_id: None,
            provenance: AnchorProvenance::SuppliedSummary {
                provenance: prov.to_string(),
            },
            is_terse: true,
            directives: all_directives,
        });
    }

    // 5. Look back through history for the most recent substantive user instruction
    let mut antecedent: Option<(&NormalizedEvent, &str)> = None;

    for ev in history.iter().rev() {
        // Task boundaries demarcate separate user workflows
        if ev.kind == EventKind::TaskBoundary {
            break;
        }

        if ev.role == Role::User && ev.kind == EventKind::Message {
            let txt = ev.text.as_str().trim();
            if !is_terse_continuation(txt) {
                antecedent = Some((ev, txt));
                break;
            }
        }
    }

    match antecedent {
        Some((ev, text)) => {
            let redacted = match redactor.redact_field(text) {
                Ok(r) => r.into_string(),
                Err(_) => {
                    return AnchorResolution::OversizedInput {
                        bytes: text.len(),
                        max: HOOK_STDIN_BYTES.max(),
                    };
                }
            };
            AnchorResolution::Established(TaskAnchor {
                text: redacted,
                source_event_id: ev.event_id.clone(),
                provenance: AnchorProvenance::HistoricalEvent {
                    event_id: ev
                        .event_id
                        .clone()
                        .unwrap_or_else(|| EventId::new("historical-unknown").unwrap()),
                },
                is_terse: true,
                directives: all_directives,
            })
        }
        None => AnchorResolution::MissingTaskContext {
            reason: "terse continuation lacks recoverable substantive antecedent instruction",
        },
    }
}
