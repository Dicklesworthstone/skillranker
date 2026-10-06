//! Bounded, offline replay and policy comparison (P5).
//!
//! Replay evaluates recorded or synthetic cases without network, child processes,
//! transcript discovery, skill execution, or state writes. Cases and policy overrides
//! are strictly bounded, owner-only, and validated before evaluation.

pub mod frozen;
use frozen::FrozenReplayInputs;

use crate::identity::SkillId;
use crate::limits::{REPLAY_POLICY_BYTES, REPLAY_POLICY_DEPTH};
use crate::output::{
    CliExit, ErrorKind, GateStatus, MAX_OUTPUT_DEPTH, OutputDocument, RunStatus, SCHEMA_VERSION,
};
use crate::scoring::{Input, Weights, rank};
use crate::storage::export::{
    DEFAULT_MAX_CASE_BYTES, ExportConfig, ExportError, export_private_atomic,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;

/// Bounded replay error kinds. Safe diagnostics contain no private text or credentials.
#[derive(Debug)]
pub enum ReplayError {
    OversizedCase { len: usize, max: usize },
    OversizedPolicy { len: usize, max: usize },
    ExcessiveDepth,
    InvalidJson(String),
    DuplicateKey(String),
    UnsupportedVersion(u64),
    InvalidField(String),
    OptionMapMismatch(String),
    IncompatiblePolicy(String),
    NotReplayable(String),
    Export(ExportError),
    Io(std::io::Error),
}

impl ReplayError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::OversizedCase { .. } | Self::OversizedPolicy { .. } => ErrorKind::OversizedInput,
            Self::ExcessiveDepth
            | Self::InvalidJson(_)
            | Self::DuplicateKey(_)
            | Self::UnsupportedVersion(_)
            | Self::InvalidField(_)
            | Self::OptionMapMismatch(_) => ErrorKind::MalformedInput,
            Self::IncompatiblePolicy(_) | Self::NotReplayable(_) => ErrorKind::InvalidConfiguration,
            Self::Export(err) => err.kind(),
            Self::Io(err) => match err.kind() {
                std::io::ErrorKind::NotFound => ErrorKind::InvalidUsage,
                std::io::ErrorKind::PermissionDenied => ErrorKind::MalformedInput,
                _ => ErrorKind::StorageFailure,
            },
        }
    }

    pub fn exit_code(&self) -> CliExit {
        self.kind().exit_code()
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OversizedCase { len, max } => {
                write!(f, "replay case exceeds maximum bytes ({len} > {max})")
            }
            Self::OversizedPolicy { len, max } => {
                write!(f, "replay policy exceeds maximum bytes ({len} > {max})")
            }
            Self::ExcessiveDepth => f.write_str("replay input exceeds maximum nesting depth"),
            Self::InvalidJson(_) => f.write_str("invalid bounded replay JSON"),
            Self::DuplicateKey(_) => f.write_str("duplicate JSON key in replay input"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported replay schema version: {v}"),
            Self::InvalidField(msg) => write!(f, "invalid field in replay input: {msg}"),
            Self::OptionMapMismatch(msg) => write!(f, "replay option map mismatch: {msg}"),
            Self::IncompatiblePolicy(msg) => write!(f, "incompatible replay policy: {msg}"),
            Self::NotReplayable(msg) => write!(f, "case is not replayable: {msg}"),
            Self::Export(err) => write!(f, "replay export failed: {err}"),
            Self::Io(err) => write!(f, "replay I/O error: {err}"),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<ExportError> for ReplayError {
    fn from(err: ExportError) -> Self {
        Self::Export(err)
    }
}

impl From<std::io::Error> for ReplayError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Current captured case schema; old readers must reject new frozen semantics.
pub const REPLAY_CASE_SCHEMA_VERSION: u64 = 2;

/// A captured replay case, with explicit missing-input metadata when incomplete.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCase {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_inputs: Option<FrozenReplayInputs>,
    pub schema_version: u64,
    pub case_id: String,
    pub created_at_unix_ms: u64,
    pub manifest: ReplayManifest,
    pub captured_request: CapturedRequest,
    pub recorded_responses: RecordedResponses,
    pub local_evidence: CapturedLocalEvidence,
    pub historical_decision: Value,
}

/// Provenance and stage metadata for the recorded case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayManifest {
    pub evidence_origin: String,
    pub adapter: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub stages_recorded: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_summary: Option<String>,
}

/// Bounded redacted request inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_text: Option<String>,
    #[serde(default)]
    pub current_constraints: Vec<String>,
    pub candidate_options: Vec<CapturedCandidate>,
}

/// Candidate skill metadata captured at the time of the decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCandidate {
    pub skill_id: String,
    pub invocation_name: String,
    pub content_hash: String,
    pub source: String,
    pub usage_kind: String,
    /// The visibility label the live decision reported for this candidate, so a
    /// replayed recommendation keeps the caveat that a harness's precedence is
    /// unverified. Absent in cases captured before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// Validated recorded provider responses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedResponses {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wide: Option<RecordedWideChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<RecordedRerankChoice>,
}

/// Recorded wide-choice provider response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedWideChoice {
    pub choice: String,
    pub choices_probability: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_score: Option<f64>,
    pub distribution: Vec<ChoiceDistributionItem>,
}

/// Recorded rerank-choice provider response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedRerankChoice {
    pub choice: String,
    pub choices_probability: f64,
    /// The provider's own stated confidence for the choice question, which is
    /// what a live decision reports as `choice_confidence`. Absent in cases
    /// captured before this field existed; absent is then reported as unknown
    /// rather than filled with a different quantity.
    #[serde(default)]
    pub stated_confidence: Option<f64>,
    #[serde(default)]
    pub fits: Vec<CandidateFitItem>,
    pub distribution: Vec<ChoiceDistributionItem>,
}

/// One candidate option's probability in a provider distribution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceDistributionItem {
    pub option_id: String,
    pub probability: f64,
}

/// One candidate's fit score from question evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateFitItem {
    pub skill_id: String,
    pub fit: f64,
}

/// Local state and evidence frozen at the time of the decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedLocalEvidence {
    pub as_of_unix_ms: u64,
    #[serde(default)]
    pub active_snoozes: Vec<String>,
    #[serde(default)]
    pub loaded_references: Vec<CapturedLoadedReference>,
    pub scoring_profile: CapturedScoringProfile,
}

