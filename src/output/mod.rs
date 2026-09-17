//! Versioned, bounded output contracts, not a ranking or publication authority.
//!
//! Imported decisions are inert data. Validation cannot prove current visibility,
//! user consent, provider execution, delivery, or quality. Unknown additive fields
//! survive round trips; unknown discriminants and versions are refused.

use crate::identity::{ContentHash, EventId, HarnessId, SkillId};
use crate::roster::InvocationName;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write};

pub const SCHEMA_VERSION: u64 = 1;
pub const MAX_OUTPUT_BYTES: usize = 2 * crate::limits::MIB;
pub const MAX_OUTPUT_DEPTH: usize = 64;
pub const MAX_WARNING_DETAILS: usize = 32;
pub const MAX_TRACE_PAGE_ITEMS: usize = 128;
pub const MAX_TRACE_ITEMS: u64 = 80_000;
const MAX_TEXT_BYTES: usize = 4096;
const SUM_TOLERANCE: f64 = 1e-4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Ranked,
    Explicit,
    Abstain,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextQuality {
    Complete,
    PromptOnly,
    Partial,
    Insufficient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Planning,
    Implementing,
    Debugging,
    Testing,
    Reviewing,
    Releasing,
    Conversing,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Demo,
    Replay,
    Report,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Complete,
    Partial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateStatus {
    Passed,
    Failed,
    NotEstablished,
    NotApplicable,
}

/// Each variant is a protocol category; process signals/broken pipes are separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CliExit {
    Success = 0,
    Usage = 2,
    Session = 3,
    Provider = 4,
    Roster = 5,
    Timeout = 6,
    Input = 7,
    Privacy = 8,
    Storage = 9,
    ProviderContract = 10,
    CacheMiss = 11,
}

macro_rules! error_kinds {
    ($($variant:ident => ($wire:literal, $exit:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum ErrorKind {
            $(#[serde(rename = $wire)] $variant),+
        }
        impl ErrorKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
            pub const fn exit_code(self) -> CliExit {
                match self { $(Self::$variant => CliExit::$exit),+ }
            }
        }
    };
}

error_kinds! {
    InvalidUsage => ("invalid-usage", Usage),
    InvalidConfiguration => ("invalid-configuration", Usage),
    MissingSession => ("missing-session", Session),
    AmbiguousSession => ("ambiguous-session", Session),
    Superseded => ("superseded", Session),
    ProviderFailure => ("provider-failure", Provider),
    Authentication => ("authentication", Provider),
    NetworkFailure => ("network-failure", Provider),
    RequestBudget => ("request-budget", Provider),
    ProviderCooldown => ("provider-cooldown", Provider),
    BudgetState => ("budget-state", Provider),
    EmptyRoster => ("empty-roster", Roster),
    UnusableRoster => ("unusable-roster", Roster),
    UnresolvedExplicit => ("unresolved-explicit", Roster),
    IncompleteRoster => ("incomplete-roster", Roster),
    RosterChanged => ("roster-changed", Roster),
    RetrievalEmpty => ("retrieval-empty", Roster),
    RetrievalFailure => ("retrieval-failure", Roster),
    Timeout => ("timeout", Timeout),
    MalformedInput => ("malformed-input", Input),
    OversizedInput => ("oversized-input", Input),
    UnsupportedInput => ("unsupported-input", Input),
    UnsupportedSourceMode => ("unsupported-source-mode", Input),
    InsufficientContext => ("insufficient-context", Input),
    OutputLimit => ("output-limit", Input),
    NetworkDenied => ("network-denied", Privacy),
    StorageFailure => ("storage-failure", Storage),
    InvalidProviderResponse => ("invalid-provider-response", ProviderContract),
    CacheMiss => ("cache-miss", CacheMiss),
}

/// Safe diagnostics never include rejected input, unknown keys, or parser text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractError {
    InvalidJson,
    UnsupportedVersion,
    InvalidField,
    InconsistentFields,
    LimitExceeded,
    SnapshotChanged,
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidJson => "invalid output JSON",
            Self::UnsupportedVersion => "unsupported output schema version",
            Self::InvalidField => "invalid or missing output field",
            Self::InconsistentFields => "inconsistent output fields",
            Self::LimitExceeded => "output contract limit exceeded",
            Self::SnapshotChanged => "output snapshot changed; restart pagination",
        })
    }
}

