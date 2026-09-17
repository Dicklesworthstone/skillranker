//! Versioned adapter and capability contracts.
//!
//! These types freeze first-supported source boundaries and the advice gate.
//! They do not read transcripts, start cass, install hooks, or emit advice.

use crate::context::PrivateText;
use crate::identity::{
    AdapterId, AdapterVersion, ContentHash, EventId, IdentityError, SessionId, SourceId,
};
use crate::limits::{
    HOOK_ADDITIONAL_CONTEXT_SCALARS, HOOK_STDIN_BYTES, LimitError, NORMALIZED_CONTEXT_DEPTH,
};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const CONTRACT_VERSION: u32 = 1;
pub const ADDITIONAL_CONTEXT_MAX_CHARS: usize = HOOK_ADDITIONAL_CONTEXT_SCALARS.max();
pub const MAX_SUGGESTED_INVOCATION_NAMES: usize = 1;
pub const USER_PROMPT_SUBMIT: &str = "UserPromptSubmit";
pub const USER_PROMPT_EXPANSION: &str = "UserPromptExpansion";
pub const CLAUDE_CODE_ID: &str = "claude_code";
pub const NORMALIZED_ID: &str = "normalized";
pub const CASS_ID: &str = "cass";
pub const FOUNDATION_IMPLEMENTED_CLI: &[&str] = &["help", "version"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    ClaudeHook,
    NativeTranscript,
    NormalizedContext,
    CassSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportClass {
    Implemented,
    Tested,
    Unverified,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    OfficialSchema,
    LocalVersionObservation,
    FixtureTest,
    RealHarnessSmoke,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRunStatus {
    Passed,
    Failed,
    NotRun,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceDimension {
    PromptTiming,
    BranchIdentity,
    Visibility,
    Restrictions,
    Compaction,
    LoadEvidence,
    HookOutput,
    Deadline,
    Delivery,
}

pub const NATIVE_ADVICE_DIMENSIONS: &[ConformanceDimension] = &[
    ConformanceDimension::PromptTiming,
    ConformanceDimension::BranchIdentity,
    ConformanceDimension::Visibility,
    ConformanceDimension::Restrictions,
    ConformanceDimension::Compaction,
    ConformanceDimension::LoadEvidence,
    ConformanceDimension::HookOutput,
    ConformanceDimension::Deadline,
    ConformanceDimension::Delivery,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceStatus {
    Pass,
    Fail,
    NotEvaluated,
    Incompatible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticsCompatibility {
    Compatible,
    Incompatible,
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownFieldPolicy {
    RejectUnknown,
    RetainAdditive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityQuestion {
    AcceptInput,
    EmitNativeAdvice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseGate {
    P0,
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
    P8,
    P9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterError {
    InvalidJson,
    DuplicateKey,
    LimitExceeded,
    UnsupportedVersion,
    UnsupportedEvent,
    UnknownEvent,
    InvalidField,
    IncompatibleSemantics,
    SupportInheritanceForbidden,
    RemoteCassSourceRejected,
    MissingCassProvenance,
    HookOutputLimit,
    UnsafeHookText,
    ConflictingSourceFlags,
    MissingExplicitStdinMode,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidJson => "invalid adapter JSON",
            Self::DuplicateKey => "duplicate JSON key",
            Self::LimitExceeded => "adapter envelope exceeds its byte or depth limit",
            Self::UnsupportedVersion => "unsupported adapter or capabilities schema version",
            Self::UnsupportedEvent => "event is known and is not on the advisory path",
            Self::UnknownEvent => "unknown hook event",
            Self::InvalidField => "invalid or missing adapter field",
            Self::IncompatibleSemantics => {
                "adapter identity or visibility semantics are incompatible"
            }
            Self::SupportInheritanceForbidden => {
                "unverified harness versions cannot inherit support"
            }
            Self::RemoteCassSourceRejected => "remote cass sources are rejected by default",
            Self::MissingCassProvenance => {
                "cass support claims need a build commit or binary digest"
            }
            Self::HookOutputLimit => "hook additionalContext exceeds 1024 characters",
            Self::UnsafeHookText => "hook output contains a forbidden control character",
            Self::ConflictingSourceFlags => "source selection flags are mutually exclusive",
            Self::MissingExplicitStdinMode => "piped stdin requires an explicit source mode",
        })
    }
}

impl std::error::Error for AdapterError {}

impl From<LimitError> for AdapterError {
    fn from(error: LimitError) -> Self {
        match error {
            LimitError::AboveLimit { .. } => Self::LimitExceeded,
            _ => Self::InvalidField,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdviceBlockReason {
    UnsupportedSemanticVersion,
    UnknownSemanticVersion,
    UnsupportedEvent,
    UnknownEvent,
    IncompatibleIdentitySemantics,
    IncompatibleVisibilitySemantics,
    UnverifiedHarness,
    SupportInheritanceForbidden,
    MissingRequiredEvidence,
    FixtureDigestIsNotInstalledProof,
    InstalledVersionNotTested,
    InvalidVersionDefinitions,
    DimensionNotEvaluated,
    DimensionFailed,
    CassNotOnDefaultHookPath,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdviceDisposition {
    Eligible,
    Disabled(AdviceBlockReason),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    pub class: EvidenceClass,
    pub run_status: EvidenceRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<AdapterVersion>,
}

impl EvidenceRecord {
    pub fn authorizes_installed_version(&self, installed: &AdapterVersion) -> bool {
        match self.class {
            EvidenceClass::FixtureTest | EvidenceClass::OfficialSchema => false,
            EvidenceClass::LocalVersionObservation => false,
            EvidenceClass::RealHarnessSmoke => {
                self.run_status == EvidenceRunStatus::Passed
                    && self.observed_version.as_ref() == Some(installed)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceCell {
    pub status: ConformanceStatus,
    #[serde(default)]
    pub evidence: Vec<EvidenceRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterRecord {
    pub adapter_id: AdapterId,
    pub kind: AdapterKind,
    pub support: SupportClass,
    pub contract_version: u32,
    pub on_default_hook_path: bool,
    pub identity_semantics: SemanticsCompatibility,
    pub visibility_semantics: SemanticsCompatibility,
    #[serde(default)]
    pub tested_versions: Vec<AdapterVersion>,
    #[serde(default)]
    pub unverified_versions: Vec<AdapterVersion>,
    pub conformance: BTreeMap<ConformanceDimension, ConformanceCell>,
}

impl AdapterRecord {
    pub fn advice(
        &self,
        question: CompatibilityQuestion,
        installed: Option<&AdapterVersion>,
    ) -> AdviceDisposition {
        if self.contract_version != CONTRACT_VERSION {
            return AdviceDisposition::Disabled(AdviceBlockReason::UnsupportedSemanticVersion);
        }
        if !self.version_definitions_are_valid() {
            return AdviceDisposition::Disabled(AdviceBlockReason::InvalidVersionDefinitions);
        }
        match self.identity_semantics {
            SemanticsCompatibility::Compatible => {}
            SemanticsCompatibility::Incompatible => {
                return AdviceDisposition::Disabled(
                    AdviceBlockReason::IncompatibleIdentitySemantics,
                );
            }
            SemanticsCompatibility::Unverified => {
                if question == CompatibilityQuestion::EmitNativeAdvice {
                    return AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness);
                }
            }
        }
        match self.visibility_semantics {
            SemanticsCompatibility::Compatible => {}
            SemanticsCompatibility::Incompatible => {
                return AdviceDisposition::Disabled(
                    AdviceBlockReason::IncompatibleVisibilitySemantics,
                );
            }
            SemanticsCompatibility::Unverified => {
                if question == CompatibilityQuestion::EmitNativeAdvice {
                    return AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness);
                }
            }
        }
        match question {
            CompatibilityQuestion::AcceptInput => self.input_acceptance(),
            CompatibilityQuestion::EmitNativeAdvice => self.native_advice(installed),
        }
    }

    fn version_definitions_are_valid(&self) -> bool {
        let mut versions = BTreeSet::new();
        self.tested_versions
            .iter()
            .chain(&self.unverified_versions)
            .all(|version| versions.insert(version))
    }

    fn input_acceptance(&self) -> AdviceDisposition {
        match self.kind {
            AdapterKind::NormalizedContext
                if self.support == SupportClass::Implemented
                    || self.support == SupportClass::Tested =>
            {
                AdviceDisposition::Eligible
            }
            AdapterKind::NormalizedContext => {
                AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness)
            }
            AdapterKind::ClaudeHook | AdapterKind::NativeTranscript | AdapterKind::CassSession => {
                if self.support == SupportClass::Unavailable {
                    AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness)
                } else {
                    AdviceDisposition::Disabled(AdviceBlockReason::MissingRequiredEvidence)
                }
            }
        }
    }

    fn native_advice(&self, installed: Option<&AdapterVersion>) -> AdviceDisposition {
        if matches!(self.kind, AdapterKind::CassSession) {
            return AdviceDisposition::Disabled(AdviceBlockReason::CassNotOnDefaultHookPath);
        }
        if matches!(self.kind, AdapterKind::NormalizedContext) {
            return AdviceDisposition::Disabled(AdviceBlockReason::MissingRequiredEvidence);
        }
        if self.support != SupportClass::Tested {
            return AdviceDisposition::Disabled(if self.support == SupportClass::Implemented {
                AdviceBlockReason::MissingRequiredEvidence
            } else {
                AdviceBlockReason::UnverifiedHarness
            });
        }
        let Some(installed) = installed else {
            return AdviceDisposition::Disabled(AdviceBlockReason::InstalledVersionNotTested);
        };
        if !self
            .tested_versions
            .iter()
            .any(|version| version == installed)
        {
            return AdviceDisposition::Disabled(AdviceBlockReason::InstalledVersionNotTested);
        }
        for dimension in NATIVE_ADVICE_DIMENSIONS {
            let Some(cell) = self.conformance.get(dimension) else {
                return AdviceDisposition::Disabled(AdviceBlockReason::DimensionNotEvaluated);
            };
            match cell.status {
                ConformanceStatus::Pass => {}
                ConformanceStatus::Fail | ConformanceStatus::Incompatible => {
                    return AdviceDisposition::Disabled(AdviceBlockReason::DimensionFailed);
                }
                ConformanceStatus::NotEvaluated => {
                    return AdviceDisposition::Disabled(AdviceBlockReason::DimensionNotEvaluated);
                }
            }
            let smoke_ok = cell.evidence.iter().any(|record| {
                record.class == EvidenceClass::RealHarnessSmoke
                    && record.authorizes_installed_version(installed)
            });
            if !smoke_ok {
                let fixture_only = cell
                    .evidence
                    .iter()
                    .any(|record| record.class == EvidenceClass::FixtureTest);
                return AdviceDisposition::Disabled(if fixture_only {
                    AdviceBlockReason::FixtureDigestIsNotInstalledProof
                } else {
                    AdviceBlockReason::MissingRequiredEvidence
                });
            }
        }
        AdviceDisposition::Eligible
    }
}

/// Native support is reusable only for the same adapter and an eligible version.
/// Version-list membership alone cannot stand in for the conformance evidence.
pub fn transfer_tested_support(
    source: &AdapterRecord,
    target_id: &AdapterId,
    target_version: &AdapterVersion,
) -> Result<(), AdapterError> {
    if source.adapter_id != *target_id
        || source.advice(
            CompatibilityQuestion::EmitNativeAdvice,
            Some(target_version),
        ) != AdviceDisposition::Eligible
    {
        return Err(AdapterError::SupportInheritanceForbidden);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedCommand {
    pub name: String,
    pub earliest_phase: PhaseGate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesDocument {
    pub schema_version: u32,
    pub adapter_contract_version: u32,
    pub implemented_cli: Vec<String>,
    pub planned_cli: Vec<PlannedCommand>,
    pub adapters: Vec<AdapterRecord>,
}

impl CapabilitiesDocument {
    pub fn from_json(bytes: &[u8]) -> Result<Self, AdapterError> {
        let value = decode_json(bytes, HOOK_STDIN_BYTES.max())?;
        let document: Self =
            serde_json::from_value(value).map_err(|_| AdapterError::InvalidField)?;
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), AdapterError> {
        if self.schema_version != CONTRACT_VERSION
            || self.adapter_contract_version != CONTRACT_VERSION
        {
            return Err(AdapterError::UnsupportedVersion);
        }
        let implemented: BTreeSet<&str> = self.implemented_cli.iter().map(String::as_str).collect();
        if implemented.len() != self.implemented_cli.len()
            || implemented != BTreeSet::from_iter(FOUNDATION_IMPLEMENTED_CLI.iter().copied())
        {
            return Err(AdapterError::InvalidField);
        }
        let mut planned = BTreeSet::new();
        for command in &self.planned_cli {
            let name = command.name.as_str();
            let valid_name = name.len() <= 64
                && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && name.split('-').all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                });
            if !valid_name || implemented.contains(name) || !planned.insert(name) {
                return Err(AdapterError::InvalidField);
            }
        }
        let mut ids = BTreeSet::new();
        for adapter in &self.adapters {
            if adapter.contract_version != CONTRACT_VERSION {
                return Err(AdapterError::UnsupportedVersion);
            }
            if !ids.insert(adapter.adapter_id.clone()) {
                return Err(AdapterError::InvalidField);
            }
            if !adapter.version_definitions_are_valid()
                || (adapter.kind == AdapterKind::CassSession && adapter.on_default_hook_path)
            {
                return Err(AdapterError::InvalidField);
            }
        }
        Ok(())
    }

    pub fn implemented_adapter(&self, id: &str) -> Option<&AdapterRecord> {
        self.adapters
            .iter()
            .find(|adapter| adapter.adapter_id.as_str() == id)
    }
}

pub fn foundation_capabilities() -> Result<CapabilitiesDocument, AdapterError> {
    let document = CapabilitiesDocument {
        schema_version: CONTRACT_VERSION,
        adapter_contract_version: CONTRACT_VERSION,
        implemented_cli: FOUNDATION_IMPLEMENTED_CLI
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        planned_cli: vec![
            planned("rank", PhaseGate::P4),
            planned("hook", PhaseGate::P6),
            planned("roster", PhaseGate::P2),
            planned("doctor", PhaseGate::P4),
            planned("capabilities", PhaseGate::P4),
            planned("demo", PhaseGate::P4),
            planned("install-hook", PhaseGate::P6),
            planned("uninstall-hook", PhaseGate::P6),
            planned("stats", PhaseGate::P5),
            planned("observe", PhaseGate::P5),
            planned("feedback", PhaseGate::P5),
            planned("snooze", PhaseGate::P6),
            planned("budget", PhaseGate::P6),
            planned("replay", PhaseGate::P5),
            planned("eval", PhaseGate::P5),
            planned("calibrate", PhaseGate::P8),
            planned("ledger", PhaseGate::P5),
            planned("tui", PhaseGate::P9),
            planned("gaps", PhaseGate::P9),
        ],
        adapters: vec![
            AdapterRecord {
                adapter_id: AdapterId::new(NORMALIZED_ID)
                    .map_err(|_| AdapterError::InvalidField)?,
                kind: AdapterKind::NormalizedContext,
                support: SupportClass::Implemented,
                contract_version: CONTRACT_VERSION,
                on_default_hook_path: false,
                identity_semantics: SemanticsCompatibility::Compatible,
                visibility_semantics: SemanticsCompatibility::Compatible,
                tested_versions: Vec::new(),
                unverified_versions: Vec::new(),
                conformance: unevaluated_matrix(),
            },
            AdapterRecord {
                adapter_id: AdapterId::new(CLAUDE_CODE_ID)
                    .map_err(|_| AdapterError::InvalidField)?,
                kind: AdapterKind::ClaudeHook,
                support: SupportClass::Unverified,
                contract_version: CONTRACT_VERSION,
                on_default_hook_path: true,
                identity_semantics: SemanticsCompatibility::Unverified,
                visibility_semantics: SemanticsCompatibility::Unverified,
                tested_versions: Vec::new(),
                unverified_versions: Vec::new(),
                conformance: unevaluated_matrix(),
            },
            AdapterRecord {
                adapter_id: AdapterId::new(CASS_ID).map_err(|_| AdapterError::InvalidField)?,
                kind: AdapterKind::CassSession,
                support: SupportClass::Unverified,
                contract_version: CONTRACT_VERSION,
                on_default_hook_path: false,
                identity_semantics: SemanticsCompatibility::Unverified,
                visibility_semantics: SemanticsCompatibility::Unverified,
                tested_versions: Vec::new(),
                unverified_versions: Vec::new(),
                conformance: unevaluated_matrix(),
            },
        ],
    };
    document.validate()?;
    Ok(document)
}

fn planned(name: &str, earliest_phase: PhaseGate) -> PlannedCommand {
    PlannedCommand {
        name: name.to_string(),
        earliest_phase,
    }
}

fn unevaluated_matrix() -> BTreeMap<ConformanceDimension, ConformanceCell> {
    NATIVE_ADVICE_DIMENSIONS
        .iter()
        .map(|dimension| {
            (
                *dimension,
                ConformanceCell {
                    status: ConformanceStatus::NotEvaluated,
                    evidence: Vec::new(),
                },
            )
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeHookEvent {
    UserPromptSubmit,
    UserPromptExpansion,
}

impl ClaudeHookEvent {
    pub fn parse(name: &str) -> Result<Self, AdapterError> {
        match name {
            USER_PROMPT_SUBMIT => Ok(Self::UserPromptSubmit),
            USER_PROMPT_EXPANSION => Err(AdapterError::UnsupportedEvent),
            _ => Err(AdapterError::UnknownEvent),
        }
    }

    pub fn advice(self) -> AdviceDisposition {
        match self {
            Self::UserPromptSubmit => AdviceDisposition::Eligible,
            Self::UserPromptExpansion => {
                AdviceDisposition::Disabled(AdviceBlockReason::UnsupportedEvent)
            }
        }
    }
}

pub struct ClaudeUserPromptSubmit {
    pub session_id: Option<SessionId>,
    pub transcript_path: Option<PrivateText>,
    pub cwd: Option<PrivateText>,
    pub prompt: PrivateText,
    pub prompt_id: Option<EventId>,
    additive: BTreeMap<String, Value>,
}

impl fmt::Debug for ClaudeUserPromptSubmit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeUserPromptSubmit")
            .field("session_id", &self.session_id)
            .field("transcript_path", &self.transcript_path)
            .field("cwd", &self.cwd)
            .field("prompt", &self.prompt)
            .field("prompt_id", &self.prompt_id)
            .field("additive_field_count", &self.additive.len())
            .finish()
    }
}

impl PartialEq for ClaudeUserPromptSubmit {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.transcript_path == other.transcript_path
            && self.cwd == other.cwd
            && self.prompt == other.prompt
            && self.prompt_id == other.prompt_id
            && self.additive == other.additive
    }
}

impl Eq for ClaudeUserPromptSubmit {}

impl ClaudeUserPromptSubmit {
    pub fn from_json(bytes: &[u8], policy: UnknownFieldPolicy) -> Result<Self, AdapterError> {
        HOOK_STDIN_BYTES.check_bytes(bytes)?;
        let value = decode_json(bytes, HOOK_STDIN_BYTES.max())?;
        Self::from_value(value, policy)
    }

    fn from_value(value: Value, policy: UnknownFieldPolicy) -> Result<Self, AdapterError> {
        let object = value.as_object().ok_or(AdapterError::InvalidField)?;
        let event = object
            .get("hook_event_name")
            .and_then(Value::as_str)
            .ok_or(AdapterError::InvalidField)?;
        ClaudeHookEvent::parse(event)?;
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or(AdapterError::InvalidField)?;
        let mut envelope = Self {
            session_id: optional_id(object, "session_id", SessionId::new)?,
            transcript_path: optional_text(object, "transcript_path")?,
            cwd: optional_text(object, "cwd")?,
            prompt: PrivateText::new(prompt),
            prompt_id: optional_id(object, "prompt_id", EventId::new)?,
            additive: BTreeMap::new(),
        };
        const KNOWN: &[&str] = &[
            "hook_event_name",
            "prompt",
            "prompt_id",
            "session_id",
            "transcript_path",
            "cwd",
        ];
        for (key, value) in object {
            if KNOWN.contains(&key.as_str()) {
                continue;
            }
            match policy {
                UnknownFieldPolicy::RejectUnknown => return Err(AdapterError::InvalidField),
                UnknownFieldPolicy::RetainAdditive => {
                    envelope.additive.insert(key.clone(), value.clone());
                }
            }
        }
        Ok(envelope)
    }

    pub fn current_request_event(&self) -> Option<&EventId> {
        self.prompt_id.as_ref()
    }

    pub fn additive_keys(&self) -> impl Iterator<Item = &str> {
        self.additive.keys().map(String::as_str)
    }

    pub fn transcript_state(
        transcript_exists: bool,
        transcript_malformed: bool,
    ) -> Result<HookTranscriptState, AdapterError> {
        if transcript_malformed {
            return Err(AdapterError::InvalidField);
        }
        Ok(if transcript_exists {
            HookTranscriptState::Present
        } else {
            HookTranscriptState::Missing
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookTranscriptState {
    Missing,
    Present,
}

fn optional_text(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<PrivateText>, AdapterError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(PrivateText::new(value))),
        Some(_) => Err(AdapterError::InvalidField),
    }
}

fn optional_id<T>(
    object: &Map<String, Value>,
    key: &str,
    ctor: fn(String) -> Result<T, IdentityError>,
) -> Result<Option<T>, AdapterError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => ctor(value.clone())
            .map(Some)
            .map_err(|_| AdapterError::InvalidField),
        Some(_) => Err(AdapterError::InvalidField),
    }
}

pub fn additional_context_allowed(
    text: &str,
    suggested_invocation_names: usize,
) -> Result<(), AdapterError> {
    if suggested_invocation_names > MAX_SUGGESTED_INVOCATION_NAMES {
        return Err(AdapterError::HookOutputLimit);
    }
    HOOK_ADDITIONAL_CONTEXT_SCALARS
        .check_unicode_scalars(text)
        .map_err(|_| AdapterError::HookOutputLimit)?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(AdapterError::UnsafeHookText);
    }
    Ok(())
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CassProducer {
    pub version: AdapterVersion,
    pub api_version: u32,
    pub contract_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_digest: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<SourceId>,
    #[serde(default)]
    pub remote_source: bool,
    #[serde(default = "true_flag")]
    pub export_omits_skills_by_default: bool,
    #[serde(default = "true_flag")]
    pub export_retains_native_shapes: bool,
}

impl fmt::Debug for CassProducer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CassProducer")
            .field("version", &self.version)
            .field("api_version", &self.api_version)
            .field("contract_version", &self.contract_version)
            .field("has_build_commit", &self.build_commit.is_some())
            .field("binary_digest", &self.binary_digest)
            .field("source_id", &self.source_id)
            .field("remote_source", &self.remote_source)
            .field(
                "export_omits_skills_by_default",
                &self.export_omits_skills_by_default,
            )
            .field(
                "export_retains_native_shapes",
                &self.export_retains_native_shapes,
            )
            .finish()
    }
}

fn is_cass_commit_id(commit: &str) -> bool {
    // cass emits a 12-digit abbreviation; full SHA-1/SHA-256 IDs are also valid.
    matches!(commit.len(), 12 | 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn true_flag() -> bool {
    true
}

impl CassProducer {
    pub fn from_json(bytes: &[u8]) -> Result<Self, AdapterError> {
        let value = decode_json(bytes, HOOK_STDIN_BYTES.max())?;
        let producer: Self =
            serde_json::from_value(value).map_err(|_| AdapterError::InvalidField)?;
        producer.validate_archive_identity()?;
        Ok(producer)
    }

    pub fn validate_archive_identity(&self) -> Result<(), AdapterError> {
        if self.remote_source {
            return Err(AdapterError::RemoteCassSourceRejected);
        }
        if self.api_version == 0 || self.contract_version == 0 {
            return Err(AdapterError::UnsupportedVersion);
        }
        if let Some(commit) = &self.build_commit
            && commit != "unknown"
            && !is_cass_commit_id(commit.strip_suffix("-dirty").unwrap_or(commit))
        {
            return Err(AdapterError::InvalidField);
        }
        Ok(())
    }

    pub fn validate_support_claim(&self) -> Result<(), AdapterError> {
        self.validate_archive_identity()?;
        // Unknown and dirty producer labels are valid archive metadata, but
        // neither identifies the built bytes without a separate binary digest.
        let has_clean_commit = self.build_commit.as_deref().is_some_and(is_cass_commit_id);
        if !has_clean_commit && self.binary_digest.is_none() {
            return Err(AdapterError::MissingCassProvenance);
        }
        Ok(())
    }

    pub fn native_export_is_our_normalized_envelope(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceRequest {
    pub claude_hook: bool,
    pub context_file: bool,
    pub native_transcript: bool,
    pub cass_session: bool,
    /// Input availability is not authority to change the declared source.
    pub stdin_present: bool,
    /// Explicit stdin selection for normalized context (or redundant hook input).
    pub stdin_mode_explicit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectedSource {
    ClaudeHook,
    NormalizedContext { stdin: bool },
    NativeTranscript,
    CassSession,
    Discovery,
}

pub fn select_source(request: SourceRequest) -> Result<SelectedSource, AdapterError> {
    let selected = [
        request.claude_hook,
        request.context_file,
        request.native_transcript,
        request.cass_session,
    ]
    .iter()
    .filter(|flag| **flag)
    .count();
    if selected > 1
        || (request.stdin_mode_explicit && !request.context_file && !request.claude_hook)
    {
        return Err(AdapterError::ConflictingSourceFlags);
    }
    let explicit_stdin = request.stdin_mode_explicit || request.claude_hook;
    if request.stdin_present && !explicit_stdin {
        return Err(AdapterError::MissingExplicitStdinMode);
    }
    Ok(if request.claude_hook {
        SelectedSource::ClaudeHook
    } else if request.context_file {
        SelectedSource::NormalizedContext {
            stdin: request.stdin_mode_explicit,
        }
    } else if request.native_transcript {
        SelectedSource::NativeTranscript
    } else if request.cass_session {
        SelectedSource::CassSession
    } else {
        SelectedSource::Discovery
    })
}

fn decode_json(bytes: &[u8], max_bytes: usize) -> Result<Value, AdapterError> {
    if bytes.len() > max_bytes {
        return Err(AdapterError::LimitExceeded);
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = JsonSeed(0).deserialize(&mut decoder).map_err(|error| {
        let message = error.to_string();
        if message.contains("duplicate JSON key") {
            AdapterError::DuplicateKey
        } else if message.contains("JSON depth limit") {
            AdapterError::LimitExceeded
        } else {
            AdapterError::InvalidJson
        }
    })?;
    decoder.end().map_err(|_| AdapterError::InvalidJson)?;
    Ok(value)
}

struct JsonSeed(usize);

impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.0 > NORMALIZED_CONTEXT_DEPTH.max() {
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