/// Loaded reference record frozen in local evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedLoadedReference {
    pub skill_id: String,
    pub content_hash: String,
    pub availability: String,
}

/// Scoring parameters frozen at decision time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedScoringProfile {
    pub gate_threshold: f64,
    pub fit_threshold: f64,
    pub w_fit: f64,
    pub w_prior: f64,
    pub w_phase: f64,
    pub top_k: usize,
}

/// Optional local policy overrides for replay comparison.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayPolicy {
    #[serde(alias = "gate", skip_serializing_if = "Option::is_none")]
    pub gate_threshold: Option<f64>,
    #[serde(alias = "fits", alias = "fit", skip_serializing_if = "Option::is_none")]
    pub fit_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_fit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_prior: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_phase: Option<f64>,
    #[serde(alias = "top", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
}

/// The result of replaying a case.
#[derive(Clone, Debug)]
pub struct ReplayOutcome {
    pub document: OutputDocument,
    pub run_status: RunStatus,
    pub gate_status: GateStatus,
    pub historical_decision: String,
    pub recomputed_decision: Option<String>,
    pub explanation: Option<String>,
}

/// Count encoded bytes before growing the buffer, including JSON escaping.
/// Geometric growth stays within the cap; an oversized case is never exported
/// from its incomplete prefix.
struct CaseWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    oversized: Option<usize>,
    allocation_failed: bool,
}

impl CaseWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            oversized: None,
            allocation_failed: false,
        }
    }
}

impl Write for CaseWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next_len = self.bytes.len().saturating_add(bytes.len());
        if next_len > self.max_bytes {
            self.oversized = Some(next_len);
            return Err(std::io::Error::other(
                "case serialization exceeds its byte cap",
            ));
        }
        if next_len > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(4096)
                .max(next_len)
                .min(self.max_bytes);
            if self
                .bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .is_err()
            {
                self.allocation_failed = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "case serialization allocation unavailable",
                ));
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ReplayCase {
    /// Parse and validate a replay case from raw bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, ReplayError> {
        if bytes.len() > DEFAULT_MAX_CASE_BYTES {
            return Err(ReplayError::OversizedCase {
                len: bytes.len(),
                max: DEFAULT_MAX_CASE_BYTES,
            });
        }
        let value = parse_bounded_json(bytes, MAX_OUTPUT_DEPTH)?;
        Self::from_value(value)
    }

    /// Validate a parsed JSON value as a replay case.
    pub fn from_value(value: Value) -> Result<Self, ReplayError> {
        let obj = value
            .as_object()
            .ok_or_else(|| ReplayError::InvalidField("case must be a JSON object".into()))?;

        let schema_version = obj
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| ReplayError::InvalidField("missing schema_version".into()))?;
        if !matches!(schema_version, 1 | REPLAY_CASE_SCHEMA_VERSION) {
            return Err(ReplayError::UnsupportedVersion(schema_version));
        }

        let case: Self = serde_json::from_value(value)
            .map_err(|_| ReplayError::InvalidField("malformed case schema".into()))?;

        case.validate()?;
        Ok(case)
    }

    /// Validate invariants: option IDs, distributions, candidate integrity.
    pub fn validate(&self) -> Result<(), ReplayError> {
        if !matches!(self.schema_version, 1 | REPLAY_CASE_SCHEMA_VERSION) {
            return Err(ReplayError::UnsupportedVersion(self.schema_version));
        }
        if self.schema_version == 1 && self.frozen_inputs.is_some() {
            return Err(ReplayError::InvalidField(
                "frozen input semantics require replay case schema 2".into(),
            ));
        }
        match self.manifest.evidence_origin.as_str() {
            "recorded" | "synthetic" | "live" => {}
            _ => {
                return Err(ReplayError::InvalidField("unknown evidence origin".into()));
            }
        }

        if self.captured_request.candidate_options.len() > 10_000
            || self
                .captured_request
                .context_text
                .as_ref()
                .is_some_and(|s| s.len() > crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max())
            || self.captured_request.current_constraints.len() > 10_000
            || self
                .captured_request
                .current_constraints
                .iter()
                .any(|s| s.len() > crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max())
            || self
                .manifest
                .prompt_summary
                .as_ref()
                .is_some_and(|s| s.chars().count() > 120)
        {
            return Err(ReplayError::InvalidField(
                "captured field exceeds its bound".into(),
            ));
        }
        ReplayPolicy {
            gate_threshold: Some(self.local_evidence.scoring_profile.gate_threshold),
            fit_threshold: Some(self.local_evidence.scoring_profile.fit_threshold),
            w_fit: Some(self.local_evidence.scoring_profile.w_fit),
            w_prior: Some(self.local_evidence.scoring_profile.w_prior),
            w_phase: Some(self.local_evidence.scoring_profile.w_phase),
            top_k: Some(self.local_evidence.scoring_profile.top_k),
        }
        .validate()?;
        let mut candidate_ids = BTreeSet::new();
        for candidate in &self.captured_request.candidate_options {
            SkillId::new(&candidate.skill_id)
                .map_err(|_| ReplayError::InvalidField("invalid captured skill identity".into()))?;
            crate::identity::ContentHash::parse(&candidate.content_hash)
                .map_err(|_| ReplayError::InvalidField("invalid captured content digest".into()))?;
            if candidate.invocation_name.len() > 512
                || candidate.source.len() > 512
                || candidate.usage_kind.len() > 64
                || candidate
                    .description
                    .as_ref()
                    .is_some_and(|s| s.len() > 256 * 1024)
                || candidate
                    .excerpt
                    .as_ref()
                    .is_some_and(|s| s.len() > 256 * 1024)
            {
                return Err(ReplayError::InvalidField(
                    "captured candidate field exceeds its bound".into(),
                ));
            }

            if candidate.skill_id == "__none__" {
                return Err(ReplayError::InvalidField(
                    "__none__ sentinel cannot be a candidate skill ID".into(),
                ));
            }
            if !candidate_ids.insert(&candidate.skill_id) {
                return Err(ReplayError::OptionMapMismatch(
                    "duplicate candidate skill definition".into(),
                ));
            }
        }

        // Validate wide response if present
        if let Some(wide) = &self.recorded_responses.wide {
            if !(0.0..=1.0).contains(&wide.choices_probability)
                || wide.gate_score.is_some_and(|g| !(0.0..=1.0).contains(&g))
            {
                return Err(ReplayError::InvalidField(
                    "invalid recorded wide probability or gate".into(),
                ));
            }
            // The wide question offers every candidate.
            validate_distribution(&wide.distribution, &candidate_ids, true, &wide.choice)?;
            if wide.choice != "__none__" && !candidate_ids.contains(&wide.choice) {
                return Err(ReplayError::OptionMapMismatch(
                    "wide choice is not a captured candidate".into(),
                ));
            }
        }

        // Validate rerank response if present
        if let Some(rerank) = &self.recorded_responses.rerank {
            if !(0.0..=1.0).contains(&rerank.choices_probability)
                || rerank
                    .stated_confidence
                    .is_some_and(|c| !(0.0..=1.0).contains(&c))
            {
                return Err(ReplayError::InvalidField(
                    "invalid recorded rerank probability or confidence".into(),
                ));
            }
            // The rerank offers only the shortlist; its fits name the same set.
            validate_distribution(&rerank.distribution, &candidate_ids, false, &rerank.choice)?;
            if !rerank.fits.is_empty() {
                let offered: BTreeSet<&str> = rerank
                    .distribution
                    .iter()
                    .map(|d| d.option_id.as_str())
                    .filter(|id| *id != "__none__")
                    .collect();
                let fitted: BTreeSet<&str> =
                    rerank.fits.iter().map(|f| f.skill_id.as_str()).collect();
                if offered != fitted {
                    return Err(ReplayError::OptionMapMismatch(
                        "rerank fits and distribution name different shortlists".into(),
                    ));
                }
            }
            if rerank.choice != "__none__" && !candidate_ids.contains(&rerank.choice) {
                return Err(ReplayError::OptionMapMismatch(
                    "rerank choice is not a captured candidate".into(),
                ));
            }
            let mut fit_ids = BTreeSet::new();
            for fit in &rerank.fits {
                if !fit_ids.insert(fit.skill_id.as_str()) {
                    return Err(ReplayError::OptionMapMismatch(
                        "duplicate skill definition in rerank fits".into(),
                    ));
                }
                if !candidate_ids.contains(&fit.skill_id) {
                    return Err(ReplayError::OptionMapMismatch(
                        "fit references an unknown captured skill".into(),
                    ));
                }
                if !(0.0..=1.0).contains(&fit.fit) {
                    return Err(ReplayError::InvalidField(format!(
                        "fit score {} out of bounds [0, 1]",
                        fit.fit
                    )));
                }
            }
        }

        let mut snoozes = BTreeSet::new();
        for id in &self.local_evidence.active_snoozes {
            SkillId::new(id).map_err(|_| {
                ReplayError::InvalidField("invalid captured snooze identity".into())
            })?;
            if !snoozes.insert(id) {
                return Err(ReplayError::InvalidField(
                    "duplicate captured snooze definition".into(),
                ));
            }
        }
        let mut loads = BTreeSet::new();
        for load in &self.local_evidence.loaded_references {
            SkillId::new(&load.skill_id)
                .map_err(|_| ReplayError::InvalidField("invalid captured load identity".into()))?;
            crate::identity::ContentHash::parse(&load.content_hash)
                .map_err(|_| ReplayError::InvalidField("invalid captured load digest".into()))?;
            if !loads.insert((&load.skill_id, &load.content_hash))
                || !matches!(load.availability.as_str(), "available" | "unknown")
            {
                return Err(ReplayError::InvalidField(
                    "duplicate or invalid captured loaded definition".into(),
                ));
            }
        }
        if let Some(frozen) = &self.frozen_inputs {
            frozen.validate(self)?;
        }
        // Validate historical decision structure
        let hist_bytes = serde_json::to_vec(&self.historical_decision)
            .map_err(|e| ReplayError::InvalidJson(e.to_string()))?;
        OutputDocument::from_json(&hist_bytes)
            .map_err(|e| ReplayError::InvalidField(format!("invalid historical decision: {e}")))?;

        Ok(())
    }

    /// Save the replay case to the given path using atomic no-clobber export.
    pub fn save_to_file(&self, path: &Path) -> Result<(), ReplayError> {
        let mut writer = CaseWriter::new(DEFAULT_MAX_CASE_BYTES);
        serde_json::to_writer_pretty(&mut writer, self).map_err(|error| {
            if let Some(len) = writer.oversized {
                ReplayError::OversizedCase {
                    len,
                    max: writer.max_bytes,
                }
            } else if writer.allocation_failed {
                ReplayError::Io(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "case serialization allocation unavailable",
                ))
            } else {
                ReplayError::InvalidJson(error.to_string())
            }
        })?;
        export_private_atomic(path, &writer.bytes, ExportConfig::for_case())?;
        Ok(())
    }

    /// Load and validate a replay case from an owner-only file path.
    pub fn load_from_file(path: &Path) -> Result<Self, ReplayError> {
        let bytes =
            read_private_replay_file(path, "replay case", DEFAULT_MAX_CASE_BYTES, |len, max| {
                ReplayError::OversizedCase { len, max }
            })?;
        Self::from_json_bytes(&bytes)
    }
}

