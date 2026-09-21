//! Bounded native JSONL snapshots and cursor generations.
//!
//! Snapshot file length, process complete records, and defer an incomplete
//! final line. Replacement, truncation, and a changed parser version rebuild
//! bounded state instead of continuing from an invalid offset. The transcript
//! file is never rewritten.
//!
//! Ranking tails and observation deltas use distinct cursors and caps. Neither
//! cursor advances across unprocessed bytes. Branch resolution is a later
//! boundary.

use crate::adapter::{AdapterError, decode_json};
use crate::blocking::{BlockingLeafKind, run_blocking_leaf};
use crate::context::{EventKind, NormalizedEvent, PrivateText, Role, ToolEvent, ToolStatus};
use crate::identity::{AgentId, BranchId, EventId, IdentityError, ToolCallId, TurnId};
use crate::limits::{
    NATIVE_TRANSCRIPT_TAIL_BYTES, NATIVE_TRANSCRIPT_TAIL_RECORDS, OBSERVATION_DELTA_BYTES,
    ONE_TRANSCRIPT_RECORD_BYTES,
};
use crate::runtime::{ProcessInvocation, RuntimeError};
use asupersync::Cx;
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, SFlag, fstat};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// Identity validation and attribution changed: rebuild prior cursor state.
pub const PARSER_VERSION: u32 = 2;

/// How far an observation read rewinds behind its own watermark.
///
/// A pass that sees a tool invocation without its result records an attempted load, which is
/// the correct reading of that evidence. The result usually lands in the next records, so a
/// read that began exactly at the watermark would never see the invocation again and the
/// attempt could never be completed — the load would stay `attempted` for good, and adoption
/// would read low for precisely the skills an agent used. Rewinding a bounded window lets the
/// pair be re-read together; the observation write is idempotent on `source_event_key`, so
/// re-reading costs nothing but the parse.
///
/// This window IS the pending state: nothing is retained between passes, so the bound is also
/// the expiry. A pair separated by more than this many bytes is not re-paired and its row stays
/// `attempted`, which undercounts rather than invents. One transcript record is bounded at
/// 256 KiB, so this holds several maximal records, and it is charged against the same
/// OBSERVATION_DELTA_BYTES budget as the new bytes.
pub const OBSERVATION_REPAIR_OVERLAP_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorKind {
    Ranking,
    Observation,
}

impl CursorKind {
    pub fn byte_cap(self) -> u64 {
        match self {
            Self::Ranking => NATIVE_TRANSCRIPT_TAIL_BYTES.max() as u64,
            Self::Observation => OBSERVATION_DELTA_BYTES.max() as u64,
        }
    }