impl std::error::Error for ContractError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputKind {
    Decision(Decision),
    Artifact(ArtifactKind),
}

/// Immutable validated wire data. Additive fields are retained, never promoted
/// into permissions. No method publishes advice or returns an actionable target.
#[derive(Clone, PartialEq)]
pub struct OutputDocument {
    value: Value,
    kind: OutputKind,
    exit: CliExit,
}

impl fmt::Debug for OutputDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputDocument")
            .field("kind", &self.kind)
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

impl OutputDocument {
    /// Limits are enforced before parsing; all duplicate object keys are refused.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ContractError> {
        if bytes.len() > MAX_OUTPUT_BYTES {
            return Err(ContractError::LimitExceeded);
        }
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let value = JsonSeed(0)
            .deserialize(&mut decoder)
            .map_err(|_| ContractError::InvalidJson)?;
        decoder.end().map_err(|_| ContractError::InvalidJson)?;
        Self::from_value(value)
    }

    /// For locally constructed data. Wire decoders must use `from_json` so an
    /// earlier permissive parser cannot hide duplicate keys.
    pub fn from_value(value: Value) -> Result<Self, ContractError> {
        check_depth(&value, 0)?;
        let _ = encode(&value)?;
        let object = object(&value)?;
        version(object)?;
        if object.contains_key("hookSpecificOutput") {
            return Err(ContractError::InconsistentFields);
        }
        let (kind, exit) = if object.contains_key("kind") {
            let kind = enum_field(object, "kind")?;
            let exit = validate_artifact(object, kind)?;
            (OutputKind::Artifact(kind), exit)
        } else {
            let decision = enum_field(object, "decision")?;
            let exit = validate_decision(object, decision)?;
            (OutputKind::Decision(decision), exit)
        };
        Ok(Self { value, kind, exit })
    }

    pub const fn kind(&self) -> OutputKind {
        self.kind
    }
    pub const fn exit_code(&self) -> CliExit {
        self.exit
    }
    pub fn as_value(&self) -> &Value {
        &self.value
    }
    pub fn to_json(&self) -> Result<Vec<u8>, ContractError> {
        encode(&self.value)
    }

    /// Construct a minimal failure with fixed diagnostics, not raw provider text.
    /// Retryability is informational and grants no network/deadline permission.
    pub fn failure(kind: ErrorKind, retryable: bool) -> Self {
        Self {
            value: json!({
                "schema_version": SCHEMA_VERSION, "decision": "unavailable",
                "error": {
                    "code": kind.exit_code() as u8, "kind": kind.as_str(),
                    "message": "The requested operation is unavailable.",
                    "hint": "Inspect local readiness and the structured error kind.",
                    "retryable": retryable
                }
            }),
            kind: OutputKind::Decision(Decision::Unavailable),
            exit: kind.exit_code(),
        }
    }
}