// Open once without following the final symlink or waiting for a FIFO writer.
// Permission/type checks and the bounded read all concern this same descriptor.
fn read_private_replay_file(
    path: &Path,
    label: &str,
    max: usize,
    oversized: fn(usize, usize) -> ReplayError,
) -> Result<Vec<u8>, ReplayError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(nix::libc::ELOOP) {
                ReplayError::InvalidField(format!("{label} path must be a regular file"))
            } else {
                ReplayError::Io(error)
            }
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ReplayError::InvalidField(format!(
            "{label} path must be a regular file"
        )));
    }
    let mode = metadata.mode() & 0o7777;
    if metadata.uid() != nix::unistd::geteuid().as_raw() || !matches!(mode, 0o600 | 0o400) {
        return Err(ReplayError::InvalidField(format!(
            "{label} file permissions must be owner-only (0600 or 0400)"
        )));
    }
    if metadata.len() > max as u64 {
        return Err(oversized(
            usize::try_from(metadata.len()).unwrap_or(usize::MAX),
            max,
        ));
    }
    // The extra byte detects growth after metadata inspection without permitting
    // an unbounded allocation. The limits are fixed small format constants.
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(oversized(bytes.len(), max));
    }
    Ok(bytes)
}

impl ReplayPolicy {
    /// Parse and validate a replay policy override document.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, ReplayError> {
        if bytes.len() > REPLAY_POLICY_BYTES.max() {
            return Err(ReplayError::OversizedPolicy {
                len: bytes.len(),
                max: REPLAY_POLICY_BYTES.max(),
            });
        }
        let value = parse_bounded_json(bytes, REPLAY_POLICY_DEPTH.max())?;
        let policy: Self = serde_json::from_value(value)
            .map_err(|_| ReplayError::InvalidField("malformed policy schema".into()))?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), ReplayError> {
        if let Some(gate) = self.gate_threshold
            && !(0.0..=1.0).contains(&gate)
        {
            return Err(ReplayError::InvalidField(format!(
                "gate threshold {gate} out of bounds [0, 1]"
            )));
        }
        if let Some(fit) = self.fit_threshold
            && !(0.0..=1.0).contains(&fit)
        {
            return Err(ReplayError::InvalidField(format!(
                "fit threshold {fit} out of bounds [0, 1]"
            )));
        }
        if let (Some(fit), Some(prior), Some(phase)) = (self.w_fit, self.w_prior, self.w_phase) {
            Weights::new(fit, prior, phase).map_err(|e| {
                ReplayError::InvalidField(format!("invalid weights combination: {e}"))
            })?;
        }
        if let Some(k) = self.top_k
            && (k == 0 || k > 32)
        {
            return Err(ReplayError::InvalidField(format!(
                "top_k {k} must be in range 1..=32"
            )));
        }
        Ok(())
    }

    /// Load and validate a replay policy override document from an owner-only file path.
    pub fn load_from_file(path: &Path) -> Result<Self, ReplayError> {
        let bytes = read_private_replay_file(
            path,
            "replay policy",
            REPLAY_POLICY_BYTES.max(),
            |len, max| ReplayError::OversizedPolicy { len, max },
        )?;
        if bytes.len() > REPLAY_POLICY_BYTES.max() {
            return Err(ReplayError::OversizedPolicy {
                len: bytes.len(),
                max: REPLAY_POLICY_BYTES.max(),
            });
        }
        if let Ok(policy) = Self::from_json_bytes(&bytes) {
            return Ok(policy);
        }
        if let Ok(text) = std::str::from_utf8(&bytes)
            && let Ok(table) = toml::from_str::<Self>(text)
        {
            table.validate()?;
            return Ok(table);
        }
        Self::from_json_bytes(&bytes)
    }
}