    pub fn record_cap(self) -> usize {
        NATIVE_TRANSCRIPT_TAIL_RECORDS.max()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    pub const fn new(dev: u64, ino: u64) -> Self {
        Self { dev, ino }
    }

    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlCursor {
    pub kind: CursorKind,
    pub identity: FileIdentity,
    pub generation: u64,
    pub byte_offset: u64,
    pub last_event_id: Option<EventId>,
    pub parser_version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipKind {
    Oversize,
    Corrupt,
    DuplicateKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SkippedRecord {
    pub byte_offset: u64,
    pub kind: SkipKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlSnapshot {
    pub cursor: JsonlCursor,
    pub events: Vec<NormalizedEvent>,
    pub incomplete_tail: bool,
    pub truncated_history: bool,
    pub missing_tool_counterpart: bool,
    pub unread_backlog: bool,
    pub rebuilt: bool,
    pub skipped: Vec<SkippedRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlError {
    UnsafePath,
    Io,
    Deadline,
    Cancelled,
}

impl std::fmt::Display for JsonlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnsafePath => "transcript path is not a regular file",
            Self::Io => "transcript could not be read",
            Self::Deadline => "transcript read reached the cleanup reserve",
            Self::Cancelled => "transcript read cancelled",
        })
    }
}

impl std::error::Error for JsonlError {}

impl From<RuntimeError> for JsonlError {
    fn from(err: RuntimeError) -> Self {
        match err {
            RuntimeError::Cancelled | RuntimeError::LateResultSuppressed => Self::Cancelled,
            RuntimeError::Deadline(_) | RuntimeError::StdinTimeout => Self::Deadline,
            _ => Self::Io,
        }
    }
}

/// Snapshot `path` at its current length and read complete records only.
pub fn snapshot_jsonl(
    invocation: &ProcessInvocation,
    cx: &Cx,
    path: &Path,
    previous: Option<&JsonlCursor>,
    kind: CursorKind,
) -> Result<JsonlSnapshot, JsonlError> {
    let path = path.to_path_buf();
    let previous = previous.cloned();
    let outcome = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Filesystem,
        false,
        move || read_snapshot(&path, previous.as_ref(), kind),
    )?;
    outcome.value
}

fn read_snapshot(
    path: &Path,
    previous: Option<&JsonlCursor>,
    kind: CursorKind,
) -> Result<JsonlSnapshot, JsonlError> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(JsonlError::UnsafePath);
    }
    let link_meta = fs::symlink_metadata(path).map_err(|_| JsonlError::Io)?;
    if link_meta.file_type().is_symlink() || !link_meta.file_type().is_file() {
        return Err(JsonlError::UnsafePath);
    }
    let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
    let fd = match open(path, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::ELOOP | Errno::EMLINK) => return Err(JsonlError::UnsafePath),
        Err(Errno::ENOENT | Errno::EACCES | Errno::EPERM) => return Err(JsonlError::Io),
        Err(_) => return Err(JsonlError::Io),
    };
    let stat = fstat(&fd).map_err(|_| JsonlError::Io)?;
    let bits = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
    if bits != SFlag::S_IFREG {
        return Err(JsonlError::UnsafePath);
    }
    if link_meta.dev() != stat.st_dev as u64 || link_meta.ino() != stat.st_ino as u64 {
        return Err(JsonlError::UnsafePath);
    }
    let identity = FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    };
    let snapshot_len = stat.st_size as u64;
    let cap = kind.byte_cap();
    let mut file = File::from(fd);

    let (start, rebuilt, align_to_record) = match previous {
        Some(cursor)
            if cursor.parser_version == PARSER_VERSION
                && cursor.identity == identity
                && cursor.kind == kind
                && snapshot_len >= cursor.byte_offset =>
        {
            match kind {
                // An observation read resumes at its watermark, minus a bounded rewind so a
                // tool invocation recorded by an earlier pass can be re-paired with a result
                // that has only just arrived (sr-jgez, and the mechanism sr-an94's upgrade
                // depends on). Starting mid-file means the first partial record is skipped,
                // which is what align_to_record does.
                CursorKind::Observation => {
                    let start = cursor
                        .byte_offset
                        .saturating_sub(OBSERVATION_REPAIR_OVERLAP_BYTES);
                    (start, false, start > 0)
                }
                CursorKind::Ranking => (cursor.byte_offset, false, false),
            }
        }
        previous => {
            let rebuilt = previous.is_some();
            match kind {
                CursorKind::Ranking => {
                    let start = snapshot_len.saturating_sub(snapshot_len.min(cap));
                    (start, rebuilt, start > 0)
                }
                CursorKind::Observation => (0, rebuilt, false),
            }
        }
    };

    let snapshot = read_window(
        &mut file,
        ReadWindow {
            identity,
            kind,
            snapshot_len,
            cap,
            start,
            align_to_record,
            generation: generation_after(previous, rebuilt),
            last_event_id: if rebuilt {
                None
            } else {
                previous.and_then(|c| c.last_event_id.clone())
            },
            rebuilt: rebuilt && previous.is_some(),
        },
    )?;

    // A resumed observation read has to prove the watermark still describes THIS transcript, and
    // it cannot do that by inode: the ledger persists an offset, a generation and the id of the
    // last complete record, not (dev, ino), so the caller can only supply the identity of the file
    // it just opened. Content is the available proof. The record the watermark names must be
    // inside the window -- the rewind above guarantees it is, for a file that was appended to --
    // and if it is absent the transcript was replaced, rewritten or compacted, so the watermark
    // describes bytes that no longer mean what they meant. Re-read from the beginning rather than
    // trust an offset into a different file, and mark the read rebuilt so the generation advances
    // (sr-jgez).
    if kind == CursorKind::Observation
        && !rebuilt
        && start > 0
        && let Some(cursor) = previous
        && let Some(expected) = cursor.last_event_id.as_ref()
        && !snapshot
            .events
            .iter()
            .any(|event| event.event_id.as_ref() == Some(expected))
    {
        return read_window(
            &mut file,
            ReadWindow {
                identity,
                kind,
                snapshot_len,
                cap,
                start: 0,
                align_to_record: false,
                generation: cursor.generation.saturating_add(1).max(1),
                last_event_id: None,
                rebuilt: true,
            },
        );
    }

    // The rewind must never cost forward progress. A window is bounded by records as well as by
    // bytes, so on a transcript of very small records the rewind can swallow the whole record
    // budget re-reading history and end at or before the watermark it started from — and then every
    // pass reads the same records for ever, which is the exact defect this resume exists to fix.
    // When that happens, re-read from the watermark itself with no rewind: the repair window is
    // worth having only while it is free, and progress is not negotiable.
    // Note the absence of a `start > 0` condition, which the first version of this guard had and
    // which made it useless in the very case it was written for: when the rewind saturates at the
    // beginning of the file, `start` IS zero, and that is precisely when the whole record budget
    // goes on history.
    if kind == CursorKind::Observation
        && !rebuilt
        && let Some(cursor) = previous
        && cursor.byte_offset > 0
        && snapshot.cursor.byte_offset <= cursor.byte_offset
    {
        return read_window(
            &mut file,
            ReadWindow {
                identity,
                kind,
                snapshot_len,
                cap,
                start: cursor.byte_offset,
                align_to_record: false,
                generation: generation_after(previous, false),
                last_event_id: cursor.last_event_id.clone(),
                rebuilt: false,
            },
        );
    }
    Ok(snapshot)
}

