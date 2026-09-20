//! Bounded, offline replay and policy comparison (P5).
//!
//! Replay evaluates recorded or synthetic cases without network, child processes,
//! transcript discovery, skill execution, or state writes. Cases and policy overrides
//! are strictly bounded, owner-only, and validated before evaluation.

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
            Self::InvalidJson(msg) => write!(f, "invalid replay JSON: {msg}"),
            Self::DuplicateKey(key) => write!(f, "duplicate JSON key in replay input: {key}"),
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

/// A complete captured replay case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayCase {
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
pub struct CapturedRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_text: Option<String>,
    #[serde(default)]
    pub current_constraints: Vec<String>,
    pub candidate_options: Vec<CapturedCandidate>,
}

/// Candidate skill metadata captured at the time of the decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapturedCandidate {
    pub skill_id: String,
    pub invocation_name: String,
    pub content_hash: String,
    pub source: String,
    pub usage_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// Validated recorded provider responses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordedResponses {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wide: Option<RecordedWideChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<RecordedRerankChoice>,
}

/// Recorded wide-choice provider response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedWideChoice {
    pub choice: String,
    pub choices_probability: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_score: Option<f64>,
    pub distribution: Vec<ChoiceDistributionItem>,
}

/// Recorded rerank-choice provider response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedRerankChoice {
    pub choice: String,
    pub choices_probability: f64,
    #[serde(default)]
    pub fits: Vec<CandidateFitItem>,
    pub distribution: Vec<ChoiceDistributionItem>,
}

/// One candidate option's probability in a provider distribution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChoiceDistributionItem {
    pub option_id: String,
    pub probability: f64,
}

/// One candidate's fit score from question evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateFitItem {
    pub skill_id: String,
    pub fit: f64,
}

/// Local state and evidence frozen at the time of the decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
pub struct CapturedLoadedReference {
    pub skill_id: String,
    pub content_hash: String,
    pub availability: String,
}