/// Execute replay evaluation with an optional baseline policy and an optional comparison policy.
pub fn execute_replay_comparison(
    case: &ReplayCase,
    policy: Option<&ReplayPolicy>,
    compare_policy: Option<&ReplayPolicy>,
) -> Result<ReplayOutcome, ReplayError> {
    if let Some(comp_pol) = compare_policy {
        let base_outcome = execute_replay(case, policy)?;
        let comp_outcome = execute_replay(case, Some(comp_pol))?;
        let mut envelope = base_outcome
            .document
            .as_value()
            .as_object()
            .ok_or_else(|| ReplayError::InvalidField("envelope must be an object".into()))?
            .clone();
        if let Some(comp_rec) = comp_outcome.document.as_value().get("recomputed") {
            envelope.insert("comparison".into(), comp_rec.clone());
            if let Some(base_rec) = envelope.get("recomputed") {
                let changes: Vec<&str> = [
                    "decision",
                    "reason",
                    "skills",
                    "omitted_rank_mass",
                    "needs_skill",
                    "phase",
                    "choice_confidence",
                    "none_probability",
                ]
                .into_iter()
                .filter(|key| base_rec[*key] != comp_rec[*key])
                .collect();
                envelope.insert("comparison_changes".into(), json!(changes));
            }
        }
        let complete = base_outcome.run_status == RunStatus::Complete
            && comp_outcome.run_status == RunStatus::Complete;
        if !complete {
            envelope.insert("run_status".into(), json!("partial"));
            envelope.insert("gate_status".into(), json!("not-established"));
            if let Some(c) = envelope
                .get_mut("completeness")
                .and_then(Value::as_object_mut)
            {
                c.insert("evidence_compatible".into(), json!(false));
                let comparison = &comp_outcome.document.as_value()["completeness"];
                let required = c
                    .get("stages_required")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .max(comparison["stages_required"].as_u64().unwrap_or(0));
                let completed = c
                    .get("stages_completed")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(comparison["stages_completed"].as_u64().unwrap_or(0));
                c.insert("stages_required".into(), json!(required));
                c.insert("stages_completed".into(), json!(completed));
            }
        }
        let doc_bytes = serde_json::to_vec(&Value::Object(envelope))
            .map_err(|e| ReplayError::InvalidJson(e.to_string()))?;
        let document = OutputDocument::from_json(&doc_bytes)
            .map_err(|e| ReplayError::InvalidField(format!("output validation failed: {e}")))?;
        Ok(ReplayOutcome {
            document,
            run_status: if base_outcome.run_status == RunStatus::Complete
                && comp_outcome.run_status == RunStatus::Complete
            {
                RunStatus::Complete
            } else {
                RunStatus::Partial
            },
            gate_status: if complete {
                base_outcome.gate_status
            } else {
                GateStatus::NotEstablished
            },
            historical_decision: base_outcome.historical_decision,
            recomputed_decision: base_outcome.recomputed_decision,
            explanation: base_outcome.explanation,
        })
    } else {
        execute_replay(case, policy)
    }
}