/// One bounded read of a window and the parse of what it held.
struct ReadWindow {
    identity: FileIdentity,
    kind: CursorKind,
    snapshot_len: u64,
    cap: u64,
    start: u64,
    align_to_record: bool,
    generation: u64,
    last_event_id: Option<EventId>,
    rebuilt: bool,
}

fn read_window(file: &mut File, window: ReadWindow) -> Result<JsonlSnapshot, JsonlError> {
    let available = window.snapshot_len.saturating_sub(window.start);
    let to_read = available.min(window.cap);
    let unread_backlog = available > to_read;
    if to_read > 0 {
        file.seek(SeekFrom::Start(window.start))
            .map_err(|_| JsonlError::Io)?;
    }
    let mut buf = vec![0_u8; to_read as usize];
    file.read_exact(&mut buf).map_err(|_| JsonlError::Io)?;

    parse_window(
        &buf,
        window.start,
        window.align_to_record,
        WindowContext {
            identity: window.identity,
            kind: window.kind,
            generation: window.generation,
            last_event_id: window.last_event_id,
            truncated_history: window.start > 0,
            unread_backlog,
            rebuilt: window.rebuilt,
        },
    )
}

fn generation_after(previous: Option<&JsonlCursor>, rebuilt: bool) -> u64 {
    match previous {
        Some(cursor) if rebuilt => cursor.generation.saturating_add(1).max(1),
        Some(cursor) => cursor.generation.max(1),
        None => 1,
    }
}

