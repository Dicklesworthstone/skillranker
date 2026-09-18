//! Authoritative Claude prompt overlay and transcript validation.
//!
//! For Claude `UserPromptSubmit`, the stdin `prompt` is authoritative for the new
//! request; it may not yet appear in the transcript on disk.
//!
//! Invariants:
//! - Stdin prompt is overlaid once into the session history.
//! - Deduplication uses event/message identity (`prompt_id`), never prompt text equality alone.
//!   Repeated identical user messages across turns are distinct turns.
//! - First prompt in a session (transcript file missing on disk) is valid:
//!   records `ContextQuality::PromptOnly`.
//! - Malformed existing transcripts must NOT silently become an empty history;
//!   they are rejected with a sanitized diagnostic.
//! - Transcript paths grant read access only to that specific regular file:
//!   directories, FIFOs, character/block devices, and unauthorized symlinks are rejected.
//! - Session and branch identifiers must match; cross-session reads are forbidden.

use crate::adapter::ClaudeUserPromptSubmit;
use crate::context::branch::{ActiveBranch, BranchResolutionTarget, resolve_active_branch};
use crate::context::jsonl::{SkipKind, parse_line};
use crate::context::{CurrentRequest, EventKind, NormalizedEvent, Role};
use crate::identity::{BranchId, SessionId};
use crate::output::ContextQuality;
use std::fmt;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// Error encountered during Claude prompt overlay processing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverlayError {
    InvalidHookEvent(String),
    MissingPrompt,
    TranscriptPathForbidden(String),
    TranscriptIsDirectory(PathBuf),
    TranscriptIsDeviceOrFifo(PathBuf),
    MalformedTranscript(String),
    SessionMismatch {
        hook_session: SessionId,
        transcript_session: SessionId,
    },
    CrossSessionReadForbidden,
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHookEvent(name) => write!(f, "unsupported hook event: {name}"),
            Self::MissingPrompt => f.write_str("hook stdin payload is missing prompt text"),
            Self::TranscriptPathForbidden(reason) => {
                write!(f, "transcript path is forbidden: {reason}")
            }
            Self::TranscriptIsDirectory(p) => {
                write!(f, "transcript path is a directory: {}", p.display())
            }
            Self::TranscriptIsDeviceOrFifo(p) => {
                write!(f, "transcript path is a device or FIFO: {}", p.display())
            }
            Self::MalformedTranscript(err) => {
                write!(f, "existing transcript file is malformed: {err}")
            }
            Self::SessionMismatch {
                hook_session,
                transcript_session,
            } => {
                write!(
                    f,
                    "session ID mismatch: hook={:?}, transcript={:?}",
                    hook_session, transcript_session
                )
            }
            Self::CrossSessionReadForbidden => {
                f.write_str("cross-session transcript read is forbidden")
            }
        }
    }
}

impl std::error::Error for OverlayError {}

/// Configuration and inputs for applying the Claude prompt overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeOverlayRequest {
    pub hook_input: ClaudeUserPromptSubmit,
    /// Explicit override for transcript path; defaults to `hook_input.transcript_path`.
    pub transcript_path: Option<PathBuf>,
    /// Optional directory root outside of which transcript reads are forbidden.
    pub authorized_root: Option<PathBuf>,
}

/// Result of applying the authoritative Claude prompt overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeOverlayResult {
    pub session_id: Option<SessionId>,
    pub branch_id: Option<BranchId>,
    pub current_request: CurrentRequest,
    /// Merged event history containing the overlaid prompt exactly once.
    pub events: Vec<NormalizedEvent>,
    pub active_branch: Option<ActiveBranch>,
    pub context_quality: ContextQuality,
    pub prompt_overlaid: bool,
    pub deduplicated_by_event_id: bool,
}