/// Execute replay evaluation over a case and optional policy override.
pub fn execute_replay(
    case: &ReplayCase,
    policy: Option<&ReplayPolicy>,
) -> Result<ReplayOutcome, ReplayError> {
    // `ReplayCase` has public fields, so a case built in memory need not have passed
    // through `from_json_bytes`. Validating here keeps scoring from reading an
    // unvalidated distribution, where a missing `__none__` counts as zero and every
    // candidate would beat none (sr-u66v).
    case.validate()?;
    if let Some(frozen) = &case.frozen_inputs
        && !frozen.computation_compatible()
    {
        return build_outcome(
            (case, policy),
            RunStatus::Partial,
            GateStatus::NotEstablished,
            case.historical_decision["decision"]
                .as_str()
                .unwrap_or("unavailable"),
            None,
            None,
            Some(
                "exact replay requires the recorded computation and tested numeric/build profile"
                    .into(),
            ),
        );
    }
    let hist_decision_str = case
        .historical_decision
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("unavailable")
        .to_string();

    // Check policy compatibility
    if let Some(pol) = policy {
        pol.validate()?;
        if case.frozen_inputs.is_none()
            && (pol.w_prior.is_some_and(|w| w > 0.0) || pol.w_phase.is_some_and(|w| w > 0.0))
        {
            return Err(ReplayError::IncompatiblePolicy(
                "turning on uncaptured prior or phase input is not replayable".into(),
            ));
        }
    }

    // Effective scoring profile
    let profile = &case.local_evidence.scoring_profile;
    let gate_threshold = policy
        .and_then(|p| p.gate_threshold)
        .unwrap_or(profile.gate_threshold);
    let fit_threshold = policy
        .and_then(|p| p.fit_threshold)
        .unwrap_or(profile.fit_threshold);
    let w_fit = policy.and_then(|p| p.w_fit).unwrap_or(profile.w_fit);
    let w_prior = policy.and_then(|p| p.w_prior).unwrap_or(profile.w_prior);
    let w_phase = policy.and_then(|p| p.w_phase).unwrap_or(profile.w_phase);
    let top_k = policy.and_then(|p| p.top_k).unwrap_or(profile.top_k);

    let weights = Weights::new(w_fit, w_prior, w_phase)
        .map_err(|e| ReplayError::InvalidField(format!("invalid weights: {e}")))?;

    // Step 1: Check if historical run was an explicit request or local abstention
    if hist_decision_str == "explicit" {
        // Explicit resolution bypasses inference and weights
        let recomputed = case.historical_decision.clone();
        return build_outcome(
            (case, policy),
            RunStatus::Complete,
            GateStatus::NotApplicable,
            &hist_decision_str,
            Some("explicit"),
            Some(recomputed),
            None,
        );
    }

    if hist_decision_str == "unavailable" {
        return build_outcome((case, policy), RunStatus::Complete, GateStatus::NotApplicable,
            &hist_decision_str, Some("unavailable"), Some(case.historical_decision.clone()),
            Some("reproduced terminal metadata only; no unobserved provider answer was reconstructed".into()));
    }

    // Step 2: Wide Gate evaluation
    if let Some(wide) = &case.recorded_responses.wide {
        let Some(gate_score) = wide.gate_score else {
            return build_outcome(
                (case, policy),
                RunStatus::Partial,
                GateStatus::NotEstablished,
                &hist_decision_str,
                None,
                None,
                Some("missing recorded wide heuristic; none probability cannot replace it".into()),
            );
        };

        if gate_score < gate_threshold {
            let recomputed = make_recomputed_abstain(case, "low-need");
            let gate_status = replay_parity_status(case, &recomputed, policy);
            return build_outcome(
                (case, policy),
                RunStatus::Complete,
                gate_status,
                &hist_decision_str,
                Some("abstain"),
                Some(recomputed),
                Some("recomputed decision abstained at wide gate threshold".into()),
            );
        }

        // Gate passed. Check if rerank response is available.
        let rerank = match &case.recorded_responses.rerank {
            Some(r) => r,
            None => {
                // If historical was low-gate abstention without rerank, and policy lowered the gate,
                // rerank is missing: report as partial and not estimable.
                return build_outcome(
                    (case, policy),
                    RunStatus::Partial,
                    GateStatus::NotEstablished,
                    &hist_decision_str,
                    None,
                    None,
                    Some(
                        "missing recorded rerank response for candidate fit evaluation at lowered gate"
                            .into(),
                    ),
                );
            }
        };

        // A case captured before `stated_confidence` existed cannot reproduce the
        // decision's `choice_confidence`, and a ranked decision must carry one.
        // Report that honestly instead of recomputing a ranking whose confidence
        // is either absent or a different quantity wearing the same name.
        if rerank.stated_confidence.is_none() {
            return build_outcome(
                (case, policy),
                RunStatus::Partial,
                GateStatus::NotEstablished,
                &hist_decision_str,
                None,
                None,
                Some(
                    "case predates recorded provider confidence; recomputing a ranked decision would have to invent it"
                        .into(),
                ),
            );
        }

        // Step 3: Candidate eligibility on shortlist
        let snoozes: BTreeSet<&str> = case
            .local_evidence
            .active_snoozes
            .iter()
            .map(|s| s.as_str())
            .collect();
        let loaded: BTreeSet<(&str, &str)> = case
            .local_evidence
            .loaded_references
            .iter()
            .filter(|r| r.availability == "available")
            .map(|r| (r.skill_id.as_str(), r.content_hash.as_str()))
            .collect();

        let none_rerank_prob = rerank
            .distribution
            .iter()
            .find(|d| d.option_id == "__none__")
            .map(|d| d.probability)
            .unwrap_or(0.0);

        let fits_by_id: BTreeMap<&str, f64> = rerank
            .fits
            .iter()
            .map(|f| (f.skill_id.as_str(), f.fit))
            .collect();

        let probs_by_id: BTreeMap<&str, f64> = rerank
            .distribution
            .iter()
            .map(|d| (d.option_id.as_str(), d.probability))
            .collect();

        struct EligibleCandidate<'a> {
            skill_id: &'a str,
            invocation_name: &'a str,
            content_hash: &'a str,
            visibility: Option<&'a str>,
            rerank_prob: f64,
            fit: f64,
        }

        let mut eligible: Vec<EligibleCandidate<'_>> = Vec::new();
        for candidate in &case.captured_request.candidate_options {
            let id = candidate.skill_id.as_str();
            let matching_reference = candidate.usage_kind == "reference"
                && loaded.contains(&(id, candidate.content_hash.as_str()));
            let allowed = case.frozen_inputs.as_ref().map(|f| {
                f.visible_roster
                    .iter()
                    .find(|m| m.skill_id == id)
                    .is_some_and(|m| m.pre_fit_eligible)
            });
            if allowed == Some(false)
                || (allowed.is_none() && (snoozes.contains(id) || matching_reference))
            {
                continue;
            }
            let fit = fits_by_id.get(id).copied().unwrap_or(0.0);
            if fit < fit_threshold {
                continue;
            }
            let prob = probs_by_id.get(id).copied().unwrap_or(0.0);
            // Each candidate must individually beat __none__
            if prob <= none_rerank_prob {
                continue;
            }
            eligible.push(EligibleCandidate {
                skill_id: id,
                invocation_name: &candidate.invocation_name,
                content_hash: &candidate.content_hash,
                visibility: candidate.visibility.as_deref(),
                rerank_prob: prob,
                fit,
            });
        }

        if eligible.is_empty() {
            let any_fitting = case.captured_request.candidate_options.iter().any(|c| {
                fits_by_id
                    .get(c.skill_id.as_str())
                    .is_some_and(|fit| *fit >= fit_threshold)
            });
            let recomputed = make_recomputed_abstain(
                case,
                if any_fitting {
                    "no-shortlist-match"
                } else {
                    "low-fit"
                },
            );
            let gate_status = replay_parity_status(case, &recomputed, policy);
            return build_outcome(
                (case, policy),
                RunStatus::Complete,
                gate_status,
                &hist_decision_str,
                Some("abstain"),
                Some(recomputed),
                Some("no shortlist candidate beat none or satisfied minimum fit".into()),
            );
        }

        // Step 4: Score eligible candidates using rank
        let parsed_skill_ids: Vec<SkillId> = eligible
            .iter()
            .map(|c| SkillId::new(c.skill_id).unwrap())
            .collect();
        let scoring_inputs: Vec<Input<'_>> = eligible
            .iter()
            .enumerate()
            .map(|(i, c)| Input {
                id: &parsed_skill_ids[i],
                rerank: c.rerank_prob,
                fit: c.fit,
                prior_delta: case
                    .frozen_inputs
                    .as_ref()
                    .and_then(|f| f.numeric_inputs.iter().find(|n| n.skill_id == c.skill_id))
                    .map_or(0.0, |n| n.prior_delta),
                phase_match: case
                    .frozen_inputs
                    .as_ref()
                    .and_then(|f| f.numeric_inputs.iter().find(|n| n.skill_id == c.skill_id))
                    .map_or(0.0, |n| n.phase_match),
            })
            .collect();

        let scored = rank(&scoring_inputs, weights, top_k)
            .map_err(|e| ReplayError::InvalidField(format!("scoring failed: {e}")))?;

        let wide_probs_by_id: BTreeMap<&str, f64> = case
            .recorded_responses
            .wide
            .as_ref()
            .map(|w| {
                w.distribution
                    .iter()
                    .map(|d| (d.option_id.as_str(), d.probability))
                    .collect()
            })
            .unwrap_or_default();

        let mut ranked_skills = Vec::new();
        for (rank_idx, s) in scored.returned.iter().enumerate() {
            let candidate = &eligible[s.index];
            let wide_prob = wide_probs_by_id
                .get(candidate.skill_id)
                .copied()
                .unwrap_or(candidate.rerank_prob);
            ranked_skills.push(json!({
                "rank": rank_idx + 1,
                "skill_id": candidate.skill_id,
                "name": candidate.invocation_name,
                "invocation_name": candidate.invocation_name,
                "rank_score": s.rank_score,
                "rerank_probability": candidate.rerank_prob,
                "wide_probability": wide_prob,
                "fits": candidate.fit,
                // Null, not a guess. A case does not record where a skill
                // lived, and `.claude/skills/<name>/SKILL.md` would state a
                // location never observed.
                "path": Value::Null,
                "content_hash": candidate.content_hash,
            }));
            if let Some(visibility) = candidate.visibility
                && let Some(entry) = ranked_skills.last_mut().and_then(Value::as_object_mut)
            {
                entry.insert("visibility".into(), Value::from(visibility));
            }
        }

        let recomputed =
            make_recomputed_ranked(case, ranked_skills, scored.omitted_mass, none_rerank_prob);

        let gate_status = replay_parity_status(case, &recomputed, policy);

        return build_outcome(
            (case, policy),
            RunStatus::Complete,
            gate_status,
            &hist_decision_str,
            Some("ranked"),
            Some(recomputed),
            None,
        );
    }

    if hist_decision_str == "ranked"
        || case.recorded_responses.rerank.is_some()
        || !case.manifest.stages_recorded.is_empty()
        || case.historical_decision["needs_skill"].is_number()
        || case
            .frozen_inputs
            .as_ref()
            .is_some_and(|frozen| !frozen.stages.is_empty())
    {
        return build_outcome((case, policy), RunStatus::Partial, GateStatus::NotEstablished,
            &hist_decision_str, None, None, Some("missing recorded wide response; historical metadata does not reconstruct inference".into()));
    }

    // If neither wide nor explicit was captured, replay the historical decision as-is
    let recomputed = case.historical_decision.clone();
    build_outcome(
        (case, policy),
        RunStatus::Complete,
        GateStatus::NotApplicable,
        &hist_decision_str,
        Some(&hist_decision_str),
        Some(recomputed),
        None,
    )
}