fn field<'a>(m: &'a Map<String, Value>, key: &str) -> Result<&'a Value, ContractError> {
    m.get(key).ok_or(ContractError::InvalidField)
}
fn object(v: &Value) -> Result<&Map<String, Value>, ContractError> {
    v.as_object().ok_or(ContractError::InvalidField)
}
fn text(v: &Value) -> Result<&str, ContractError> {
    let text = v.as_str().ok_or(ContractError::InvalidField)?;
    if text.is_empty() || text.len() > MAX_TEXT_BYTES || text.chars().any(char::is_control) {
        return Err(ContractError::InvalidField);
    }
    Ok(text)
}
fn count(m: &Map<String, Value>, key: &str) -> Result<u64, ContractError> {
    field(m, key)?.as_u64().ok_or(ContractError::InvalidField)
}
fn boolean(m: &Map<String, Value>, key: &str) -> Result<bool, ContractError> {
    field(m, key)?.as_bool().ok_or(ContractError::InvalidField)
}
fn array<'a>(
    m: &'a Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<&'a [Value], ContractError> {
    let a = field(m, key)?
        .as_array()
        .ok_or(ContractError::InvalidField)?;
    if a.len() > max {
        return Err(ContractError::LimitExceeded);
    }
    Ok(a)
}
fn enum_field<T: serde::de::DeserializeOwned>(
    m: &Map<String, Value>,
    key: &str,
) -> Result<T, ContractError> {
    serde_json::from_value(field(m, key)?.clone()).map_err(|_| ContractError::InvalidField)
}
fn version(m: &Map<String, Value>) -> Result<(), ContractError> {
    if count(m, "schema_version")? != SCHEMA_VERSION {
        return Err(ContractError::UnsupportedVersion);
    }
    Ok(())
}
fn probability(v: &Value) -> Result<f64, ContractError> {
    let p = v.as_f64().ok_or(ContractError::InvalidField)?;
    if !p.is_finite() || !(0.0..=1.0).contains(&p) {
        return Err(ContractError::InvalidField);
    }
    Ok(p)
}
fn nullable_number(m: &Map<String, Value>, key: &str) -> Result<Option<f64>, ContractError> {
    let value = field(m, key)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .map(Some)
        .ok_or(ContractError::InvalidField)
}
fn nullable_probability(m: &Map<String, Value>, key: &str) -> Result<Option<f64>, ContractError> {
    let v = field(m, key)?;
    if v.is_null() {
        Ok(None)
    } else {
        probability(v).map(Some)
    }
}
fn digest(v: &Value) -> Result<ContentHash, ContractError> {
    ContentHash::parse(text(v)?).map_err(|_| ContractError::InvalidField)
}
fn identifier(v: &Value) -> Result<(), ContractError> {
    EventId::new(text(v)?)
        .map(|_| ())
        .map_err(|_| ContractError::InvalidField)
}
fn error(m: &Map<String, Value>) -> Result<CliExit, ContractError> {
    let kind: ErrorKind = enum_field(m, "kind")?;
    if count(m, "code")? != kind.exit_code() as u64 {
        return Err(ContractError::InconsistentFields);
    }
    text(field(m, "message")?)?;
    text(field(m, "hint")?)?;
    boolean(m, "retryable")?;
    Ok(kind.exit_code())
}