/// Scoring parameters frozen at decision time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
        if schema_version != SCHEMA_VERSION {
            return Err(ReplayError::UnsupportedVersion(schema_version));
        }

        let case: Self = serde_json::from_value(value)
            .map_err(|e| ReplayError::InvalidField(format!("malformed case schema: {e}")))?;

        case.validate()?;
        Ok(case)
    }

    /// Validate invariants: option IDs, distributions, candidate integrity.
    pub fn validate(&self) -> Result<(), ReplayError> {
        match self.manifest.evidence_origin.as_str() {
            "recorded" | "synthetic" | "live" => {}
            other => {
                return Err(ReplayError::InvalidField(format!(
                    "unknown evidence origin: {other}"
                )));
            }
        }

        let mut candidate_ids = BTreeSet::new();
        for candidate in &self.captured_request.candidate_options {
            if candidate.skill_id == "__none__" {
                return Err(ReplayError::InvalidField(
                    "__none__ sentinel cannot be a candidate skill ID".into(),
                ));
            }
            if !candidate_ids.insert(&candidate.skill_id) {
                return Err(ReplayError::OptionMapMismatch(format!(
                    "duplicate candidate skill ID: {}",
                    candidate.skill_id
                )));
            }
        }

        // Validate wide response if present
        if let Some(wide) = &self.recorded_responses.wide {
            validate_distribution(&wide.distribution, &candidate_ids)?;
            if wide.choice != "__none__" && !candidate_ids.contains(&wide.choice) {
                return Err(ReplayError::OptionMapMismatch(format!(
                    "wide choice {} not found in candidate options",
                    wide.choice
                )));
            }
        }

        // Validate rerank response if present
        if let Some(rerank) = &self.recorded_responses.rerank {
            validate_distribution(&rerank.distribution, &candidate_ids)?;
            if rerank.choice != "__none__" && !candidate_ids.contains(&rerank.choice) {
                return Err(ReplayError::OptionMapMismatch(format!(
                    "rerank choice {} not found in candidate options",
                    rerank.choice
                )));
            }
            for fit in &rerank.fits {
                if !candidate_ids.contains(&fit.skill_id) {
                    return Err(ReplayError::OptionMapMismatch(format!(
                        "fit for unknown skill ID: {}",
                        fit.skill_id
                    )));
                }
                if !(0.0..=1.0).contains(&fit.fit) {
                    return Err(ReplayError::InvalidField(format!(
                        "fit score {} out of bounds [0, 1]",
                        fit.fit
                    )));
                }
            }
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
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| ReplayError::InvalidJson(e.to_string()))?;
        export_private_atomic(path, content.as_bytes(), ExportConfig::for_case())?;
        Ok(())
    }

    /// Load and validate a replay case from an owner-only file path.
    pub fn load_from_file(path: &Path) -> Result<Self, ReplayError> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            return Err(ReplayError::InvalidField(
                "replay case path must be a regular file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = metadata.mode();
            let uid = nix::unistd::geteuid().as_raw();
            if metadata.uid() != uid || (mode & 0o7777 != 0o600 && mode & 0o7777 != 0o400) {
                return Err(ReplayError::InvalidField(
                    "replay case file permissions must be owner-only (0600 or 0400)".into(),
                ));
            }
        }
        let bytes = std::fs::read(path)?;
        Self::from_json_bytes(&bytes)
    }
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
            .map_err(|e| ReplayError::InvalidField(format!("malformed policy schema: {e}")))?;
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
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            return Err(ReplayError::InvalidField(
                "replay policy path must be a regular file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = metadata.mode();
            let uid = nix::unistd::geteuid().as_raw();
            if metadata.uid() != uid || (mode & 0o7777 != 0o600 && mode & 0o7777 != 0o400) {
                return Err(ReplayError::InvalidField(
                    "replay policy file permissions must be owner-only (0600 or 0400)".into(),
                ));
            }
        }
        let bytes = std::fs::read(path)?;
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
            gate_status: base_outcome.gate_status,
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
    let hist_decision_str = case
        .historical_decision
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("unavailable")
        .to_string();

    // Check policy compatibility
    if let Some(pol) = policy {
        pol.validate()?;
        if pol.w_prior.is_some_and(|w| w > 0.0) {
            return Err(ReplayError::IncompatiblePolicy(
                "turning on uncaptured prior is not replayable".into(),
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
            case,
            RunStatus::Complete,
            GateStatus::NotApplicable,
            &hist_decision_str,
            Some("explicit"),
            Some(recomputed),
            None,
        );
    }

    // Step 2: Wide Gate evaluation
    if let Some(wide) = &case.recorded_responses.wide {
        let gate_score = wide.gate_score.unwrap_or_else(|| {
            // Default gate heuristic: 1.0 - none Choice probability
            let none_prob = wide
                .distribution
                .iter()
                .find(|d| d.option_id == "__none__")
                .map(|d| d.probability)
                .unwrap_or(0.0);
            (1.0 - none_prob).clamp(0.0, 1.0)
        });

        if gate_score < gate_threshold {
            let recomputed = make_recomputed_abstain(case, "low-fit");
            let gate_status = if case.manifest.evidence_origin == "synthetic" {
                GateStatus::NotApplicable
            } else if hist_decision_str == "abstain" {
                GateStatus::Passed
            } else {
                GateStatus::NotEstablished
            };
            return build_outcome(
                case,
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
                    case,
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

        // Step 3: Candidate eligibility on shortlist
        let snoozes: BTreeSet<&str> = case
            .local_evidence
            .active_snoozes
            .iter()
            .map(|s| s.as_str())
            .collect();
        let loaded: BTreeSet<&str> = case
            .local_evidence
            .loaded_references
            .iter()
            .map(|r| r.skill_id.as_str())
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
            rerank_prob: f64,
            fit: f64,
        }

        let mut eligible: Vec<EligibleCandidate<'_>> = Vec::new();
        for candidate in &case.captured_request.candidate_options {
            let id = candidate.skill_id.as_str();
            if snoozes.contains(id) || loaded.contains(id) {
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
                rerank_prob: prob,
                fit,
            });
        }

        if eligible.is_empty() {
            let recomputed = make_recomputed_abstain(case, "no-shortlist-match");
            let gate_status = if case.manifest.evidence_origin == "synthetic" {
                GateStatus::NotApplicable
            } else if hist_decision_str == "abstain" {
                GateStatus::Passed
            } else {
                GateStatus::NotEstablished
            };
            return build_outcome(
                case,
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
                prior_delta: 0.0,
                phase_match: 0.0,
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
                "path": format!(".claude/skills/{}/SKILL.md", candidate.invocation_name),
                "content_hash": candidate.content_hash,
            }));
        }

        let recomputed =
            make_recomputed_ranked(case, ranked_skills, scored.omitted_mass, none_rerank_prob);

        let gate_status = if case.manifest.evidence_origin == "synthetic" {
            GateStatus::NotApplicable
        } else if hist_decision_str == "ranked" {
            GateStatus::Passed
        } else {
            GateStatus::NotEstablished
        };

        return build_outcome(
            case,
            RunStatus::Complete,
            gate_status,
            &hist_decision_str,
            Some("ranked"),
            Some(recomputed),
            None,
        );
    }

    // If neither wide nor explicit was captured, replay the historical decision as-is
    let recomputed = case.historical_decision.clone();
    build_outcome(
        case,
        RunStatus::Complete,
        GateStatus::NotApplicable,
        &hist_decision_str,
        Some(&hist_decision_str),
        Some(recomputed),
        None,
    )
}