fn build_outcome(
    context: (&ReplayCase, Option<&ReplayPolicy>),
    run_status: RunStatus,
    gate_status: GateStatus,
    hist_decision: &str,
    recomputed_decision: Option<&str>,
    recomputed_value: Option<Value>,
    explanation: Option<String>,
) -> Result<ReplayOutcome, ReplayError> {
    let (case, policy) = context;
    // Count actual received responses independently of input/profile compatibility.
    // A lowered gate can require a stage that the original invocation never started.
    let stages_comp = if hist_decision == "unavailable" {
        // Terminal metadata replay evaluates no provider stages. Availability of
        // attempted inputs/responses remains visible in input_completeness.
        0
    } else {
        usize::from(case.recorded_responses.wide.is_some())
            + usize::from(case.recorded_responses.rerank.is_some())
    };
    let mut stages_req = case
        .frozen_inputs
        .as_ref()
        .map_or(0, |f| f.stages.len())
        .max(stages_comp);
    let threshold = policy
        .and_then(|p| p.gate_threshold)
        .unwrap_or(case.local_evidence.scoring_profile.gate_threshold);
    if hist_decision == "unavailable" {
        stages_req = 0;
    } else if case.recorded_responses.rerank.is_some()
        || case
            .recorded_responses
            .wide
            .as_ref()
            .and_then(|w| w.gate_score)
            .map_or(hist_decision == "ranked", |gate| gate >= threshold)
    {
        stages_req = stages_req.max(2);
    } else if !case.manifest.stages_recorded.is_empty()
        || case.historical_decision["needs_skill"].is_number()
    {
        stages_req = stages_req.max(1);
    }

    let mut envelope = Map::new();
    envelope.insert("schema_version".into(), json!(SCHEMA_VERSION));
    envelope.insert("kind".into(), json!("replay"));
    envelope.insert("actionable".into(), json!(false));
    envelope.insert(
        "run_status".into(),
        json!(match run_status {
            RunStatus::Complete => "complete",
            RunStatus::Partial => "partial",
        }),
    );
    envelope.insert(
        "gate_status".into(),
        json!(match gate_status {
            GateStatus::Passed => "passed",
            GateStatus::Failed => "failed",
            GateStatus::NotEstablished => "not-established",
            GateStatus::NotApplicable => "not-applicable",
        }),
    );
    envelope.insert(
        "evidence_origin".into(),
        json!(&case.manifest.evidence_origin),
    );
    envelope.insert(
        "completeness".into(),
        json!({
            "cases_requested": 1,
            "cases_completed": 1,
            "stages_required": stages_req,
            "stages_completed": stages_comp,
            // A run that could not recompute did not have compatible evidence.
            // Reporting `true` beside a null recomputation told a reader the
            // artifact was sufficient when it demonstrably was not.
            "evidence_compatible": run_status == RunStatus::Complete && case.frozen_inputs.as_ref().is_some_and(FrozenReplayInputs::exact_compatible) && hist_decision != "unavailable"
        }),
    );
    let frozen = case.frozen_inputs.as_ref();
    envelope.insert(
        "input_completeness".into(),
        json!({
            "frozen_format": frozen.map(|f| f.format_version),
            "visible_roster": frozen.is_some_and(|f| f.roster_complete),
            "local_eligibility": frozen.is_some(),
            "numeric_inputs": frozen.is_some(),
            "computation_profile": frozen.is_some_and(FrozenReplayInputs::computation_compatible),
            "stages": frozen.map(|f| f.stages.iter().map(|s| json!({
                "stage": s.stage, "request": true, "option_map": true,
                "response": s.response_json.is_some(), "exact_inputs": !s.privacy_transformed
            })).collect::<Vec<_>>()).unwrap_or_default()
        }),
    );
    if let Some(note) = &explanation {
        envelope.insert("replay_note".into(), Value::from(note.clone()));
    }
    envelope.insert("historical".into(), case.historical_decision.clone());
    if let Some(rec) = recomputed_value {
        envelope.insert("recomputed".into(), rec);
    }

    let doc_bytes = serde_json::to_vec(&Value::Object(envelope))
        .map_err(|e| ReplayError::InvalidJson(e.to_string()))?;
    let document = OutputDocument::from_json(&doc_bytes)
        .map_err(|e| ReplayError::InvalidField(format!("output validation failed: {e}")))?;

    Ok(ReplayOutcome {
        document,
        run_status,
        gate_status,
        historical_decision: hist_decision.to_string(),
        recomputed_decision: recomputed_decision.map(ToString::to_string),
        explanation,
    })
}