fn validate_decision(m: &Map<String, Value>, decision: Decision) -> Result<CliExit, ContractError> {
    version(m)?;
    // These fields belong only to a non-actionable artifact envelope.
    if [
        "kind",
        "actionable",
        "run_status",
        "gate_status",
        "hookSpecificOutput",
    ]
    .iter()
    .any(|k| m.contains_key(*k))
    {
        return Err(ContractError::InconsistentFields);
    }
    let exit = if decision == Decision::Unavailable {
        error(object(field(m, "error")?)?)?
    } else {
        if m.contains_key("error") {
            return Err(ContractError::InconsistentFields);
        }
        CliExit::Success
    };
    validate_unresolved(m, decision)?;
    // A pre-input failure has no fabricated session, roster or model metadata.
    if decision == Decision::Unavailable && !m.contains_key("event_id") {
        if m.get("skills")
            .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
        {
            return Err(ContractError::InconsistentFields);
        }
        if m.keys().any(|k| {
            matches!(
                k.as_str(),
                "needs_skill"
                    | "choice_confidence"
                    | "none_probability"
                    | "phase"
                    | "omitted_rank_mass"
                    | "quality"
                    | "context_quality"
                    | "roster"
                    | "cache"
                    | "model"
                    | "usage"
                    | "trace"
                    | "harness"
                    | "persistence"
            )
        }) {
            return Err(ContractError::InconsistentFields);
        }
        return Ok(exit);
    }
    identifier(field(m, "event_id")?)?;
    HarnessId::new(text(field(m, "harness")?)?).map_err(|_| ContractError::InvalidField)?;
    let reason = text(field(m, "reason")?)?;
    if !reason
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ContractError::InvalidField);
    }
    let quality: ContextQuality = enum_field(m, "context_quality")?;
    if quality == ContextQuality::Insufficient && decision != Decision::Unavailable {
        return Err(ContractError::InconsistentFields);
    }
    {
        let v = field(m, "quality")?;
        let q = object(v)?;
        for flag in [
            "prompt_complete",
            "task_anchor_known",
            "history_windowed",
            "attachments_omitted",
            "source_gaps",
        ] {
            boolean(q, flag)?;
        }
        if decision == Decision::Ranked
            && (!boolean(q, "prompt_complete")? || !boolean(q, "task_anchor_known")?)
        {
            return Err(ContractError::InconsistentFields);
        }
    }
    let need = nullable_probability(m, "needs_skill")?;
    let confidence = nullable_probability(m, "choice_confidence")?;
    let none = nullable_probability(m, "none_probability")?;
    if confidence.is_some() != none.is_some() {
        return Err(ContractError::InconsistentFields);
    }
    let phase = field(m, "phase")?;
    if !phase.is_null() {
        let _: Phase = enum_field(m, "phase")?;
    }
    let omitted = nullable_probability(m, "omitted_rank_mass")?;
    let skills = array(m, "skills", 32)?;
    match decision {
        Decision::Ranked
            if skills.is_empty()
                || need.is_none()
                || none.is_none()
                || phase.is_null()
                || omitted.is_none() =>
        {
            return Err(ContractError::InconsistentFields);
        }
        Decision::Explicit
            if skills.is_empty()
                || need.is_some()
                || confidence.is_some()
                || !phase.is_null()
                || omitted.is_some() =>
        {
            return Err(ContractError::InconsistentFields);
        }
        Decision::Abstain | Decision::Unavailable if !skills.is_empty() => {
            return Err(ContractError::InconsistentFields);
        }
        _ => {}
    }
    let mut ids = BTreeSet::new();
    let mut mass = 0.0;
    let mut wide_mass = 0.0;
    let mut rerank_mass = 0.0;
    for (index, skill) in skills.iter().enumerate() {
        let s = object(skill)?;
        let id =
            SkillId::new(text(field(s, "skill_id")?)?).map_err(|_| ContractError::InvalidField)?;
        if !ids.insert(id) || count(s, "rank")? != (index + 1) as u64 {
            return Err(ContractError::InconsistentFields);
        }
        text(field(s, "name")?)?;
        InvocationName::new(text(field(s, "invocation_name")?)?)
            .map_err(|_| ContractError::InvalidField)?;
        digest(field(s, "content_hash")?)?;
        let path = field(s, "path")?;
        if !path.is_null() {
            text(path)?;
        }
        for name in [
            "rank_score",
            "wide_probability",
            "rerank_probability",
            "fits",
        ] {
            let p = nullable_probability(s, name)?;
            if (decision == Decision::Explicit) != p.is_none() {
                return Err(ContractError::InconsistentFields);
            }
        }
        if decision == Decision::Ranked {
            mass += probability(field(s, "rank_score")?)?;
            wide_mass += probability(field(s, "wide_probability")?)?;
            rerank_mass += probability(field(s, "rerank_probability")?)?;
            if probability(field(s, "rerank_probability")?)? <= none.unwrap_or(1.0) {
                return Err(ContractError::InconsistentFields);
            }
        }
    }
    if decision == Decision::Ranked
        && ((mass + omitted.unwrap_or(0.0) - 1.0).abs() > SUM_TOLERANCE
            || wide_mass > 1.0 + SUM_TOLERANCE
            || rerank_mass + none.unwrap_or(0.0) > 1.0 + SUM_TOLERANCE)
    {
        return Err(ContractError::InconsistentFields);
    }
    validate_roster(object(field(m, "roster")?)?, decision, skills.len())?;
    validate_metadata(m)?;
    if decision == Decision::Explicit {
        let usage = object(field(m, "usage")?)?;
        let model = object(field(m, "model")?)?;
        let cache = object(field(m, "cache")?)?;
        if [
            "requests",
            "http_attempts",
            "input_tokens",
            "output_tokens",
            "unknown_usage_attempts",
        ]
        .iter()
        .any(|k| usage.get(*k).and_then(Value::as_u64) != Some(0))
            || [
                "requested",
                "wide_returned",
                "rerank_returned",
                "immutable_revision",
            ]
            .iter()
            .any(|k| !model.get(*k).is_some_and(Value::is_null))
            || boolean(cache, "hit")?
            || boolean(cache, "wide_hit")?
            || boolean(cache, "rerank_hit")?
        {
            return Err(ContractError::InconsistentFields);
        }
    }
    if let Some(v) = m.get("trace") {
        let trace = object(v)?;
        validate_trace(trace)?;
        let trace_snapshot = field(object(field(trace, "cursor")?)?, "snapshot_id")?;
        let roster = object(field(m, "roster")?)?;
        let provenance = object(field(roster, "provenance")?)?;
        if trace_snapshot != field(provenance, "snapshot_id")? {
            return Err(ContractError::InconsistentFields);
        }
    }
    Ok(exit)
}