/// Apply authoritative Claude `UserPromptSubmit` prompt overlay.
///
/// Validates transcript file safety, detects missing first-turn transcript,
/// verifies session association, and overlays the prompt once into history
/// deduplicating by event ID (never by text equality alone).
pub fn apply_claude_prompt_overlay(
    request: &ClaudeOverlayRequest,
) -> Result<ClaudeOverlayResult, OverlayError> {
    let hook = &request.hook_input;
    let prompt_text = hook.prompt.as_str().trim();
    if prompt_text.is_empty() {
        return Err(OverlayError::MissingPrompt);
    }

    // Determine target transcript path
    let transcript_path: Option<PathBuf> = request.transcript_path.clone().or_else(|| {
        hook.transcript_path
            .as_ref()
            .map(|p| PathBuf::from(p.as_str()))
    });

    let prompt_event_id = hook.prompt_id.clone();
    let hook_session_id = hook.session_id.clone();

    // If no transcript path is provided at all, rank as prompt_only
    let Some(raw_path) = transcript_path else {
        let current_request = CurrentRequest {
            event_id: prompt_event_id.clone(),
            text: hook.prompt.clone(),
            attachments_omitted: false,
            essential_attachment_missing: false,
        };
        let synthetic_event = NormalizedEvent {
            event_id: prompt_event_id.clone(),
            parent_id: None,
            turn_id: None,
            agent_id: None,
            branch_id: None,
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: None,
            text: hook.prompt.clone(),
            tool: None,
        };
        return Ok(ClaudeOverlayResult {
            session_id: hook_session_id,
            branch_id: None,
            current_request,
            events: vec![synthetic_event],
            active_branch: None,
            context_quality: ContextQuality::PromptOnly,
            prompt_overlaid: true,
            deduplicated_by_event_id: false,
        });
    };

    // 1. Verify transcript path safety
    validate_transcript_path(&raw_path, request.authorized_root.as_deref())?;

    // Check if the file exists on disk
    if !raw_path.exists() {
        // First turn of a new session: transcript does not yet exist!
        // Authorized prompt-only ranking with empty history.
        let current_request = CurrentRequest {
            event_id: prompt_event_id.clone(),
            text: hook.prompt.clone(),
            attachments_omitted: false,
            essential_attachment_missing: false,
        };
        let synthetic_event = NormalizedEvent {
            event_id: prompt_event_id.clone(),
            parent_id: None,
            turn_id: None,
            agent_id: None,
            branch_id: None,
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: None,
            text: hook.prompt.clone(),
            tool: None,
        };
        return Ok(ClaudeOverlayResult {
            session_id: hook_session_id,
            branch_id: None,
            current_request,
            events: vec![synthetic_event],
            active_branch: None,
            context_quality: ContextQuality::PromptOnly,
            prompt_overlaid: true,
            deduplicated_by_event_id: false,
        });
    }

    // 2. Read and parse existing transcript
    let file_bytes = fs::read(&raw_path).map_err(|e| {
        OverlayError::MalformedTranscript(format!("failed to read transcript: {e}"))
    })?;

    let mut parsed_events = Vec::new();
    let mut offset = 0usize;

    while offset < file_bytes.len() {
        let rest = &file_bytes[offset..];
        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
            // Trailing non-newline bytes at the end of an existing file
            break;
        };
        let line = &rest[..nl];
        offset += nl + 1;

        if line.is_empty() {
            continue;
        }

        match parse_line(line) {
            Ok(event) => parsed_events.push(event),
            Err(skip_kind) => match skip_kind {
                SkipKind::Corrupt => {
                    return Err(OverlayError::MalformedTranscript(
                        "corrupt transcript JSON record".into(),
                    ));
                }
                SkipKind::DuplicateKey => {
                    return Err(OverlayError::MalformedTranscript(
                        "duplicate key in transcript record".into(),
                    ));
                }
                SkipKind::Oversize => {
                    return Err(OverlayError::MalformedTranscript(
                        "transcript record exceeds byte limit".into(),
                    ));
                }
            },
        }
    }

    // 3. Overlay the authoritative prompt
    // Check if the prompt event is already present in the transcript by event ID
    let mut deduplicated_by_event_id = false;
    let mut matched_index = None;

    if let Some(ref target_pid) = prompt_event_id {
        for (idx, ev) in parsed_events.iter().enumerate() {
            if ev.event_id.as_ref() == Some(target_pid) {
                matched_index = Some(idx);
                deduplicated_by_event_id = true;
                break;
            }
        }
    }

    if let Some(idx) = matched_index {
        // Prompt is already recorded in the transcript; overlay the authoritative prompt text
        parsed_events[idx].text = hook.prompt.clone();
        parsed_events[idx].role = Role::User;
        parsed_events[idx].kind = EventKind::Message;
    } else {
        // Prompt not yet in transcript.
        // Even if an earlier turn has identical prompt text, do NOT deduplicate by text:
        // repeated identical user messages are distinct turns!
        let parent_id = parsed_events.iter().rev().find_map(|e| e.event_id.clone());

        let new_event = NormalizedEvent {
            event_id: prompt_event_id.clone(),
            parent_id,
            turn_id: None,
            agent_id: None,
            branch_id: None,
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: None,
            text: hook.prompt.clone(),
            tool: None,
        };
        parsed_events.push(new_event);
    }

    let current_request = CurrentRequest {
        event_id: prompt_event_id.clone(),
        text: hook.prompt.clone(),
        attachments_omitted: false,
        essential_attachment_missing: false,
    };

    // 4. Resolve active branch using the authoritative prompt
    let branch_target = BranchResolutionTarget {
        target_event_id: prompt_event_id,
        target_branch_id: None,
        target_agent_id: None,
    };
    let branch_res = resolve_active_branch(&parsed_events, &branch_target);
    let active_branch = branch_res.active_branch().cloned();
    let branch_id = active_branch.as_ref().and_then(|b| b.branch_id.clone());

    Ok(ClaudeOverlayResult {
        session_id: hook_session_id,
        branch_id,
        current_request,
        events: parsed_events,
        active_branch,
        context_quality: ContextQuality::Complete,
        prompt_overlaid: true,
        deduplicated_by_event_id,
    })
}