struct WindowContext {
    identity: FileIdentity,
    kind: CursorKind,
    generation: u64,
    last_event_id: Option<EventId>,
    truncated_history: bool,
    unread_backlog: bool,
    rebuilt: bool,
}

fn parse_window(
    buf: &[u8],
    start: u64,
    align_to_record: bool,
    context: WindowContext,
) -> Result<JsonlSnapshot, JsonlError> {
    let WindowContext {
        identity,
        kind,
        generation,
        mut last_event_id,
        truncated_history,
        unread_backlog,
        rebuilt,
    } = context;
    let record_cap = kind.record_cap();
    let mut offset = 0usize;
    if align_to_record {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => offset = i + 1,
            None => {
                return Ok(JsonlSnapshot {
                    cursor: JsonlCursor {
                        kind,
                        identity,
                        generation,
                        byte_offset: start,
                        last_event_id,
                        parser_version: PARSER_VERSION,
                    },
                    events: Vec::new(),
                    incomplete_tail: !buf.is_empty(),
                    truncated_history,
                    missing_tool_counterpart: false,
                    unread_backlog,
                    rebuilt,
                    skipped: Vec::new(),
                });
            }
        }
    }

    let mut events = Vec::new();
    let mut skipped = Vec::new();
    let mut complete_end = start + offset as u64;
    let mut invocations = std::collections::BTreeSet::new();
    let mut results = Vec::new();
    let mut hit_record_cap = false;

    while offset < buf.len() {
        let rest = &buf[offset..];
        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
            break;
        };
        let line = &rest[..nl];
        let record_offset = start + offset as u64;
        offset += nl + 1;
        if events.len() + skipped.len() >= record_cap {
            hit_record_cap = true;
            offset -= nl + 1;
            break;
        }
        match parse_line(line) {
            Ok(event) => {
                if let Some(tool) = event.tool.as_ref()
                    && let Some(id) = tool.call_id.as_ref()
                {
                    match event.kind {
                        EventKind::ToolInvocation => {
                            invocations.insert(id.as_str().to_owned());
                        }
                        EventKind::ToolResult => results.push(id.as_str().to_owned()),
                        _ => {}
                    }
                }
                if let Some(id) = event.event_id.clone() {
                    last_event_id = Some(id);
                }
                events.push(event);
                complete_end = start + offset as u64;
            }
            Err(kind) => {
                skipped.push(SkippedRecord {
                    byte_offset: record_offset,
                    kind,
                });
                complete_end = start + offset as u64;
            }
        }
    }

    let incomplete_tail = offset < buf.len() && !hit_record_cap;
    if hit_record_cap {
        complete_end = start + offset as u64;
    }
    let missing_tool_counterpart = results.iter().any(|id| !invocations.contains(id.as_str()));

    Ok(JsonlSnapshot {
        cursor: JsonlCursor {
            kind,
            identity,
            generation,
            byte_offset: complete_end,
            last_event_id,
            parser_version: PARSER_VERSION,
        },
        events,
        incomplete_tail,
        truncated_history,
        missing_tool_counterpart,
        unread_backlog: unread_backlog || hit_record_cap,
        rebuilt,
        skipped,
    })
}

pub fn parse_line(line: &[u8]) -> Result<NormalizedEvent, SkipKind> {
    if line.len() > ONE_TRANSCRIPT_RECORD_BYTES.max() {
        return Err(SkipKind::Oversize);
    }
    if line.is_empty() {
        return Err(SkipKind::Corrupt);
    }
    let value =
        decode_json(line, ONE_TRANSCRIPT_RECORD_BYTES.max()).map_err(|error| match error {
            AdapterError::DuplicateKey => SkipKind::DuplicateKey,
            _ => SkipKind::Corrupt,
        })?;
    event_from_value(&value).ok_or(SkipKind::Corrupt)
}

