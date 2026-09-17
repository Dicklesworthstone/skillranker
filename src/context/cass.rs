//! Optional, exact-source cass archive access. Never used by the native hook.
//!
//! All subprocesses use the existing owned, bounded runner with an empty
//! environment. A cass export is private native data, not our normalized input
//! envelope, not proof of skill loading, and never a provider-ready payload.

use crate::adapter::{CassProducer, decode_json};
use crate::context::PrivateText;
use crate::context::source::SourcePolicy;
use crate::identity::{AdapterVersion, ContentHash, SourceId, SourceProvenance};
use crate::runtime::EntryClock;
use crate::subprocess::{self, ChildRequest, SubprocessError, TrustedExecutable};
use asupersync::Cx;
use serde_json::Value;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub const QUALIFIED_VERSION: &str = "0.8.0";
pub const CAPABILITIES_BYTES: usize = 256 * 1024;
pub const EXPORT_BYTES: usize = crate::limits::CASS_STDOUT_BYTES.max();
pub const MAX_MESSAGES: usize = 10_000;
pub const MAX_LISTING: usize = 1_000;
const STDERR_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CassError {
    UnsupportedSourceMode,
    InvalidRequest,
    UnsupportedProducer,
    RemoteSource,
    InvalidResponse,
    LimitExceeded,
    ChildFailed,
    Process(SubprocessError),
}
impl fmt::Display for CassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedSourceMode => "cass is unavailable in this source mode; supply a direct transcript or normalized input",
            Self::InvalidRequest => "invalid trusted cass request",
            Self::UnsupportedProducer => "cass version or capabilities are not qualified",
            Self::RemoteSource => "remote cass source is not permitted",
            Self::InvalidResponse => "invalid cass response",
            Self::LimitExceeded => "cass response exceeds input limits",
            Self::ChildFailed => "cass command failed",
            Self::Process(_) => "bounded cass subprocess failed",
        })
    }
}
impl std::error::Error for CassError {}
impl From<SubprocessError> for CassError {
    fn from(error: SubprocessError) -> Self {
        if error == SubprocessError::OutputLimit {
            Self::LimitExceeded
        } else {
            Self::Process(error)
        }
    }
}

/// Paths/executable are supplied by trusted local configuration, never by the
/// export payload. An explicit database prevents ambient cass DB selection.
pub struct CassConfig {
    pub executable: TrustedExecutable,
    pub directory: PathBuf,
    pub database: PathBuf,
    /// Digest of the operator-qualified executable bytes (not a version guess).
    pub binary_digest: ContentHash,
}
impl fmt::Debug for CassConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CassConfig(<private>)")
    }
}

pub struct CassAdapter {
    config: CassConfig,
    producer: CassProducer,
}
impl fmt::Debug for CassAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CassAdapter")
            .field("producer", &self.producer)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ArchiveSelection {
    path: PathBuf,
    source: SourceId,
}
impl fmt::Debug for ArchiveSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveSelection(<private>)")
    }
}
impl ArchiveSelection {
    /// v1 permits only cass's documented local source. Other source identities
    /// must not silently inherit local-file or native-adapter authority.
    pub fn local(path: PathBuf, source: SourceId) -> Result<Self, CassError> {
        if source.as_str() != "local" {
            return Err(CassError::RemoteSource);
        }
        validate_path(&path)?;
        Ok(Self { path, source })
    }
    pub fn source(&self) -> &SourceId {
        &self.source
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub struct CassRecord(Value);
impl CassRecord {
    pub fn native_value(&self) -> &Value {
        &self.0
    }
}
impl fmt::Debug for CassRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CassRecord(<private>)")
    }
}
#[derive(Debug)]
pub struct ArchiveExport {
    pub producer: CassProducer,
    pub records: Vec<CassRecord>,
    pub tool_content_included: bool,
    /// cass strips skill injections by default; absence is not not-loaded proof.
    pub skill_load_evidence_unknown: bool,
}
impl ArchiveExport {
    pub fn provenance(&self) -> SourceProvenance {
        SourceProvenance::Cass {
            source: self.producer.source_id.clone(),
            version: self.producer.version.clone(),
        }
    }
}
#[derive(Debug)]
pub struct ArchiveSession {
    pub selection: ArchiveSelection,
    pub workspace: PrivateText,
    pub harness: PrivateText,
}
#[derive(Debug)]
pub struct ArchiveListing {
    pub sessions: Vec<ArchiveSession>,
    /// cass sessions has no total/count cursor contract; a full page is incomplete.
    pub complete: bool,
    pub rejected_remote: usize,
    pub rejected_workspace: usize,
}