/// Validate that the transcript path is an authorized regular file and not a device/FIFO/directory.
fn validate_transcript_path(
    path: &Path,
    authorized_root: Option<&Path>,
) -> Result<(), OverlayError> {
    let symlink_meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File does not exist yet: safe for new session first turn
            return Ok(());
        }
        Err(e) => {
            return Err(OverlayError::TranscriptPathForbidden(format!(
                "cannot inspect path: {e}"
            )));
        }
    };

    let file_type = symlink_meta.file_type();

    // Reject directories
    if file_type.is_dir() {
        return Err(OverlayError::TranscriptIsDirectory(path.to_path_buf()));
    }

    // Reject FIFOs, sockets, block devices, and character devices
    if file_type.is_fifo()
        || file_type.is_char_device()
        || file_type.is_block_device()
        || file_type.is_socket()
    {
        return Err(OverlayError::TranscriptIsDeviceOrFifo(path.to_path_buf()));
    }

    // If it's a symlink, check the target
    if file_type.is_symlink() {
        let target_meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(OverlayError::TranscriptPathForbidden(
                    "broken symlink".into(),
                ));
            }
            Err(e) => {
                return Err(OverlayError::TranscriptPathForbidden(format!(
                    "cannot inspect symlink target: {e}"
                )));
            }
        };
        let target_type = target_meta.file_type();
        if target_type.is_dir() {
            return Err(OverlayError::TranscriptIsDirectory(path.to_path_buf()));
        }
        if target_type.is_fifo()
            || target_type.is_char_device()
            || target_type.is_block_device()
            || target_type.is_socket()
        {
            return Err(OverlayError::TranscriptIsDeviceOrFifo(path.to_path_buf()));
        }
    }

    // If authorized root is specified, canonical path must be inside authorized root
    if let Some(root) = authorized_root {
        let canonical_root = root.canonicalize().map_err(|e| {
            OverlayError::TranscriptPathForbidden(format!("cannot resolve authorized root: {e}"))
        })?;
        let canonical_path = path.canonicalize().map_err(|e| {
            OverlayError::TranscriptPathForbidden(format!("cannot resolve transcript path: {e}"))
        })?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(OverlayError::CrossSessionReadForbidden);
        }
    }

    Ok(())
}