fn event_from_value(value: &Value) -> Option<NormalizedEvent> {
    let object = value.as_object()?;
    if object.contains_key("role") && object.contains_key("kind") {
        return serde_json::from_value(value.clone()).ok();
    }
    let event_id = native_identity(object, &["event_id", "uuid"], EventId::new)?;
    let parent_id = native_identity(object, &["parent_id", "parentUuid"], EventId::new)?;
    let turn_id = native_identity(object, &["turn_id"], TurnId::new)?;
    let agent_id = native_identity(object, &["agent_id"], AgentId::new)?;
    let branch_id = native_identity(object, &["branch_id"], BranchId::new)?;
    let native_type = string_field(object, &["type"]).unwrap_or("message");
    let (default_role, default_kind) = map_native_type(native_type);
    let timestamp_unix_ms = object.get("timestamp_unix_ms").and_then(Value::as_i64);

    if matches!(default_kind, EventKind::Compaction | EventKind::Resume) {
        let text = string_field(object, &["text"]).unwrap_or("").to_owned();
        return Some(NormalizedEvent {
            event_id,
            parent_id,
            turn_id,
            agent_id,
            branch_id,
            role: default_role,
            kind: default_kind,
            timestamp_unix_ms,
            text: PrivateText::new(text),
            tool: None,
        });
    }

    let parsed = parse_native_content(object, default_role, default_kind)?;
    // A user message the user did not submit is context, never the request.
    let role = if parsed.role == Role::User
        && parsed.kind == EventKind::Message
        && injected_user_record(object, &parsed.text)
    {
        Role::System
    } else {
        parsed.role
    };
    Some(NormalizedEvent {
        event_id,
        parent_id,
        turn_id,
        agent_id,
        branch_id,
        role,
        kind: parsed.kind,
        timestamp_unix_ms,
        text: PrivateText::new(parsed.text),
        tool: parsed.tool,
    })
}

/// A Claude `user` record that is not a prompt the user submitted:
/// - injected meta content, such as skill bodies, scheduled prompts and
///   caveats;
/// - a compaction summary;
/// - a message whose `promptSource` is `system`, or whose `origin.kind` is
///   not `human` (task notifications, auto-continuations, peers);
/// - an interrupt marker or local command output.
///
/// These are unverified harness conventions, so records without such marks
/// stay user messages, and the text markers apply only to records that carry
/// no submission provenance.
fn injected_user_record(object: &serde_json::Map<String, Value>, text: &str) -> bool {
    let flagged = |name: &str| object.get(name).and_then(Value::as_bool) == Some(true);
    let origin = object
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str);
    let source = object.get("promptSource").and_then(Value::as_str);
    if flagged("isMeta")
        || flagged("isCompactSummary")
        || source == Some("system")
        || origin.is_some_and(|kind| kind != "human")
    {
        return true;
    }
    // Explicit submission provenance outranks the text heuristics: a typed
    // prompt may itself quote an interrupt marker or command output.
    if source.is_some() || origin.is_some() {
        return false;
    }
    let text = text.trim_start();
    text.starts_with("[Request interrupted by user")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<local-command-stderr>")
}

fn map_native_type(native_type: &str) -> (Role, EventKind) {
    match native_type {
        "user" => (Role::User, EventKind::Message),
        "assistant" => (Role::Assistant, EventKind::Message),
        "tool_use" | "tool_invocation" => (Role::Tool, EventKind::ToolInvocation),
        "tool_result" => (Role::Tool, EventKind::ToolResult),
        "compaction" => (Role::System, EventKind::Compaction),
        "resume" => (Role::System, EventKind::Resume),
        "system" => (Role::System, EventKind::Message),
        _ => (Role::System, EventKind::Message),
    }
}

fn string_field<'a>(object: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

struct ParsedContent {
    role: Role,
    kind: EventKind,
    text: String,
    tool: Option<ToolEvent>,
}