fn make_recomputed_abstain(case: &ReplayCase, reason: &str) -> Value {
    let mut recomputed = case.historical_decision.clone();
    recomputed["event_id"] = Value::from(format!("replay-{}", case.case_id));
    recomputed["decision"] = Value::from("abstain");
    recomputed["reason"] = Value::from(reason);
    recomputed["skills"] = Value::Array(Vec::new());
    recomputed["omitted_rank_mass"] = Value::Null;
    recomputed["needs_skill"] = Value::Null;
    recomputed["choice_confidence"] = Value::Null;
    recomputed["none_probability"] = Value::Null;
    recomputed["phase"] = Value::Null;
    if case.frozen_inputs.is_some() {
        recomputed["needs_skill"] = case
            .recorded_responses
            .wide
            .as_ref()
            .and_then(|w| w.gate_score)
            .map_or(Value::Null, Value::from);
        // Phase is frozen in the full recorded wide response and historical output.
        recomputed["phase"] = case
            .frozen_inputs
            .as_ref()
            .and_then(FrozenReplayInputs::phase)
            .map_or(Value::Null, Value::from);
        if reason != "low-need" {
            recomputed["choice_confidence"] = case
                .recorded_responses
                .rerank
                .as_ref()
                .and_then(|r| r.stated_confidence)
                .map_or(Value::Null, Value::from);
            recomputed["none_probability"] = case
                .recorded_responses
                .rerank
                .as_ref()
                .and_then(|r| r.distribution.iter().find(|d| d.option_id == "__none__"))
                .map_or(Value::Null, |d| Value::from(d.probability));
        }
    }

    if case.frozen_inputs.is_none()
        && let Some(roster) = recomputed.get_mut("roster").and_then(Value::as_object_mut)
    {
        roster.insert("wide_candidates".into(), Value::from(0));
        roster.insert("shortlist".into(), Value::from(0));
        roster.insert("retrieval".into(), Value::from("not-evaluated"));
        if let Some(provenance) = roster.get_mut("provenance").and_then(Value::as_object_mut) {
            provenance.insert("wide_set_id".into(), Value::Null);
            provenance.insert("rerank_set_id".into(), Value::Null);
        }
    }
    recomputed
}

fn make_recomputed_ranked(
    case: &ReplayCase,
    ranked_skills: Vec<Value>,
    omitted_mass: f64,
    none_prob: f64,
) -> Value {
    let mut recomputed = case.historical_decision.clone();
    recomputed["event_id"] = Value::from(format!("replay-{}", case.case_id));
    recomputed["decision"] = Value::from("ranked");
    recomputed["reason"] = Value::from("eligible-candidates");
    if let Some(frozen) = &case.frozen_inputs {
        recomputed["phase"] = frozen.phase().map_or(Value::Null, Value::from);
        recomputed["needs_skill"] = case
            .recorded_responses
            .wide
            .as_ref()
            .and_then(|w| w.gate_score)
            .map_or(Value::Null, Value::from);
    }
    let returned_count = ranked_skills.len();
    recomputed["skills"] = Value::Array(ranked_skills);
    recomputed["omitted_rank_mass"] = Value::from(omitted_mass);
    recomputed["none_probability"] = Value::from(none_prob);
    // `choice_confidence` is the provider's stated confidence, not the chosen
    // option's probability. Substituting one for the other made every
    // historical-versus-recomputed comparison show a difference that no policy
    // change caused, which is precisely the signal replay exists to give.
    recomputed["choice_confidence"] = case
        .recorded_responses
        .rerank
        .as_ref()
        .and_then(|rerank| rerank.stated_confidence)
        .map_or(Value::Null, Value::from);
    if case.frozen_inputs.is_none()
        && let Some(roster) = recomputed.get_mut("roster").and_then(Value::as_object_mut)
    {
        let wide_count = case.captured_request.candidate_options.len() as u64;
        let shortlist_count = case
            .recorded_responses
            .rerank
            .as_ref()
            .map(|r| r.fits.len() as u64)
            .unwrap_or(returned_count as u64)
            .max(returned_count as u64);

        let total = wide_count.max(1);
        let eligible = wide_count.max(1);
        let wide = wide_count.max(1);
        let shortlist = shortlist_count.min(wide).max(returned_count as u64);

        roster.insert("total".into(), Value::from(total));
        roster.insert("eligible".into(), Value::from(eligible));
        roster.insert("wide_candidates".into(), Value::from(wide));
        roster.insert("shortlist".into(), Value::from(shortlist));
        roster.insert("retrieval".into(), Value::from("full"));
    }
    recomputed
}