fn build_outcome(
    case: &ReplayCase,
    run_status: RunStatus,
    gate_status: GateStatus,
    hist_decision: &str,
    recomputed_decision: Option<&str>,
    recomputed_value: Option<Value>,
    explanation: Option<String>,
) -> Result<ReplayOutcome, ReplayError> {
    let stages_req = case.manifest.stages_recorded.len().max(1);
    let stages_comp = if run_status == RunStatus::Complete {
        stages_req
    } else {
        case.manifest.stages_recorded.len().saturating_sub(1)
    };

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
            "evidence_compatible": true
        }),
    );
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
    if let Some(roster) = recomputed.get_mut("roster").and_then(Value::as_object_mut) {
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
    let returned_count = ranked_skills.len();
    recomputed["skills"] = Value::Array(ranked_skills);
    recomputed["omitted_rank_mass"] = Value::from(omitted_mass);
    recomputed["none_probability"] = Value::from(none_prob);
    if let Some(rerank) = &case.recorded_responses.rerank {
        recomputed["choice_confidence"] = Value::from(rerank.choices_probability);
    }
    if let Some(roster) = recomputed.get_mut("roster").and_then(Value::as_object_mut) {
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

fn validate_distribution(
    dist: &[ChoiceDistributionItem],
    candidate_ids: &BTreeSet<&String>,
) -> Result<(), ReplayError> {
    if dist.is_empty() {
        return Err(ReplayError::InvalidField(
            "distribution must not be empty".into(),
        ));
    }
    let mut sum = 0.0;
    let mut seen = BTreeSet::new();
    for item in dist {
        if !seen.insert(&item.option_id) {
            return Err(ReplayError::OptionMapMismatch(format!(
                "duplicate option ID in distribution: {}",
                item.option_id
            )));
        }
        if item.option_id != "__none__" && !candidate_ids.contains(&item.option_id) {
            return Err(ReplayError::OptionMapMismatch(format!(
                "distribution option ID {} not in candidates",
                item.option_id
            )));
        }
        if !item.probability.is_finite() || item.probability < 0.0 || item.probability > 1.0 {
            return Err(ReplayError::InvalidField(format!(
                "invalid distribution probability: {}",
                item.probability
            )));
        }
        sum += item.probability;
    }
    // Distribution sum must be positive and within 0.1 of 1.0 (per plan invariant)
    if (sum - 1.0).abs() > 0.101 {
        return Err(ReplayError::InvalidField(format!(
            "distribution probabilities must sum to 1.0 ± 0.1, got {sum}"
        )));
    }
    Ok(())
}

fn parse_bounded_json(bytes: &[u8], _max_depth: usize) -> Result<Value, ReplayError> {
    use serde::de::DeserializeSeed;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    crate::output::JsonSeed(0)
        .deserialize(&mut deserializer)
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("duplicate JSON key") {
                ReplayError::DuplicateKey(msg)
            } else if msg.contains("JSON depth limit") {
                ReplayError::ExcessiveDepth
            } else {
                ReplayError::InvalidJson(msg)
            }
        })
}