fn validate_unresolved(m: &Map<String, Value>, decision: Decision) -> Result<(), ContractError> {
    if let Some(v) = m.get("unresolved") {
        let unresolved = v.as_array().ok_or(ContractError::InvalidField)?;
        if unresolved.len() > 32 {
            return Err(ContractError::LimitExceeded);
        }
        if !unresolved.is_empty() && decision != Decision::Unavailable {
            return Err(ContractError::InconsistentFields);
        }
        for item in unresolved {
            let item = object(item)?;
            text(field(item, "reference")?)?;
            let reason = text(field(item, "reason")?)?;
            if !matches!(reason, "missing" | "ambiguous" | "restricted") {
                return Err(ContractError::InvalidField);
            }
        }
    }
    Ok(())
}

fn validate_roster(
    m: &Map<String, Value>,
    decision: Decision,
    returned: usize,
) -> Result<(), ContractError> {
    let total = count(m, "total")?;
    let eligible = count(m, "eligible")?;
    let wide = count(m, "wide_candidates")?;
    let shortlist = count(m, "shortlist")?;
    boolean(m, "partial")?;
    if total > 10_000
        || eligible > total
        || wide > eligible
        || wide > 254
        || shortlist > wide
        || shortlist > 32
    {
        return Err(ContractError::InconsistentFields);
    }
    match text(field(m, "retrieval")?)? {
        "full" if eligible <= 254 && (wide == eligible || wide == 0) => {}
        "quill-bm25" if eligible > 254 => {}
        "not-evaluated" if wide == 0 && shortlist == 0 => {}
        _ => return Err(ContractError::InconsistentFields),
    }
    if decision == Decision::Ranked && (returned as u64 > shortlist || wide == 0) {
        return Err(ContractError::InconsistentFields);
    }
    {
        let v = field(m, "provenance")?;
        let p = object(v)?;
        digest(field(p, "snapshot_id")?)?;
        identifier(field(p, "policy_version")?)?;
        for (name, evaluated) in [("wide_set_id", wide > 0), ("rerank_set_id", shortlist > 0)] {
            let id = field(p, name)?;
            if id.is_null() == evaluated {
                return Err(ContractError::InconsistentFields);
            }
            if !id.is_null() {
                digest(id)?;
            }
        }
    }
    Ok(())
}