/// Checks a recorded distribution the way the live codec checks a live one, so
/// replay cannot accept an answer a live run would refuse. `__none__` must be
/// present: a missing sentinel would read as probability zero and let every
/// candidate beat it. With `require_all`, every allowed option must be present
/// too. The sum must be positive and within the codec's tolerance, and
/// `choice` must be a maximum-probability option (ties allowed).
fn validate_distribution(
    dist: &[ChoiceDistributionItem],
    candidate_ids: &BTreeSet<&String>,
    require_all: bool,
    choice: &str,
) -> Result<(), ReplayError> {
    if dist.is_empty() {
        return Err(ReplayError::InvalidField(
            "distribution must not be empty".into(),
        ));
    }
    let mut sum = 0.0;
    let mut maximum = f64::NEG_INFINITY;
    let mut choice_probability = None;
    let mut seen = BTreeSet::new();
    for item in dist {
        if !seen.insert(&item.option_id) {
            return Err(ReplayError::OptionMapMismatch(
                "duplicate distribution option definition".into(),
            ));
        }
        if item.option_id != "__none__" && !candidate_ids.contains(&item.option_id) {
            return Err(ReplayError::OptionMapMismatch(
                "distribution contains a foreign option".into(),
            ));
        }
        if !item.probability.is_finite() || item.probability < 0.0 || item.probability > 1.0 {
            return Err(ReplayError::InvalidField(format!(
                "invalid distribution probability: {}",
                item.probability
            )));
        }
        sum += item.probability;
        maximum = maximum.max(item.probability);
        if item.option_id == choice {
            choice_probability = Some(item.probability);
        }
    }
    if !seen.iter().any(|id| id.as_str() == "__none__") {
        return Err(ReplayError::OptionMapMismatch(
            "distribution must include __none__".into(),
        ));
    }
    if require_all && candidate_ids.iter().any(|id| !seen.contains(id)) {
        return Err(ReplayError::OptionMapMismatch(
            "distribution is missing a candidate option".into(),
        ));
    }
    let tolerance = crate::jev::codec::SUM_TOLERANCE;
    if sum <= 0.0 || (sum - 1.0).abs() > tolerance {
        return Err(ReplayError::InvalidField(format!(
            "distribution probabilities must sum to 1.0 ± {tolerance}, got {sum}"
        )));
    }
    if choice_probability != Some(maximum) {
        return Err(ReplayError::InvalidField(
            "choice is not a maximum-probability option".into(),
        ));
    }
    Ok(())
}

fn parse_bounded_json(bytes: &[u8], max_depth: usize) -> Result<Value, ReplayError> {
    use serde::de::DeserializeSeed;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = crate::output::JsonSeed(0)
        .deserialize(&mut deserializer)
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("duplicate JSON key") {
                ReplayError::DuplicateKey("replay definition".into())
            } else if message.contains("JSON depth limit") {
                ReplayError::ExcessiveDepth
            } else {
                ReplayError::InvalidJson("syntax or value".into())
            }
        })?;
    deserializer
        .end()
        .map_err(|_| ReplayError::InvalidJson("trailing content".into()))?;
    fn depth(value: &Value, current: usize, max: usize) -> Result<(), ReplayError> {
        if current > max {
            return Err(ReplayError::ExcessiveDepth);
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    depth(item, current + 1, max)?;
                }
            }
            Value::Object(items) => {
                for item in items.values() {
                    depth(item, current + 1, max)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    depth(&value, 1, max_depth)?;
    Ok(value)
}

fn replay_parity_status(
    case: &ReplayCase,
    recomputed: &Value,
    policy: Option<&ReplayPolicy>,
) -> GateStatus {
    if case.manifest.evidence_origin == "synthetic" {
        return GateStatus::NotApplicable;
    }
    if policy.is_some()
        || !case
            .frozen_inputs
            .as_ref()
            .is_some_and(FrozenReplayInputs::exact_compatible)
    {
        return GateStatus::NotEstablished;
    }
    let keys = [
        "decision",
        "reason",
        "needs_skill",
        "phase",
        "none_probability",
        "choice_confidence",
        "omitted_rank_mass",
    ];
    let equal = keys
        .iter()
        .all(|k| recomputed[*k] == case.historical_decision[*k])
        && replay_skill_values(recomputed) == replay_skill_values(&case.historical_decision);
    if equal {
        GateStatus::Passed
    } else {
        GateStatus::Failed
    }
}
fn replay_skill_values(value: &Value) -> Vec<Value> {
    value["skills"]
        .as_array()
        .map(|skills| {
            skills
                .iter()
                .map(|skill| {
                    let mut object = Map::new();
                    for key in [
                        "rank",
                        "skill_id",
                        "invocation_name",
                        "content_hash",
                        "visibility",
                        "rank_score",
                        "wide_probability",
                        "rerank_probability",
                        "fits",
                    ] {
                        object.insert(key.into(), skill.get(key).cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(object)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod case_serialization_tests {
    use super::CaseWriter;

    #[test]
    fn escaped_json_must_fit_before_buffer_growth_and_keeps_a_success_counterpart() {
        let input = "\"".repeat(10);
        let expected = b"\"\\\"\\\"\\\"\\\"\\\"\\\"\\\"\\\"\\\"\\\"\"";
        assert!(input.len() < 16);
        assert!(expected.len() > 16);
        let mut too_small = CaseWriter::new(16);
        assert!(serde_json::to_writer_pretty(&mut too_small, &input).is_err());
        assert!(too_small.oversized.is_some_and(|len| len > 16));
        assert!(too_small.bytes.len() <= 16);
        assert!(too_small.bytes.capacity() <= 16);

        let mut enough = CaseWriter::new(expected.len());
        serde_json::to_writer_pretty(&mut enough, &input).unwrap();
        assert_eq!(enough.bytes, expected);
        assert!(enough.bytes.capacity() <= expected.len());
    }
}
