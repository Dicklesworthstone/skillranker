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
use crate::identity::{EventId, ToolCallId, TurnId};
use crate::limits::{
    NATIVE_TRANSCRIPT_TAIL_BYTES, NATIVE_TRANSCRIPT_TAIL_RECORDS, OBSERVATION_DELTA_BYTES,
    ONE_TRANSCRIPT_RECORD_BYTES,
};
use crate::runtime::{ProcessInvocation, RuntimeError};
use asupersync::Cx;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub const PARSER_VERSION: u32 = 1;

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
    dev: u64,
    ino: u64,
}

impl FileIdentity {
    fn from_metadata(meta: &fs::Metadata) -> Self {
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
    let link_meta = fs::symlink_metadata(path).map_err(|_| JsonlError::Io)?;
    if link_meta.file_type().is_symlink() || !link_meta.file_type().is_file() {
        return Err(JsonlError::UnsafePath);
    }
    let mut file = File::open(path).map_err(|_| JsonlError::Io)?;
    let meta = file.metadata().map_err(|_| JsonlError::Io)?;
    if !meta.is_file() {
        return Err(JsonlError::UnsafePath);
    }
    let identity = FileIdentity::from_metadata(&meta);
    let snapshot_len = meta.len();
    let cap = kind.byte_cap();

    let (start, rebuilt, align_to_record) = match previous {
        Some(cursor)
            if cursor.parser_version == PARSER_VERSION
                && cursor.identity == identity
                && cursor.kind == kind
                && snapshot_len >= cursor.byte_offset =>
        {
            (cursor.byte_offset, false, false)
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

    let available = snapshot_len.saturating_sub(start);
    let to_read = available.min(cap);
    let unread_backlog = available > to_read;
    if to_read > 0 {
        file.seek(SeekFrom::Start(start))
            .map_err(|_| JsonlError::Io)?;
    }
    let mut buf = vec![0_u8; to_read as usize];
    file.read_exact(&mut buf).map_err(|_| JsonlError::Io)?;

    parse_window(
        &buf,
        start,
        align_to_record,
        WindowContext {
            identity,
            kind,
            generation: generation_after(previous, rebuilt),
            last_event_id: if rebuilt {
                None
            } else {
                previous.and_then(|c| c.last_event_id.clone())
            },
            truncated_history: start > 0,
            unread_backlog,
            rebuilt: rebuilt && previous.is_some(),
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
        if events.len() >= record_cap {
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

fn parse_line(line: &[u8]) -> Result<NormalizedEvent, SkipKind> {
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
    let event_id = string_field(object, &["event_id", "uuid"]).and_then(|v| EventId::new(v).ok());
    let parent_id =
        string_field(object, &["parent_id", "parentUuid"]).and_then(|v| EventId::new(v).ok());
    let turn_id = string_field(object, &["turn_id"]).and_then(|v| TurnId::new(v).ok());
    let native_type = string_field(object, &["type"]).unwrap_or("message");
    let (role, kind) = map_native_type(native_type);
    let text = native_text(object);
    let tool = native_tool(object, kind);
    let timestamp_unix_ms = object.get("timestamp_unix_ms").and_then(Value::as_i64);
    Some(NormalizedEvent {
        event_id,
        parent_id,
        turn_id,
        agent_id: None,
        branch_id: None,
        role,
        kind,
        timestamp_unix_ms,
        text: PrivateText::new(text),
        tool,
    })
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

fn native_text(object: &serde_json::Map<String, Value>) -> String {
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    match object.get("message") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(message)) => message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => String::new(),
    }
}

fn native_tool(object: &serde_json::Map<String, Value>, kind: EventKind) -> Option<ToolEvent> {
    if !matches!(kind, EventKind::ToolInvocation | EventKind::ToolResult) {
        return None;
    }
    let call_id =
        string_field(object, &["call_id", "tool_use_id"]).and_then(|v| ToolCallId::new(v).ok());
    let name = string_field(object, &["name", "tool_name"]).unwrap_or("tool");
    Some(ToolEvent {
        call_id,
        name: PrivateText::new(name),
        status: if kind == EventKind::ToolResult {
            ToolStatus::Unknown
        } else {
            ToolStatus::Attempted
        },
        arguments: None,
        result: None,
    })
}