fn validate_metadata(m: &Map<String, Value>) -> Result<(), ContractError> {
    let cache = object(field(m, "cache")?)?;
    let hit = boolean(cache, "hit")?;
    let wide_hit = boolean(cache, "wide_hit")?;
    let rerank_hit = boolean(cache, "rerank_hit")?;
    if boolean(cache, "stale")? || (hit && !wide_hit) || (rerank_hit && !wide_hit) {
        return Err(ContractError::InconsistentFields);
    }
    let age = field(cache, "age_ms")?;
    if !age.is_null() && age.as_u64().is_none() {
        return Err(ContractError::InvalidField);
    }
    if hit && age.is_null() {
        return Err(ContractError::InconsistentFields);
    }
    let model = object(field(m, "model")?)?;
    for key in [
        "requested",
        "wide_returned",
        "rerank_returned",
        "immutable_revision",
    ] {
        let v = field(model, key)?;
        if !v.is_null() {
            text(v)?;
        }
    }
    let usage = object(field(m, "usage")?)?;
    let requests = count(usage, "requests")?;
    let attempts = count(usage, "http_attempts")?;
    let unknown = count(usage, "unknown_usage_attempts")?;
    if requests > 2 || attempts > 4 || requests > attempts || unknown > attempts {
        return Err(ContractError::InconsistentFields);
    }
    for name in ["input_tokens", "output_tokens"] {
        count(usage, name)?;
    }
    if hit
        && (requests != 0
            || attempts != 0
            || count(usage, "input_tokens")? != 0
            || count(usage, "output_tokens")? != 0)
    {
        return Err(ContractError::InconsistentFields);
    }
    if !matches!(
        text(field(m, "persistence")?)?,
        "recorded" | "disabled" | "unavailable"
    ) {
        return Err(ContractError::InvalidField);
    }
    count(m, "elapsed_ms")?;
    for warning in array(m, "warnings", MAX_WARNING_DETAILS)? {
        let w = object(warning)?;
        identifier(field(w, "kind")?)?;
        count(w, "count")?;
        text(field(w, "message")?)?;
    }
    count(m, "warnings_omitted")?;
    Ok(())
}

fn validate_artifact(m: &Map<String, Value>, kind: ArtifactKind) -> Result<CliExit, ContractError> {
    if boolean(m, "actionable")? || m.contains_key("decision") {
        return Err(ContractError::InconsistentFields);
    }
    let run: RunStatus = enum_field(m, "run_status")?;
    let gate: GateStatus = enum_field(m, "gate_status")?;
    let c = object(field(m, "completeness")?)?;
    let requested = count(c, "cases_requested")?;
    let completed = count(c, "cases_completed")?;
    let required = count(c, "stages_required")?;
    let evaluated = count(c, "stages_completed")?;
    let compatible = boolean(c, "evidence_compatible")?;
    if requested > 10_000 || completed > requested || evaluated > required || required > 20_000 {
        return Err(ContractError::InconsistentFields);
    }
    let complete = completed == requested && evaluated == required;
    if run == RunStatus::Complete && !complete {
        return Err(ContractError::InconsistentFields);
    }
    if gate == GateStatus::Passed
        && (run != RunStatus::Complete || !compatible || requested == 0 || required == 0)
    {
        return Err(ContractError::InconsistentFields);
    }
    match text(field(m, "evidence_origin")?)? {
        "synthetic" if gate == GateStatus::Passed => return Err(ContractError::InconsistentFields),
        "synthetic" | "recorded" | "live" => {}
        _ => return Err(ContractError::InvalidField),
    }
    if kind == ArtifactKind::Demo
        && (gate != GateStatus::NotApplicable || text(field(m, "evidence_origin")?)? != "synthetic")
    {
        return Err(ContractError::InconsistentFields);
    }
    for name in ["historical", "recomputed"] {
        if let Some(v) = m.get(name)
            && !v.is_null()
        {
            let inner = object(v)?;
            let decision = enum_field(inner, "decision")?;
            validate_decision(inner, decision)?;
        }
    }
    if matches!(kind, ArtifactKind::Demo | ArtifactKind::Replay) && !m.contains_key("historical") {
        return Err(ContractError::InvalidField);
    }
    if matches!(kind, ArtifactKind::Demo | ArtifactKind::Replay)
        && run == RunStatus::Complete
        && field(m, "historical")?.is_null()
    {
        return Err(ContractError::InconsistentFields);
    }
    if let Some(v) = m.get("error") {
        if run != RunStatus::Partial || gate == GateStatus::Passed {
            return Err(ContractError::InconsistentFields);
        }
        error(object(v)?)
    } else {
        Ok(CliExit::Success)
    }
}

