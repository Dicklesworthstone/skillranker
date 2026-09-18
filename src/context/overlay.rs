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

use crate::adapter::{ClaudeUserPromptSubmit, decode_json};
use crate::authorized_read::{AuthorizedRoot, AuthorizedRoots, FileKind, ReadError};
use crate::context::branch::{ActiveBranch, BranchResolutionTarget, resolve_active_branch};
use crate::context::jsonl::{SkipKind, parse_line};
use crate::context::{CurrentRequest, EventKind, NormalizedEvent, Role};
use crate::identity::{BranchId, SessionId};
use crate::limits::{
    NATIVE_TRANSCRIPT_TAIL_BYTES, NATIVE_TRANSCRIPT_TAIL_RECORDS, ONE_TRANSCRIPT_RECORD_BYTES,
};
use crate::output::ContextQuality;
use crate::runtime::EntryClock;
use std::collections::BTreeSet;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
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
    AmbiguousBranch,
    Deadline,
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHookEvent(name) => write!(f, "unsupported hook event: {name}"),
            Self::MissingPrompt => f.write_str("hook stdin payload is missing prompt text"),
            Self::TranscriptPathForbidden(reason) => {
                write!(f, "transcript path is forbidden: {reason}")
            }
            Self::TranscriptIsDirectory(_) => f.write_str("transcript path is a directory"),
            Self::TranscriptIsDeviceOrFifo(_) => f.write_str("transcript path is a device or FIFO"),
            Self::MalformedTranscript(err) => {
                write!(f, "existing transcript file is malformed: {err}")
            }
            Self::SessionMismatch { .. } => {
                f.write_str("transcript session does not match hook session")
            }
            Self::AmbiguousBranch => f.write_str("transcript branch cannot be resolved"),
            Self::Deadline => f.write_str("transcript overlay deadline exhausted"),
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
    let clock = EntryClock::capture().map_err(|_| OverlayError::Deadline)?;
    apply_claude_prompt_overlay_before(request, &clock)
}