fn parse_native_content(
    object: &serde_json::Map<String, Value>,
    default_role: Role,
    default_kind: EventKind,
) -> Option<ParsedContent> {
    // 1. Dedicated tool invocation or result record (top-level)
    if matches!(
        default_kind,
        EventKind::ToolInvocation | EventKind::ToolResult
    ) {
        let call_id = native_identity(object, &["call_id", "tool_use_id", "id"], ToolCallId::new)?;
        let call_id = call_id?;
        let name = string_field(object, &["name", "tool_name"]);
        let text = string_field(object, &["text"]).unwrap_or("").to_owned();
        if default_kind == EventKind::ToolInvocation {
            let name = name?;
            let arguments = if let Some(input) = object.get("input") {
                match input {
                    Value::String(s) => Some(PrivateText::new(s.clone())),
                    other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                }
            } else if let Some(args) = object.get("arguments") {
                match args {
                    Value::String(s) => Some(PrivateText::new(s.clone())),
                    other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                }
            } else {
                None
            };
            return Some(ParsedContent {
                role: default_role,
                kind: EventKind::ToolInvocation,
                text,
                tool: Some(ToolEvent {
                    call_id: Some(call_id),
                    name: PrivateText::new(name),
                    status: ToolStatus::Attempted,
                    arguments,
                    result: None,
                }),
            });
        } else {
            // ToolResult
            let name = name.unwrap_or("tool");
            let status = match object.get("is_error").and_then(Value::as_bool) {
                Some(true) => ToolStatus::Failed,
                Some(false) => ToolStatus::Succeeded,
                None => ToolStatus::Unknown,
            };
            let result = if let Some(content) = object.get("content") {
                match content {
                    Value::String(s) => Some(PrivateText::new(s.clone())),
                    other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                }
            } else if let Some(res) = object.get("result") {
                match res {
                    Value::String(s) => Some(PrivateText::new(s.clone())),
                    other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                }
            } else {
                None
            };
            return Some(ParsedContent {
                role: Role::Tool,
                kind: EventKind::ToolResult,
                text,
                tool: Some(ToolEvent {
                    call_id: Some(call_id),
                    name: PrivateText::new(name),
                    status,
                    arguments: None,
                    result,
                }),
            });
        }
    }

    // 2. Message records (Claude / Anthropic style)
    let (content_val, role_hint) = if let Some(msg) = object.get("message") {
        match msg {
            Value::String(_) => (Some(msg), None),
            Value::Object(m) => {
                let role_hint = match m.get("role").and_then(Value::as_str) {
                    Some("user") => Some(Role::User),
                    Some("assistant") => Some(Role::Assistant),
                    Some("system") => Some(Role::System),
                    Some("tool") => Some(Role::Tool),
                    _ => None,
                };
                if let Some(c) = m.get("content") {
                    (Some(c), role_hint)
                } else {
                    let t = m.get("text")?;
                    (Some(t), role_hint)
                }
            }
            _ => return None,
        }
    } else if let Some(text) = object.get("text") {
        (Some(text), None)
    } else {
        let content = object.get("content")?;
        (Some(content), None)
    };

    let role = role_hint.unwrap_or(default_role);

    match content_val? {
        Value::String(s) => {
            if s.is_empty() {
                return None;
            }
            Some(ParsedContent {
                role,
                kind: default_kind,
                text: s.clone(),
                tool: None,
            })
        }
        Value::Array(blocks) => parse_blocks(blocks, role, default_kind),
        _ => None,
    }
}