/// A cursor names a snapshot and query, not a live transcript ingestion position.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TraceCursor {
    pub schema_version: u64,
    pub snapshot_id: ContentHash,
    pub query_id: ContentHash,
    pub offset: u64,
}

impl TraceCursor {
    pub fn resume(
        &self,
        snapshot: &ContentHash,
        query: &ContentHash,
        total: u64,
    ) -> Result<u64, ContractError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ContractError::UnsupportedVersion);
        }
        if &self.snapshot_id != snapshot || &self.query_id != query {
            return Err(ContractError::SnapshotChanged);
        }
        if self.offset > total || total > MAX_TRACE_ITEMS {
            return Err(ContractError::InvalidField);
        }
        Ok(self.offset)
    }
}

fn validate_trace(m: &Map<String, Value>) -> Result<(), ContractError> {
    let cursor: TraceCursor = serde_json::from_value(field(m, "cursor")?.clone())
        .map_err(|_| ContractError::InvalidField)?;
    let total = count(m, "total")?;
    cursor.resume(&cursor.snapshot_id, &cursor.query_id, total)?;
    let entries = array(m, "entries", MAX_TRACE_PAGE_ITEMS)?;
    let end = cursor
        .offset
        .checked_add(entries.len() as u64)
        .ok_or(ContractError::LimitExceeded)?;
    if end > total {
        return Err(ContractError::InconsistentFields);
    }
    let next = field(m, "next_offset")?;
    if end < total {
        if entries.is_empty() || next.as_u64() != Some(end) {
            return Err(ContractError::InconsistentFields);
        }
    } else if !next.is_null() {
        return Err(ContractError::InconsistentFields);
    }
    for entry in entries {
        let e = object(entry)?;
        SkillId::new(text(field(e, "skill_id")?)?).map_err(|_| ContractError::InvalidField)?;
        if !matches!(
            text(field(e, "stage")?)?,
            "discovery"
                | "visibility"
                | "local-policy"
                | "quill-admission"
                | "wide-shortlist"
                | "fit-none"
                | "ordering"
                | "publication"
        ) {
            return Err(ContractError::InvalidField);
        }
        let status = text(field(e, "status")?)?;
        if !matches!(
            status,
            "passed" | "excluded" | "not-evaluated" | "not-in-snapshot"
        ) {
            return Err(ContractError::InvalidField);
        }
        let operand = nullable_number(e, "value")?;
        let threshold = nullable_number(e, "threshold")?;
        let reason = field(e, "reason")?;
        if matches!(status, "not-evaluated" | "not-in-snapshot") {
            if operand.is_some() || threshold.is_some() || !reason.is_null() {
                return Err(ContractError::InconsistentFields);
            }
        } else {
            identifier(reason)?;
        }
    }
    Ok(())
}

pub(crate) fn check_depth(value: &Value, depth: usize) -> Result<(), ContractError> {
    if depth > MAX_OUTPUT_DEPTH {
        return Err(ContractError::LimitExceeded);
    }
    match value {
        Value::Array(v) => {
            for item in v {
                check_depth(item, depth + 1)?;
            }
        }
        Value::Object(v) => {
            for item in v.values() {
                check_depth(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

struct BoundedBuffer(Vec<u8>);
impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_OUTPUT_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("output limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(value: &Value) -> Result<Vec<u8>, ContractError> {
    let mut out = BoundedBuffer(Vec::new());
    serde_json::to_writer(&mut out, value).map_err(|_| ContractError::LimitExceeded)?;
    Ok(out.0)
}

// Unlike deserializing into Value directly, this rejects duplicate known AND
// additive keys before they can disappear. Errors are sanitized at the boundary.
pub(crate) struct JsonSeed(pub(crate) usize);
impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.0 > MAX_OUTPUT_DEPTH {
            return Err(serde::de::Error::custom("JSON depth limit"));
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonSeed {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(v) = seq.next_element_seed(JsonSeed(self.0 + 1))? {
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate JSON key"));
            }
            values.insert(key, map.next_value_seed(JsonSeed(self.0 + 1))?);
        }
        Ok(Value::Object(values))
    }
}