/// Shared-deadline variant for invocation orchestration. Filesystem syscalls
/// remain cooperative; no late result is returned as timely completion.
pub fn apply_claude_prompt_overlay_before(
    request: &ClaudeOverlayRequest,
    clock: &EntryClock,
) -> Result<ClaudeOverlayResult, OverlayError> {
    checkpoint(clock)?;
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

    let opened = open_transcript(&raw_path, request.authorized_root.as_deref())?;
    checkpoint(clock)?;
    if opened.is_none() {
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

    // Read only a fixed-length tail from the descriptor whose authority/type
    // was checked. Never reopen a path after checking it.
    let mut file = opened.ok_or_else(|| malformed("missing opened transcript"))?;
    let before = file
        .metadata()
        .map_err(|_| malformed("cannot inspect transcript"))?;
    let length = before.len();
    let start = length.saturating_sub(NATIVE_TRANSCRIPT_TAIL_BYTES.max() as u64);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| malformed("cannot seek transcript"))?;
    let mut file_bytes = vec![0; (length - start) as usize];
    file.read_exact(&mut file_bytes)
        .map_err(|_| malformed("transcript changed during read"))?;
    let after = file
        .metadata()
        .map_err(|_| malformed("cannot inspect transcript"))?;
    if after.len() < length
        || (after.len() == length && before.modified().ok() != after.modified().ok())
    {
        return Err(malformed("transcript changed during read"));
    }
    checkpoint(clock)?;
    let mut partial = start > 0 || (!file_bytes.is_empty() && !file_bytes.ends_with(b"\n"));
    let aligned = if start == 0 {
        0
    } else {
        file_bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(file_bytes.len(), |p| p + 1)
    };
    let complete_end = file_bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(aligned, |p| p + 1);
    let complete = &file_bytes[aligned..complete_end.max(aligned)];
    let mut records = complete
        .rsplit(|b| *b == b'\n')
        .filter(|line| !line.is_empty());
    let mut lines = records
        .by_ref()
        .take(NATIVE_TRANSCRIPT_TAIL_RECORDS.max())
        .collect::<Vec<_>>();
    partial |= records.next().is_some();
    lines.reverse();
    let mut parsed_events = Vec::with_capacity(lines.len());
    let mut event_ids = BTreeSet::new();
    let mut native_lineage = false;
    for line in lines {
        checkpoint(clock)?;
        let event = parse_line(line).map_err(|kind| match kind {
            SkipKind::Corrupt => malformed("corrupt transcript JSON record"),
            SkipKind::DuplicateKey => malformed("duplicate key in transcript record"),
            SkipKind::Oversize => malformed("transcript record exceeds byte limit"),
        })?;
        let value = decode_json(line, ONE_TRANSCRIPT_RECORD_BYTES.max())
            .map_err(|_| malformed("corrupt, duplicate or oversized transcript record"))?;
        native_lineage |= value.get("parentUuid").is_some();
        for key in ["sessionId", "session_id"] {
            if let Some(observed) = value.get(key) {
                let observed = observed
                    .as_str()
                    .and_then(|v| SessionId::new(v).ok())
                    .ok_or_else(|| malformed("invalid transcript session identity"))?;
                if let Some(expected) = hook_session_id.as_ref() {
                    if expected != &observed {
                        return Err(OverlayError::SessionMismatch {
                            hook_session: expected.clone(),
                            transcript_session: observed,
                        });
                    }
                } else {
                    return Err(OverlayError::CrossSessionReadForbidden);
                }
            }
        }
        if let Some(id) = &event.event_id
            && !event_ids.insert(id.clone())
        {
            return Err(malformed("duplicate transcript event identity"));
        }
        parsed_events.push(event);
    }

    let had_parent_links =
        native_lineage || parsed_events.iter().any(|event| event.parent_id.is_some());

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
        // Event identity cannot turn an assistant/tool record into a user turn.
        if parsed_events[idx].role != Role::User || parsed_events[idx].kind != EventKind::Message {
            return Err(malformed("prompt identity refers to a non-user message"));
        }
        // Prompt is already recorded in the transcript; overlay the authoritative prompt text
        parsed_events[idx].text = hook.prompt.clone();
        parsed_events[idx].role = Role::User;
        parsed_events[idx].kind = EventKind::Message;
    } else {
        // Prompt not yet in transcript.
        // Even if an earlier turn has identical prompt text, do NOT deduplicate by text:
        // repeated identical user messages are distinct turns!
        let parent_id = if had_parent_links {
            let branch = resolve_active_branch(
                &parsed_events,
                &BranchResolutionTarget {
                    target_event_id: None,
                    target_branch_id: None,
                    target_agent_id: None,
                },
            );
            branch
                .active_branch()
                .ok_or(OverlayError::AmbiguousBranch)?
                .leaf_event_id
                .clone()
        } else {
            // Legacy linear normalized records have no parent links. This
            // fallback never resolves an explicit fork by physical file order.
            parsed_events.iter().rev().find_map(|e| e.event_id.clone())
        };

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
    // A pending prompt without an event ID is not a node in the native DAG.
    // Resolve the historical lineage first, then append that prompt exactly once.
    let anonymous_prompt = if had_parent_links && branch_target.target_event_id.is_none() {
        parsed_events.pop()
    } else {
        None
    };
    let branch_res = resolve_active_branch(&parsed_events, &branch_target);
    let mut active_branch = branch_res.active_branch().cloned();
    if let (Some(branch), Some(prompt)) = (active_branch.as_mut(), anonymous_prompt) {
        branch.events.push(prompt);
        branch.leaf_event_id = None;
    }
    let branch_id = active_branch.as_ref().and_then(|b| b.branch_id.clone());
    if had_parent_links {
        let branch = active_branch
            .as_ref()
            .ok_or(OverlayError::AmbiguousBranch)?;
        partial |= branch.ancestor_chain_truncated;
        parsed_events = branch.events.clone();
    }
    checkpoint(clock)?;

    Ok(ClaudeOverlayResult {
        session_id: hook_session_id,
        branch_id,
        current_request,
        events: parsed_events,
        active_branch,
        context_quality: if partial {
            ContextQuality::Partial
        } else {
            ContextQuality::Complete
        },
        prompt_overlaid: true,
        deduplicated_by_event_id,
    })
}

fn checkpoint(clock: &EntryClock) -> Result<(), OverlayError> {
    clock
        .admit_new_work()
        .map(|_| ())
        .map_err(|_| OverlayError::Deadline)
}
fn malformed(detail: &str) -> OverlayError {
    OverlayError::MalformedTranscript(detail.to_owned())
}

fn open_transcript(
    path: &Path,
    root: Option<&Path>,
) -> Result<Option<std::fs::File>, OverlayError> {
    if !path.is_absolute() {
        return Err(OverlayError::TranscriptPathForbidden(
            "absolute path required".into(),
        ));
    }
    let root = root
        .or_else(|| path.parent())
        .ok_or(OverlayError::CrossSessionReadForbidden)?;
    let authority = AuthorizedRoot::open_absolute(root)
        .map_err(|_| OverlayError::TranscriptPathForbidden("authorized root unavailable".into()))?;
    match AuthorizedRoots::single(authority).open_absolute_file(path) {
        Ok(file) => Ok(Some(file)),
        Err(ReadError::NotFound) => Ok(None),
        Err(ReadError::NotRegularFile(FileKind::Directory)) => {
            Err(OverlayError::TranscriptIsDirectory(path.to_path_buf()))
        }
        Err(ReadError::NotRegularFile(_)) => {
            Err(OverlayError::TranscriptIsDeviceOrFifo(path.to_path_buf()))
        }
        Err(ReadError::EscapesAuthorizedRoots) => Err(OverlayError::CrossSessionReadForbidden),
        Err(_) => Err(OverlayError::TranscriptPathForbidden(
            "cannot open authorized transcript".into(),
        )),
    }
}