fn parse_blocks(
    blocks: &[Value],
    default_role: Role,
    default_kind: EventKind,
) -> Option<ParsedContent> {
    if blocks.is_empty() {
        return None;
    }
    let mut text_parts = Vec::new();
    let mut tool_event: Option<ToolEvent> = None;
    let mut kind = default_kind;
    let mut role = default_role;

    for block in blocks {
        let block_obj = block.as_object()?;
        let block_type = block_obj.get("type").and_then(Value::as_str)?;
        match block_type {
            "text" | "input_text" | "output_text" => {
                let part = block_obj.get("text").and_then(Value::as_str)?;
                text_parts.push(part.to_owned());
            }
            "tool_use" => {
                let name = block_obj.get("name").and_then(Value::as_str)?;
                let call_id = native_identity(
                    block_obj,
                    &["call_id", "tool_use_id", "id"],
                    ToolCallId::new,
                )?;
                let arguments = if let Some(input) = block_obj.get("input") {
                    match input {
                        Value::String(s) => Some(PrivateText::new(s.clone())),
                        other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                    }
                } else if let Some(args) = block_obj.get("arguments") {
                    match args {
                        Value::String(s) => Some(PrivateText::new(s.clone())),
                        other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                    }
                } else {
                    None
                };
                tool_event = Some(ToolEvent {
                    call_id,
                    name: PrivateText::new(name),
                    status: ToolStatus::Attempted,
                    arguments,
                    result: None,
                });
                kind = EventKind::ToolInvocation;
            }
            "tool_result" => {
                let call_id = native_identity(
                    block_obj,
                    &["call_id", "tool_use_id", "id"],
                    ToolCallId::new,
                )?;
                let status = match block_obj.get("is_error").and_then(Value::as_bool) {
                    Some(true) => ToolStatus::Failed,
                    Some(false) => ToolStatus::Succeeded,
                    None => ToolStatus::Unknown,
                };
                let result = if let Some(content) = block_obj.get("content") {
                    match content {
                        Value::String(s) => Some(PrivateText::new(s.clone())),
                        Value::Array(inner_blocks) => {
                            let mut inner_text = Vec::new();
                            for ib in inner_blocks {
                                if let Some(ibo) = ib.as_object()
                                    && let Some(t) = ibo.get("text").and_then(Value::as_str)
                                {
                                    inner_text.push(t);
                                }
                            }
                            Some(PrivateText::new(inner_text.join("\n")))
                        }
                        other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                    }
                } else if let Some(res) = block_obj.get("result") {
                    match res {
                        Value::String(s) => Some(PrivateText::new(s.clone())),
                        other => Some(PrivateText::new(serde_json::to_string(other).ok()?)),
                    }
                } else {
                    None
                };
                let name = string_field(block_obj, &["name", "tool_name"]).unwrap_or("tool");
                tool_event = Some(ToolEvent {
                    call_id,
                    name: PrivateText::new(name),
                    status,
                    arguments: None,
                    result,
                });
                kind = EventKind::ToolResult;
                role = Role::Tool;
            }
            "thinking" => {
                let _ = block_obj.get("thinking").and_then(Value::as_str)?;
            }
            "image" => {}
            _ => return None,
        }
    }

    if text_parts.is_empty() && tool_event.is_none() {
        return None;
    }

    Some(ParsedContent {
        role,
        kind,
        text: text_parts.join("\n"),
        tool: tool_event,
    })
}

/// Outer None rejects malformed/conflicting declarations; inner None is genuine
/// absence. Null is an explicit absence and cannot mask a conflicting alias.
fn native_identity<T: Eq>(
    object: &serde_json::Map<String, Value>,
    names: &[&str],
    construct: impl Fn(String) -> Result<T, IdentityError>,
) -> Option<Option<T>> {
    let mut declared: Option<Option<T>> = None;
    for name in names {
        if let Some(value) = object.get(*name) {
            let parsed = if value.is_null() {
                None
            } else {
                Some(construct(value.as_str()?.to_owned()).ok()?)
            };
            if declared
                .as_ref()
                .is_some_and(|previous| previous != &parsed)
            {
                return None;
            }
            declared = Some(parsed);
        }
    }
    Some(declared.flatten())
}