impl CassAdapter {
    pub async fn probe(
        config: CassConfig,
        policy: SourcePolicy,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<Self, CassError> {
        allowed(policy)?;
        validate_path(&config.directory)?;
        validate_path(&config.database)?;
        let bytes = command(
            &config,
            vec!["capabilities".into(), "--json".into()],
            CAPABILITIES_BYTES,
            cx,
            clock,
        )
        .await?;
        let producer = validate_capabilities(&bytes, config.binary_digest.clone())?;
        check_budget(cx, clock)?;
        Ok(Self { config, producer })
    }
    pub fn producer(&self) -> &CassProducer {
        &self.producer
    }

    pub async fn export(
        &self,
        selected: &ArchiveSelection,
        include_tools: bool,
        policy: SourcePolicy,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<ArchiveExport, CassError> {
        allowed(policy)?;
        let bytes = command(
            &self.config,
            vec![
                "export".into(),
                selected.path.as_os_str().to_owned(),
                "--source".into(),
                selected.source.as_str().into(),
                "--format".into(),
                "json".into(),
                "--include-tools".into(),
            ],
            EXPORT_BYTES,
            cx,
            clock,
        )
        .await?;
        let mut producer = self.producer.clone();
        producer.source_id = Some(selected.source.clone());
        let result = decode_export(&bytes, producer, include_tools)?;
        check_budget(cx, clock)?;
        Ok(result)
    }

    pub async fn sessions(
        &self,
        workspace: &Path,
        limit: usize,
        policy: SourcePolicy,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<ArchiveListing, CassError> {
        allowed(policy)?;
        validate_path(workspace)?;
        if limit == 0 || limit > MAX_LISTING {
            return Err(CassError::InvalidRequest);
        }
        let bytes = command(
            &self.config,
            vec![
                "sessions".into(),
                "--workspace".into(),
                workspace.as_os_str().to_owned(),
                "--limit".into(),
                limit.to_string().into(),
                "--json".into(),
            ],
            EXPORT_BYTES,
            cx,
            clock,
        )
        .await?;
        let result = decode_listing(&bytes, workspace, limit)?;
        check_budget(cx, clock)?;
        Ok(result)
    }
}

fn check_budget(cx: &Cx, clock: &EntryClock) -> Result<(), CassError> {
    if cx.is_cancel_requested() {
        return Err(CassError::Process(SubprocessError::Cancelled));
    }
    clock
        .admit_new_work()
        .map_err(|_| CassError::Process(SubprocessError::DeadlineExceeded))?;
    Ok(())
}
fn allowed(policy: SourcePolicy) -> Result<(), CassError> {
    if !policy.cass_allowed() || (policy.offline && policy.allow_network) {
        return Err(CassError::UnsupportedSourceMode);
    }
    Ok(())
}
fn validate_path(path: &Path) -> Result<(), CassError> {
    if !path.is_absolute()
        || path.as_os_str().len() > 16 * 1024
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(CassError::InvalidRequest);
    }
    Ok(())
}
async fn command(
    config: &CassConfig,
    args: Vec<OsString>,
    cap: usize,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<Vec<u8>, CassError> {
    let mut full = vec!["--db".into(), config.database.as_os_str().to_owned()];
    full.extend(args);
    let output = subprocess::run(
        cx,
        clock,
        ChildRequest {
            executable: config.executable.clone(),
            args: full,
            directory: config.directory.clone(),
            environment: vec![],
            stdin: vec![],
            stdout_limit: cap,
            stderr_limit: STDERR_BYTES,
        },
    )
    .await?;
    if !output.status.success() {
        return Err(CassError::ChildFailed);
    }
    Ok(output.stdout)
}

/// Pure parser also used with recorded producer captures. Successful parsing
/// alone is not live compatibility or executable-digest verification.
pub fn validate_capabilities(
    bytes: &[u8],
    binary_digest: ContentHash,
) -> Result<CassProducer, CassError> {
    let value = bounded_json(bytes, CAPABILITIES_BYTES)?;
    if value.get("crate_version").and_then(Value::as_str) != Some(QUALIFIED_VERSION)
        || value.get("api_version").and_then(Value::as_u64) != Some(1)
        || value.get("contract_version").and_then(Value::as_str) != Some("1")
    {
        return Err(CassError::UnsupportedProducer);
    }
    let features = value
        .get("features")
        .and_then(Value::as_array)
        .ok_or(CassError::UnsupportedProducer)?;
    if ![
        "json_output",
        "export_command",
        "self_describing_capabilities",
    ]
    .iter()
    .all(|required| features.iter().any(|f| f.as_str() == Some(required)))
    {
        return Err(CassError::UnsupportedProducer);
    }
    let commands = value
        .get("commands")
        .and_then(Value::as_array)
        .ok_or(CassError::UnsupportedProducer)?;
    for (name, args) in [
        ("export", &["path", "source", "format", "include-tools"][..]),
        ("sessions", &["workspace", "limit", "json"][..]),
    ] {
        let matches: Vec<_> = commands
            .iter()
            .filter(|c| c.get("name").and_then(Value::as_str) == Some(name))
            .collect();
        if matches.len() != 1 {
            return Err(CassError::UnsupportedProducer);
        }
        let actual = matches[0]
            .get("arguments")
            .and_then(Value::as_array)
            .ok_or(CassError::UnsupportedProducer)?;
        if !args.iter().all(|a| {
            actual
                .iter()
                .any(|v| v.get("name").and_then(Value::as_str) == Some(a))
        }) {
            return Err(CassError::UnsupportedProducer);
        }
    }
    let producer = CassProducer {
        version: AdapterVersion::parse(QUALIFIED_VERSION)
            .map_err(|_| CassError::UnsupportedProducer)?,
        api_version: 1,
        contract_version: 1,
        build_commit: value
            .get("build_commit")
            .and_then(Value::as_str)
            .map(str::to_owned),
        binary_digest: Some(binary_digest),
        source_id: None,
        remote_source: false,
        export_omits_skills_by_default: true,
        export_retains_native_shapes: true,
    };
    producer
        .validate_support_claim()
        .map_err(|_| CassError::UnsupportedProducer)?;
    Ok(producer)
}

pub fn decode_export(
    bytes: &[u8],
    producer: CassProducer,
    include_tools: bool,
) -> Result<ArchiveExport, CassError> {
    producer
        .validate_support_claim()
        .map_err(|_| CassError::UnsupportedProducer)?;
    if producer.version.as_str() != QUALIFIED_VERSION
        || producer.api_version != 1
        || producer.contract_version != 1
    {
        return Err(CassError::UnsupportedProducer);
    }
    if producer.source_id.as_ref().map(SourceId::as_str) != Some("local") {
        return Err(CassError::RemoteSource);
    }
    let value = bounded_json(bytes, EXPORT_BYTES)?;
    let array = value.as_array().ok_or(CassError::InvalidResponse)?;
    if array.len() > MAX_MESSAGES {
        return Err(CassError::LimitExceeded);
    }
    let mut records = Vec::with_capacity(array.len());
    for record in array {
        if !record.is_object() {
            return Err(CassError::InvalidResponse);
        }
        if include_tools {
            records.push(CassRecord(record.clone()));
        } else if let Some(record) = text_only(record)? {
            records.push(CassRecord(record));
        }
    }
    Ok(ArchiveExport {
        producer,
        records,
        tool_content_included: include_tools,
        skill_load_evidence_unknown: true,
    })
}

/// Project only known text fields; do not trust cass's include/no-tools flags.
/// Retain recognized native envelopes and event-link metadata while dropping
/// tool blocks and all unknown payload fields. Unsupported shapes fail closed.
fn text_only(record: &Value) -> Result<Option<Value>, CassError> {
    let (message, envelope) = if record.get("message").is_some_and(Value::is_object) {
        (&record["message"], Some("message"))
    } else if record.get("payload").is_some_and(Value::is_object) {
        (&record["payload"], Some("payload"))
    } else {
        (record, None)
    };
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| record.get("type").and_then(Value::as_str))
        .ok_or(CassError::InvalidResponse)?;
    if matches!(role, "tool" | "function") {
        return Ok(None);
    }
    if !matches!(role, "user" | "assistant" | "system" | "developer") {
        return Err(CassError::InvalidResponse);
    }
    let content = match message.get("content") {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut text = Vec::new();
            for b in blocks {
                if matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                ) {
                    let value = b
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or(CassError::InvalidResponse)?;
                    text.push(serde_json::json!({"type": b["type"], "text": value}));
                }
            }
            if text.is_empty() {
                return Ok(None);
            }
            Value::Array(text)
        }
        _ => return Err(CassError::InvalidResponse),
    };
    let mut projected = serde_json::Map::new();
    for key in [
        "uuid",
        "parentUuid",
        "event_id",
        "parent_id",
        "timestamp",
        "type",
    ] {
        if let Some(value) = record.get(key) {
            if !value.is_string() && !value.is_null() {
                return Err(CassError::InvalidResponse);
            }
            projected.insert(key.to_owned(), value.clone());
        }
    }
    let fields = serde_json::json!({"role": role, "content": content});
    if let Some(key) = envelope {
        projected.insert(key.to_owned(), fields);
    } else {
        projected.extend(
            fields
                .as_object()
                .ok_or(CassError::InvalidResponse)?
                .clone(),
        );
    }
    Ok(Some(Value::Object(projected)))
}

pub fn decode_listing(
    bytes: &[u8],
    workspace: &Path,
    limit: usize,
) -> Result<ArchiveListing, CassError> {
    validate_path(workspace)?;
    if limit == 0 || limit > MAX_LISTING {
        return Err(CassError::InvalidRequest);
    }
    let value = bounded_json(bytes, EXPORT_BYTES)?;
    let array = value
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or(CassError::InvalidResponse)?;
    if array.len() > limit {
        return Err(CassError::LimitExceeded);
    }
    let mut sessions = Vec::new();
    let mut remote = 0;
    let mut other_workspace = 0;
    for row in array {
        let source = row
            .get("source_id")
            .and_then(Value::as_str)
            .ok_or(CassError::InvalidResponse)?;
        if source != "local" || row.get("origin_host").is_some_and(|v| !v.is_null()) {
            remote += 1;
            continue;
        }
        let found = row
            .get("workspace")
            .and_then(Value::as_str)
            .ok_or(CassError::InvalidResponse)?;
        if Path::new(found) != workspace {
            other_workspace += 1;
            continue;
        }
        let path = row
            .get("path")
            .and_then(Value::as_str)
            .ok_or(CassError::InvalidResponse)?;
        let agent = row
            .get("agent")
            .and_then(Value::as_str)
            .ok_or(CassError::InvalidResponse)?;
        let selection = ArchiveSelection::local(
            PathBuf::from(path),
            SourceId::parse(source).map_err(|_| CassError::InvalidResponse)?,
        )?;
        sessions.push(ArchiveSession {
            selection,
            workspace: PrivateText::new(found),
            harness: PrivateText::new(agent),
        });
    }
    Ok(ArchiveListing {
        sessions,
        complete: array.len() < limit && remote == 0 && other_workspace == 0,
        rejected_remote: remote,
        rejected_workspace: other_workspace,
    })
}
fn bounded_json(bytes: &[u8], limit: usize) -> Result<Value, CassError> {
    if bytes.len() > limit {
        return Err(CassError::LimitExceeded);
    }
    decode_json(bytes, limit).map_err(|_| CassError::InvalidResponse)
}
