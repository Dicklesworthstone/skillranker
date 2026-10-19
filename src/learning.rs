//! Evidence-gated learning, centered shrunk usefulness priors, threshold
//! calibration with managed-field rollback, and composite-risk sequential
//! supermartingale monitoring (Phase P8: sr-roadmap-l1i.9.1..9.9).
//!
//! Pure mathematical and policy contracts live in this module alongside bounded
//! owner-only state persistence for policy revisions and monitor epochs.
//! Applying or rolling back a policy updates only managed `[ranking]` fields in
//! trusted user configuration while preserving `[network]`, `[hook]`,
//! credentials, endpoints, skill roots, and exclusions.

use crate::evaluation::{logsumexp, parse_bounded_json};
use crate::limits::{
    CONFIG_FILE_BYTES, CONFIG_FILE_DEPTH, EVALUATION_DATASET_BYTES, EVALUATION_DATASET_DEPTH,
};
use crate::output::ErrorKind;
use crate::scoring::{EPSILON, MAX_TOP_K, Weights, log_odds};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

fn strict_json(text: &str) -> Result<Value, LearningError> {
    parse_bounded_json(text.as_bytes(), EVALUATION_DATASET_DEPTH.max())
        .map_err(|e| LearningError::MalformedInput(e.to_string()))
}

pub const CALIBRATION_SCHEMA: &str = "sr.calibration.v1";
pub const MONITOR_SCHEMA: &str = "sr.monitor.v1";
pub const POLICY_REVISION_SCHEMA_VERSION: u32 = 1;

/// Minimum judged evaluations required before a phase-specific prior cell can be
/// used instead of backing off to the pooled skill-revision estimate.
pub const MIN_PHASE_CELL_JUDGED: u64 = 20;

/// Default conditional risk ceiling under the composite harm-or-unresolved null.
pub const DEFAULT_MONITOR_TAU: f64 = 0.02;

/// Fixed three-point alternative grid for the composite-risk supermartingale.
pub const MONITOR_ALTERNATIVES: [f64; 3] = [0.05, 0.10, 0.20];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LearningError {
    OversizedInput {
        len: usize,
        max: usize,
    },
    MalformedInput(String),
    SplitContamination(String),
    InsufficientCohort(String),
    NotPromotable(String),
    PolicyConflict(String),
    IncompatibleRevision(String),
    MonitorInvalid(String),
    AlphaBudgetExhausted {
        requested: String,
        remaining: String,
    },
    Storage(String),
}

impl LearningError {
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::OversizedInput { .. } => ErrorKind::OversizedInput,
            Self::MalformedInput(_) => ErrorKind::MalformedInput,
            Self::SplitContamination(_)
            | Self::InsufficientCohort(_)
            | Self::NotPromotable(_)
            | Self::PolicyConflict(_)
            | Self::IncompatibleRevision(_)
            | Self::MonitorInvalid(_)
            | Self::AlphaBudgetExhausted { .. } => ErrorKind::InvalidConfiguration,
            Self::Storage(_) => ErrorKind::StorageFailure,
        }
    }
}

impl fmt::Display for LearningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OversizedInput { len, max } => {
                write!(f, "calibration input exceeds size limit ({len} > {max})")
            }
            Self::MalformedInput(msg) => write!(f, "malformed calibration input: {msg}"),
            Self::SplitContamination(msg) => write!(f, "split contamination: {msg}"),
            Self::InsufficientCohort(msg) => write!(f, "insufficient calibration cohort: {msg}"),
            Self::NotPromotable(msg) => write!(f, "candidate policy is not promotable: {msg}"),
            Self::PolicyConflict(msg) => write!(f, "managed policy conflict: {msg}"),
            Self::IncompatibleRevision(msg) => write!(f, "incompatible policy revision: {msg}"),
            Self::MonitorInvalid(msg) => write!(f, "sequential monitor error: {msg}"),
            Self::AlphaBudgetExhausted {
                requested,
                remaining,
            } => write!(
                f,
                "nonrefundable monitor alpha budget exhausted (requested {requested}, remaining {remaining})"
            ),
            Self::Storage(msg) => write!(f, "calibration storage failure: {msg}"),
        }
    }
}

impl std::error::Error for LearningError {}

// =============================================================================
// 1. Optional Centered, Shrunk Beta(1, 4) Usefulness Priors (sr-roadmap-l1i.9.1)
// =============================================================================

/// Centered, shrunk Beta(1, 4) prior adjustment from independently judged
/// usefulness labels:
///
/// ```text
/// q_i = (useful_i + 1) / (judged_i + 5)
/// shrink_i = judged_i / (judged_i + 20)
/// prior_delta_i = shrink_i * clamp(log_odds(q_i) - log_odds(0.2), -1, 1)
/// ```
///
/// Unseen skills (`judged == 0`) receive exact `0.0` contribution.
pub fn beta_shrunk_prior_delta(useful: u64, judged: u64) -> Result<f64, LearningError> {
    if useful > judged {
        return Err(LearningError::MalformedInput(
            "useful count cannot exceed judged count".into(),
        ));
    }
    if judged == 0 {
        return Ok(0.0);
    }
    let q = (useful as f64 + 1.0) / (judged as f64 + 5.0);
    let shrink = (judged as f64) / (judged as f64 + 20.0);
    let baseline = log_odds(0.2);
    let centered = (log_odds(q) - baseline).clamp(-1.0, 1.0);
    Ok(shrink * centered)
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct PriorJudgmentRecord {
    pub event_id: String,
    pub skill_id: String,
    pub content_hash: String,
    pub policy_version: String,
    #[serde(default)]
    pub phase: Option<String>,
    /// True only when an independent usefulness judgment marked the skill useful.
    pub useful: bool,
    /// Must be `"independent_judgment"`; implicit adoption/load records are rejected.
    pub label_source: String,
    pub judged_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct PriorCellCounts {
    pub useful: u64,
    pub judged: u64,
}

impl PriorCellCounts {
    fn record(&mut self, useful: bool) {
        self.judged = self.judged.saturating_add(1);
        if useful {
            self.useful = self.useful.saturating_add(1);
        }
    }

    pub fn delta(self) -> f64 {
        beta_shrunk_prior_delta(self.useful, self.judged).unwrap_or(0.0)
    }

    pub fn has_usable_phase_coverage(self) -> bool {
        self.judged >= MIN_PHASE_CELL_JUDGED && self.useful > 0 && self.useful < self.judged
    }
}

#[derive(Clone, Debug, Default)]
pub struct PriorSnapshot {
    as_of_unix_ms: u64,
    pooled: BTreeMap<(String, String, String), PriorCellCounts>,
    by_phase: BTreeMap<(String, String, String, String), PriorCellCounts>,
}

impl PriorSnapshot {
    /// Build an `as_of`-bounded prior snapshot strictly from independent
    /// usefulness judgments recorded at or before `as_of_unix_ms`.
    pub fn from_judgments(
        records: &[PriorJudgmentRecord],
        as_of_unix_ms: u64,
    ) -> Result<Self, LearningError> {
        let mut snapshot = Self {
            as_of_unix_ms,
            pooled: BTreeMap::new(),
            by_phase: BTreeMap::new(),
        };
        let mut seen_events = BTreeSet::new();
        for record in records {
            if record.label_source != "independent_judgment" {
                return Err(LearningError::MalformedInput(format!(
                    "priors require independent_judgment provenance, got {}",
                    record.label_source
                )));
            }
            if record.skill_id.trim().is_empty()
                || record.content_hash.trim().is_empty()
                || record.policy_version.trim().is_empty()
            {
                return Err(LearningError::MalformedInput(
                    "prior judgment requires non-empty skill_id, content_hash, and policy_version"
                        .into(),
                ));
            }
            // Enforce strict as_of cutoff so future labels never leak into earlier snapshots.
            if record.judged_at_unix_ms > as_of_unix_ms {
                continue;
            }
            let dedup_key = (
                record.event_id.clone(),
                record.skill_id.clone(),
                record.content_hash.clone(),
            );
            if !seen_events.insert(dedup_key) {
                continue;
            }
            snapshot
                .pooled
                .entry((
                    record.skill_id.clone(),
                    record.content_hash.clone(),
                    record.policy_version.clone(),
                ))
                .or_default()
                .record(record.useful);
            if let Some(phase) = &record.phase
                && !phase.trim().is_empty()
            {
                snapshot
                    .by_phase
                    .entry((
                        record.skill_id.clone(),
                        record.content_hash.clone(),
                        record.policy_version.clone(),
                        phase.clone(),
                    ))
                    .or_default()
                    .record(record.useful);
            }
        }
        Ok(snapshot)
    }

    pub const fn as_of_unix_ms(&self) -> u64 {
        self.as_of_unix_ms
    }

    pub fn is_empty(&self) -> bool {
        self.pooled.is_empty() && self.by_phase.is_empty()
    }

    pub fn to_serialized(&self) -> SerializedPriorSnapshot {
        let pooled = self
            .pooled
            .iter()
            .map(
                |((skill_id, content_hash, policy_version), counts)| PooledPriorCell {
                    skill_id: skill_id.clone(),
                    content_hash: content_hash.clone(),
                    policy_version: policy_version.clone(),
                    useful: counts.useful,
                    judged: counts.judged,
                },
            )
            .collect();
        let by_phase = self
            .by_phase
            .iter()
            .map(
                |((skill_id, content_hash, policy_version, phase), counts)| PhasePriorCell {
                    skill_id: skill_id.clone(),
                    content_hash: content_hash.clone(),
                    policy_version: policy_version.clone(),
                    phase: phase.clone(),
                    useful: counts.useful,
                    judged: counts.judged,
                },
            )
            .collect();
        SerializedPriorSnapshot {
            schema_version: 1,
            as_of_unix_ms: self.as_of_unix_ms,
            pooled,
            by_phase,
        }
    }

    pub fn from_serialized(raw: SerializedPriorSnapshot) -> Result<Self, LearningError> {
        if raw.schema_version != 1 {
            return Err(LearningError::MalformedInput(format!(
                "unsupported prior snapshot schema_version {}",
                raw.schema_version
            )));
        }
        let mut pooled = BTreeMap::new();
        for cell in raw.pooled {
            if cell.useful > cell.judged {
                return Err(LearningError::MalformedInput(
                    "prior cell useful count exceeds judged count".into(),
                ));
            }
            pooled.insert(
                (cell.skill_id, cell.content_hash, cell.policy_version),
                PriorCellCounts {
                    useful: cell.useful,
                    judged: cell.judged,
                },
            );
        }
        let mut by_phase = BTreeMap::new();
        for cell in raw.by_phase {
            if cell.useful > cell.judged {
                return Err(LearningError::MalformedInput(
                    "phase prior cell useful count exceeds judged count".into(),
                ));
            }
            by_phase.insert(
                (
                    cell.skill_id,
                    cell.content_hash,
                    cell.policy_version,
                    cell.phase,
                ),
                PriorCellCounts {
                    useful: cell.useful,
                    judged: cell.judged,
                },
            );
        }
        Ok(Self {
            as_of_unix_ms: raw.as_of_unix_ms,
            pooled,
            by_phase,
        })
    }

    /// Resolve the centered shrunk prior delta for a candidate skill revision.
    /// Changed `content_hash` or `policy_version` never inherits old evidence.
    /// Sparse phase cells back off to the pooled revision estimate, or `0.0`.
    pub fn prior_delta(
        &self,
        skill_id: &str,
        content_hash: &str,
        policy_version: &str,
        phase: Option<&str>,
    ) -> f64 {
        if let Some(phase_name) = phase
            && let Some(cell) = self.by_phase.get(&(
                skill_id.to_owned(),
                content_hash.to_owned(),
                policy_version.to_owned(),
                phase_name.to_owned(),
            ))
            && cell.has_usable_phase_coverage()
        {
            return cell.delta();
        }
        self.pooled
            .get(&(
                skill_id.to_owned(),
                content_hash.to_owned(),
                policy_version.to_owned(),
            ))
            .copied()
            .map(PriorCellCounts::delta)
            .unwrap_or(0.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PooledPriorCell {
    pub skill_id: String,
    pub content_hash: String,
    pub policy_version: String,
    pub useful: u64,
    pub judged: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhasePriorCell {
    pub skill_id: String,
    pub content_hash: String,
    pub policy_version: String,
    pub phase: String,
    pub useful: u64,
    pub judged: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerializedPriorSnapshot {
    pub schema_version: u32,
    pub as_of_unix_ms: u64,
    #[serde(default)]
    pub pooled: Vec<PooledPriorCell>,
    #[serde(default)]
    pub by_phase: Vec<PhasePriorCell>,
}

// =============================================================================
// 2. Managed Policy, Split Calibration & Rollback (sr-roadmap-l1i.9.2..9.4)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ManagedRankingPolicy {
    pub gate_threshold: f64,
    pub fit_threshold: f64,
    pub w_fit: f64,
    pub w_prior: f64,
    pub w_phase: f64,
    pub top_k: u32,
    pub shortlist_m: u32,
}

impl Default for ManagedRankingPolicy {
    fn default() -> Self {
        Self::BASELINE
    }
}

impl ManagedRankingPolicy {
    pub const BASELINE: Self = Self {
        gate_threshold: 0.30,
        fit_threshold: 0.30,
        w_fit: 1.0,
        w_prior: 0.0,
        w_phase: 0.0,
        top_k: 5,
        shortlist_m: 8,
    };

    pub fn validate(&self) -> Result<(), LearningError> {
        if !(0.0..=1.0).contains(&self.gate_threshold) || !self.gate_threshold.is_finite() {
            return Err(LearningError::MalformedInput(
                "gate_threshold must be in [0.0, 1.0]".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.fit_threshold) || !self.fit_threshold.is_finite() {
            return Err(LearningError::MalformedInput(
                "fit_threshold must be in [0.0, 1.0]".into(),
            ));
        }
        Weights::new(self.w_fit, self.w_prior, self.w_phase)
            .map_err(|_| LearningError::MalformedInput("weights out of bounds".into()))?;
        if self.top_k == 0
            || self.shortlist_m == 0
            || self.top_k > self.shortlist_m
            || self.shortlist_m as usize > MAX_TOP_K
        {
            return Err(LearningError::MalformedInput(
                "top_k and shortlist_m must satisfy 1 <= top_k <= shortlist_m <= 32".into(),
            ));
        }
        Ok(())
    }

    pub fn digest(&self) -> String {
        let canonical = format!(
            "v1|gate={:.6}|fit={:.6}|w_fit={:.6}|w_prior={:.6}|w_phase={:.6}|k={}|m={}",
            self.gate_threshold,
            self.fit_threshold,
            self.w_fit,
            self.w_prior,
            self.w_phase,
            self.top_k,
            self.shortlist_m
        );
        blake3::hash(canonical.as_bytes()).to_hex().to_string()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "gate_threshold": self.gate_threshold,
            "fit_threshold": self.fit_threshold,
            "w_fit": self.w_fit,
            "w_prior": self.w_prior,
            "w_phase": self.w_phase,
            "top_k": self.top_k,
            "shortlist_m": self.shortlist_m,
            "digest": self.digest(),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum CalibrationSplit {
    Training,
    Validation,
    Holdout,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibrationCandidateInput {
    pub skill_id: String,
    pub content_hash: String,
    pub rerank_probability: f64,
    pub fit: f64,
    #[serde(default = "default_true")]
    pub eligible: bool,
    #[serde(default)]
    pub phase_match: f64,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibrationCaseInput {
    pub case_id: String,
    pub family_id: String,
    pub split: CalibrationSplit,
    pub timestamp_unix_ms: u64,
    pub acceptable_skills: Vec<String>,
    #[serde(default)]
    pub near_miss: bool,
    pub needs_skill_gate: f64,
    pub none_probability: f64,
    #[serde(default = "default_true")]
    pub stage2_present: bool,
    #[serde(default)]
    pub operationally_unavailable: bool,
    #[serde(default)]
    pub phase: Option<String>,
    pub candidates: Vec<CalibrationCandidateInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibrationArtifact {
    pub schema_version: u32,
    pub policy_version: String,
    #[serde(default = "default_min_split_cases")]
    pub min_split_cases: usize,
    #[serde(default)]
    pub unfinished_cases: usize,
    #[serde(default)]
    pub training_judgments: Vec<PriorJudgmentRecord>,
    #[serde(default)]
    pub candidate_policies: Vec<ManagedRankingPolicy>,
    pub cases: Vec<CalibrationCaseInput>,
}

const fn default_min_split_cases() -> usize {
    3
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SplitLossSummary {
    pub total_cases: usize,
    pub positive_cases: usize,
    pub negative_cases: usize,
    pub near_miss_cases: usize,
    pub operational_failures: usize,
    pub total_loss: u64,
    pub mean_loss: f64,
    pub normalized_bounded_loss: f64,
    pub estimable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CalibrationReport {
    pub schema: String,
    pub mode: String,
    pub applied: bool,
    pub promotable: bool,
    pub revision_id: String,
    pub evaluation_digest: String,
    pub prior_as_of_unix_ms: u64,
    pub baseline_policy: ManagedRankingPolicy,
    pub selected_policy: ManagedRankingPolicy,
    pub baseline_validation: SplitLossSummary,
    pub selected_validation: SplitLossSummary,
    pub baseline_holdout: SplitLossSummary,
    pub selected_holdout: SplitLossSummary,
    pub non_estimable_candidates: usize,
    pub promotion_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_snapshot: Option<SerializedPriorSnapshot>,
}

/// Score one case under `policy` and `priors`.
///
/// Tuning loss contract:
/// - `0`: correct top-1 recommendation on a positive case, or abstention on a no-match case
/// - `1`: abstaining on a positive case
/// - `2`: wrong suggestion on a positive or negative case, or operational `unavailable`
///
/// Returns `None` if the policy requires stage-2 rerank evidence that is absent
/// (`policy.gate_threshold <= case.needs_skill_gate && !case.stage2_present`).
pub fn evaluate_case_loss(
    case: &CalibrationCaseInput,
    policy: &ManagedRankingPolicy,
    priors: &PriorSnapshot,
    policy_version: &str,
) -> Option<u64> {
    if case.operationally_unavailable {
        return Some(2);
    }
    let passes_wide_gate = case.needs_skill_gate >= policy.gate_threshold;
    if !passes_wide_gate {
        return Some(if case.acceptable_skills.is_empty() {
            0
        } else {
            1
        });
    }
    if !case.stage2_present {
        // Stage 2 was skipped when recorded; cannot impute missing fits.
        return None;
    }
    // Filter by local eligibility, sentinel (strictly beating __none__), and fit threshold.
    // Priors and weights only reorder surviving candidates; they never rescue a candidate.
    let mut surviving: Vec<(&CalibrationCandidateInput, f64)> = Vec::new();
    for cand in &case.candidates {
        if !cand.eligible {
            continue;
        }
        if cand.rerank_probability <= case.none_probability {
            continue;
        }
        if cand.fit < policy.fit_threshold {
            continue;
        }
        let prior_delta = if policy.w_prior > 0.0 {
            priors.prior_delta(
                &cand.skill_id,
                &cand.content_hash,
                policy_version,
                case.phase.as_deref(),
            )
        } else {
            0.0
        };
        let logit = cand.rerank_probability.clamp(EPSILON, 1.0 - EPSILON).ln()
            + policy.w_fit * log_odds(cand.fit)
            + policy.w_prior * prior_delta
            + policy.w_phase * cand.phase_match.clamp(0.0, 1.0);
        surviving.push((cand, logit));
    }
    if surviving.is_empty() {
        return Some(if case.acceptable_skills.is_empty() {
            0
        } else {
            1
        });
    }
    surviving.sort_by(|(a_cand, a_score), (b_cand, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| a_cand.skill_id.cmp(&b_cand.skill_id))
    });
    let top_skill = &surviving[0].0.skill_id;
    if case.acceptable_skills.iter().any(|s| s == top_skill) {
        Some(0)
    } else {
        Some(2)
    }
}

pub fn evaluate_split(
    cases: &[&CalibrationCaseInput],
    policy: &ManagedRankingPolicy,
    priors: &PriorSnapshot,
    policy_version: &str,
) -> SplitLossSummary {
    let total_cases = cases.len();
    let positive_cases = cases
        .iter()
        .filter(|c| !c.acceptable_skills.is_empty())
        .count();
    let negative_cases = total_cases.saturating_sub(positive_cases);
    let near_miss_cases = cases.iter().filter(|c| c.near_miss).count();
    let operational_failures = cases.iter().filter(|c| c.operationally_unavailable).count();

    let mut total_loss = 0_u64;
    for case in cases {
        let Some(loss) = evaluate_case_loss(case, policy, priors, policy_version) else {
            return SplitLossSummary {
                total_cases,
                positive_cases,
                negative_cases,
                near_miss_cases,
                operational_failures,
                total_loss: 0,
                mean_loss: f64::NAN,
                normalized_bounded_loss: f64::NAN,
                estimable: false,
            };
        };
        total_loss = total_loss.saturating_add(loss);
    }
    let mean_loss = if total_cases > 0 {
        total_loss as f64 / total_cases as f64
    } else {
        0.0
    };
    SplitLossSummary {
        total_cases,
        positive_cases,
        negative_cases,
        near_miss_cases,
        operational_failures,
        total_loss,
        mean_loss,
        normalized_bounded_loss: mean_loss / 2.0,
        estimable: true,
    }
}

pub fn run_calibration_from_bytes(bytes: &[u8]) -> Result<CalibrationReport, LearningError> {
    if bytes.len() > EVALUATION_DATASET_BYTES.max() {
        return Err(LearningError::OversizedInput {
            len: bytes.len(),
            max: EVALUATION_DATASET_BYTES.max(),
        });
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| LearningError::MalformedInput("calibration input is not UTF-8".into()))?;
    let value = strict_json(text)
        .map_err(|_| LearningError::MalformedInput("invalid or duplicate-key JSON".into()))?;
    let artifact: CalibrationArtifact = serde_json::from_value(value)
        .map_err(|e| LearningError::MalformedInput(format!("schema decode failed: {e}")))?;
    if artifact.schema_version != 1 {
        return Err(LearningError::MalformedInput(format!(
            "unsupported calibration schema_version {}",
            artifact.schema_version
        )));
    }
    if artifact.unfinished_cases > 0 {
        return Err(LearningError::InsufficientCohort(format!(
            "evaluation batch has {} unfinished cases; partial batches cannot calibrate a policy",
            artifact.unfinished_cases
        )));
    }

    let mut seen_cases = BTreeSet::new();
    let mut family_splits: BTreeMap<&str, CalibrationSplit> = BTreeMap::new();
    for case in &artifact.cases {
        if case.case_id.trim().is_empty() || case.family_id.trim().is_empty() {
            return Err(LearningError::MalformedInput(
                "case_id and family_id must be non-empty".into(),
            ));
        }
        if !seen_cases.insert(case.case_id.as_str()) {
            return Err(LearningError::MalformedInput(format!(
                "duplicate case_id {}",
                case.case_id
            )));
        }
        if let Some(&prev_split) = family_splits.get(case.family_id.as_str())
            && prev_split != case.split
        {
            return Err(LearningError::SplitContamination(format!(
                "family {} appears in both {:?} and {:?}",
                case.family_id, prev_split, case.split
            )));
        }
        family_splits.insert(case.family_id.as_str(), case.split);
    }

    let training_cases: Vec<&CalibrationCaseInput> = artifact
        .cases
        .iter()
        .filter(|c| c.split == CalibrationSplit::Training)
        .collect();
    let validation_cases: Vec<&CalibrationCaseInput> = artifact
        .cases
        .iter()
        .filter(|c| c.split == CalibrationSplit::Validation)
        .collect();
    let holdout_cases: Vec<&CalibrationCaseInput> = artifact
        .cases
        .iter()
        .filter(|c| c.split == CalibrationSplit::Holdout)
        .collect();

    for (name, split_cases) in [
        ("training", &training_cases),
        ("validation", &validation_cases),
        ("holdout", &holdout_cases),
    ] {
        if split_cases.len() < artifact.min_split_cases {
            return Err(LearningError::InsufficientCohort(format!(
                "{name} split has {} cases, minimum required is {}",
                split_cases.len(),
                artifact.min_split_cases
            )));
        }
        let pos = split_cases
            .iter()
            .filter(|c| !c.acceptable_skills.is_empty())
            .count();
        let neg = split_cases.len().saturating_sub(pos);
        let near = split_cases.iter().filter(|c| c.near_miss).count();
        if pos == 0 || neg == 0 || near == 0 {
            return Err(LearningError::InsufficientCohort(format!(
                "{name} split must include positive ({pos}), no-match ({neg}), and near-miss ({near}) cases"
            )));
        }
    }

    // Temporal leakage check: prior snapshot cutoff is the maximum training case timestamp,
    // which must predate every validation and holdout case.
    let prior_as_of_unix_ms = training_cases
        .iter()
        .map(|c| c.timestamp_unix_ms)
        .max()
        .unwrap_or(0);
    let min_eval_ts = validation_cases
        .iter()
        .chain(holdout_cases.iter())
        .map(|c| c.timestamp_unix_ms)
        .min()
        .unwrap_or(0);
    if prior_as_of_unix_ms >= min_eval_ts {
        return Err(LearningError::SplitContamination(format!(
            "training timestamp cutoff ({prior_as_of_unix_ms}) must strictly predate validation/holdout ({min_eval_ts})"
        )));
    }

    let priors = PriorSnapshot::from_judgments(&artifact.training_judgments, prior_as_of_unix_ms)?;

    let baseline = ManagedRankingPolicy::BASELINE;
    let baseline_validation = evaluate_split(
        &validation_cases,
        &baseline,
        &priors,
        &artifact.policy_version,
    );
    if !baseline_validation.estimable {
        return Err(LearningError::InsufficientCohort(
            "baseline policy is not estimable on validation split".into(),
        ));
    }

    let mut candidates = vec![baseline.clone()];
    for cand in &artifact.candidate_policies {
        cand.validate()?;
        if !candidates.contains(cand) {
            candidates.push(cand.clone());
        }
    }
    if artifact.candidate_policies.is_empty() {
        for gate in [0.25, 0.30, 0.35, 0.45] {
            for fit in [0.25, 0.30, 0.40, 0.50] {
                for w_prior in [0.0, 0.25, 0.50] {
                    let p = ManagedRankingPolicy {
                        gate_threshold: gate,
                        fit_threshold: fit,
                        w_fit: 1.0,
                        w_prior,
                        w_phase: 0.0,
                        top_k: 5,
                        shortlist_m: 8,
                    };
                    if !candidates.contains(&p) {
                        candidates.push(p);
                    }
                }
            }
        }
    }

    let mut selected_policy = baseline.clone();
    let mut selected_validation = baseline_validation.clone();
    let mut non_estimable_candidates = 0_usize;

    for cand in &candidates {
        let summary = evaluate_split(&validation_cases, cand, &priors, &artifact.policy_version);
        if !summary.estimable {
            non_estimable_candidates += 1;
            continue;
        }
        let better_loss = summary.mean_loss + 1e-12 < selected_validation.mean_loss;
        let tied_loss = (summary.mean_loss - selected_validation.mean_loss).abs() <= 1e-12;
        let fewer_failures =
            summary.operational_failures < selected_validation.operational_failures;
        if better_loss || (tied_loss && fewer_failures) {
            selected_policy = cand.clone();
            selected_validation = summary;
        }
    }

    let baseline_holdout =
        evaluate_split(&holdout_cases, &baseline, &priors, &artifact.policy_version);
    let selected_holdout = evaluate_split(
        &holdout_cases,
        &selected_policy,
        &priors,
        &artifact.policy_version,
    );
    if !baseline_holdout.estimable || !selected_holdout.estimable {
        return Err(LearningError::InsufficientCohort(
            "selected or baseline policy is not estimable on holdout split".into(),
        ));
    }

    let changed_from_baseline = selected_policy != baseline;
    let holdout_better = selected_holdout.mean_loss + 1e-12 < baseline_holdout.mean_loss
        || ((selected_holdout.mean_loss - baseline_holdout.mean_loss).abs() <= 1e-12
            && selected_holdout.operational_failures < baseline_holdout.operational_failures);
    let promotable = changed_from_baseline && holdout_better;
    let promotion_reason = if !changed_from_baseline {
        "baseline_retained_on_validation".to_owned()
    } else if holdout_better {
        "held_out_loss_improved_over_baseline".to_owned()
    } else {
        "held_out_benefit_not_demonstrated".to_owned()
    };

    let evaluation_digest = blake3::hash(bytes).to_hex().to_string();
    let revision_seed = format!("{}:{}", evaluation_digest, selected_policy.digest());
    let revision_id = format!(
        "pol-{}",
        &blake3::hash(revision_seed.as_bytes()).to_hex()[..16]
    );
    let prior_snapshot = if priors.is_empty() {
        None
    } else {
        Some(priors.to_serialized())
    };

    Ok(CalibrationReport {
        schema: CALIBRATION_SCHEMA.to_owned(),
        mode: "preview".to_owned(),
        applied: false,
        promotable,
        revision_id,
        evaluation_digest,
        prior_as_of_unix_ms,
        baseline_policy: baseline,
        selected_policy,
        baseline_validation,
        selected_validation,
        baseline_holdout,
        selected_holdout,
        non_estimable_candidates,
        promotion_reason,
        prior_snapshot,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyRevisionRecord {
    pub schema_version: u32,
    pub revision_id: String,
    pub evaluation_digest: String,
    pub previous_policy: ManagedRankingPolicy,
    pub applied_policy: ManagedRankingPolicy,
    pub expected_managed_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_prior_snapshot: Option<SerializedPriorSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_prior_snapshot: Option<SerializedPriorSnapshot>,
}

fn ensure_private_dir(path: &Path) -> Result<(), LearningError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        if !path.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| LearningError::Storage(format!("create {}: {e}", path.display())))?;
        }
        let meta = fs::symlink_metadata(path)
            .map_err(|e| LearningError::Storage(format!("stat {}: {e}", path.display())))?;
        if meta.file_type().is_symlink() || !meta.is_dir() || (meta.mode() & 0o077) != 0 {
            return Err(LearningError::Storage(format!(
                "directory {} is not owner-only 0700",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path).map_err(|e| LearningError::Storage(e.to_string()))?;
    Ok(())
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), LearningError> {
    let parent = path
        .parent()
        .ok_or_else(|| LearningError::Storage("missing parent directory".into()))?;
    ensure_private_dir(parent)?;
    let tmp = parent.join(format!(
        ".{}.sr-tmp-{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("policy"),
        std::process::id()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| LearningError::Storage(format!("open temp {}: {e}", tmp.display())))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| LearningError::Storage(format!("write temp {}: {e}", tmp.display())))?;
    }
    #[cfg(not(unix))]
    fs::write(&tmp, bytes).map_err(|e| LearningError::Storage(e.to_string()))?;
    fs::rename(&tmp, path)
        .map_err(|e| LearningError::Storage(format!("rename to {}: {e}", path.display())))?;
    Ok(())
}

pub fn save_active_prior_snapshot(
    state_dir: &Path,
    snapshot: &PriorSnapshot,
) -> Result<(), LearningError> {
    ensure_private_dir(state_dir)?;
    let path = state_dir.join("prior_snapshot.json");
    let bytes = serde_json::to_vec_pretty(&snapshot.to_serialized())
        .map_err(|e| LearningError::Storage(e.to_string()))?;
    write_private_atomic(&path, &bytes)
}

pub fn load_active_prior_snapshot(
    state_dir: &Path,
) -> Result<Option<PriorSnapshot>, LearningError> {
    let path = state_dir.join("prior_snapshot.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = read_bounded_regular_file_string(&path, CONFIG_FILE_BYTES.max(), "prior snapshot")?;
    let val = strict_json(&text).map_err(|_| {
        LearningError::MalformedInput("corrupt or duplicate-key prior snapshot".into())
    })?;
    let raw: SerializedPriorSnapshot = serde_json::from_value(val).map_err(|e| {
        LearningError::MalformedInput(format!("invalid prior snapshot schema: {e}"))
    })?;
    let snap = PriorSnapshot::from_serialized(raw)?;
    if snap.is_empty() {
        Ok(None)
    } else {
        Ok(Some(snap))
    }
}

fn read_bounded_regular_file_string(
    path: &Path,
    max_bytes: usize,
    what: &str,
) -> Result<String, LearningError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| LearningError::Storage(format!("stat {}: {e}", path.display())))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(LearningError::Storage(format!(
            "trusted {what} path must be a regular file"
        )));
    }
    let file = fs::File::open(path)
        .map_err(|e| LearningError::Storage(format!("open {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| LearningError::Storage(format!("read {}: {e}", path.display())))?;
    if bytes.len() > max_bytes {
        return Err(LearningError::OversizedInput {
            len: bytes.len(),
            max: max_bytes,
        });
    }
    String::from_utf8(bytes)
        .map_err(|_| LearningError::MalformedInput(format!("{what} is not valid UTF-8")))
}

fn check_toml_depth(
    value: &toml::Value,
    depth: usize,
    max_depth: usize,
) -> Result<(), LearningError> {
    if depth > max_depth {
        return Err(LearningError::MalformedInput(format!(
            "config.toml exceeds maximum nesting depth {max_depth}"
        )));
    }
    match value {
        toml::Value::Table(table) => {
            for child in table.values() {
                check_toml_depth(child, depth + 1, max_depth)?;
            }
        }
        toml::Value::Array(items) => {
            for child in items {
                check_toml_depth(child, depth + 1, max_depth)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn toml_as_f64(value: &toml::Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|v| v as f64))
}

/// Extract the currently configured managed `[ranking]` policy from a TOML
/// table (falling back to `ManagedRankingPolicy::BASELINE` for omitted keys).
pub fn read_managed_policy_from_toml(
    config_path: &Path,
) -> Result<(toml::Table, ManagedRankingPolicy), LearningError> {
    if !config_path.exists() {
        return Ok((toml::Table::new(), ManagedRankingPolicy::BASELINE));
    }
    let text =
        read_bounded_regular_file_string(config_path, CONFIG_FILE_BYTES.max(), "configuration")?;
    let root: toml::Table = toml::from_str(&text)
        .map_err(|e| LearningError::MalformedInput(format!("invalid config.toml: {e}")))?;
    for child in root.values() {
        check_toml_depth(child, 1, CONFIG_FILE_DEPTH.max())?;
    }

    let mut policy = ManagedRankingPolicy::BASELINE;
    if let Some(toml::Value::Table(ranking)) = root.get("ranking") {
        if let Some(v) = ranking
            .get("gate")
            .or_else(|| ranking.get("gate_threshold"))
            .and_then(toml_as_f64)
        {
            policy.gate_threshold = v;
        }
        if let Some(v) = ranking
            .get("fits")
            .or_else(|| ranking.get("fit_threshold"))
            .and_then(toml_as_f64)
        {
            policy.fit_threshold = v;
        }
        if let Some(v) = ranking.get("w_fit").and_then(toml_as_f64) {
            policy.w_fit = v;
        }
        if let Some(v) = ranking.get("w_prior").and_then(toml_as_f64) {
            policy.w_prior = v;
        }
        if let Some(v) = ranking.get("w_phase").and_then(toml_as_f64) {
            policy.w_phase = v;
        }
        if let Some(v) = ranking
            .get("top")
            .or_else(|| ranking.get("top_k"))
            .and_then(toml::Value::as_integer)
        {
            policy.top_k = u32::try_from(v)
                .map_err(|_| LearningError::MalformedInput("invalid top in config.toml".into()))?;
        }
        if let Some(v) = ranking
            .get("shortlist")
            .or_else(|| ranking.get("shortlist_m"))
            .and_then(toml::Value::as_integer)
        {
            policy.shortlist_m = u32::try_from(v).map_err(|_| {
                LearningError::MalformedInput("invalid shortlist in config.toml".into())
            })?;
        }
    }
    policy.validate()?;
    Ok((root, policy))
}

fn write_managed_policy_to_toml(
    config_path: &Path,
    mut root: toml::Table,
    policy: &ManagedRankingPolicy,
    expected_base_digest: Option<&crate::identity::ContentHash>,
) -> Result<(), LearningError> {
    policy.validate()?;
    let current_bytes = if config_path.exists() {
        read_bounded_regular_file_string(config_path, CONFIG_FILE_BYTES.max(), "configuration")?
            .into_bytes()
    } else {
        Vec::new()
    };
    let current_digest = crate::identity::ContentHash::from_bytes(&current_bytes);
    if let Some(expected) = expected_base_digest
        && &current_digest != expected
    {
        return Err(LearningError::PolicyConflict(
            "config.toml changed on disk before managed policy replacement".into(),
        ));
    }
    let mutation_changes = vec![
        (
            "ranking.gate".to_owned(),
            crate::config::RawValue::Float(policy.gate_threshold),
        ),
        (
            "ranking.fits".to_owned(),
            crate::config::RawValue::Float(policy.fit_threshold),
        ),
        (
            "ranking.w_fit".to_owned(),
            crate::config::RawValue::Float(policy.w_fit),
        ),
        (
            "ranking.w_prior".to_owned(),
            crate::config::RawValue::Float(policy.w_prior),
        ),
        (
            "ranking.w_phase".to_owned(),
            crate::config::RawValue::Float(policy.w_phase),
        ),
    ];
    crate::config::ManagedPolicyMutation::new(current_digest, mutation_changes).map_err(|e| {
        LearningError::MalformedInput(format!("managed policy mutation rejected: {e}"))
    })?;

    let ranking_entry = root
        .entry("ranking")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(ranking) = ranking_entry.as_table_mut() else {
        return Err(LearningError::MalformedInput(
            "[ranking] in config.toml is not a table".into(),
        ));
    };
    // Strip legacy non-registry names so the resulting file strictly satisfies KEY_SPECS.
    ranking.remove("gate_threshold");
    ranking.remove("fit_threshold");
    ranking.remove("top_k");
    ranking.remove("shortlist_m");

    ranking.insert("gate".into(), toml::Value::Float(policy.gate_threshold));
    ranking.insert("fits".into(), toml::Value::Float(policy.fit_threshold));
    ranking.insert("w_fit".into(), toml::Value::Float(policy.w_fit));
    ranking.insert("w_prior".into(), toml::Value::Float(policy.w_prior));
    ranking.insert("w_phase".into(), toml::Value::Float(policy.w_phase));
    ranking.insert("top".into(), toml::Value::Integer(i64::from(policy.top_k)));
    ranking.insert(
        "shortlist".into(),
        toml::Value::Integer(i64::from(policy.shortlist_m)),
    );
    let rendered = toml::to_string(&root)
        .map_err(|e| LearningError::Storage(format!("serialize config.toml: {e}")))?;
    write_private_atomic(config_path, rendered.as_bytes())
}

pub fn apply_calibrated_policy(
    mut report: CalibrationReport,
    config_path: &Path,
    state_dir: &Path,
) -> Result<CalibrationReport, LearningError> {
    if !report.promotable {
        return Err(LearningError::NotPromotable(report.promotion_reason));
    }
    // Check if an active sequential monitor has alarmed or been invalidated.
    let monitor_path = state_dir.join("monitor_state.json");
    if monitor_path.exists() {
        let state = load_monitor_state(&monitor_path)?;
        if state.alarm_triggered || state.status == "alarmed" || state.status == "invalidated" {
            return Err(LearningError::NotPromotable(format!(
                "sequential risk monitor status is '{}' (alarm_triggered={}); return to baseline/shadow before promoting a policy",
                state.status, state.alarm_triggered
            )));
        }
    }

    let base_bytes = if config_path.exists() {
        read_bounded_regular_file_string(config_path, CONFIG_FILE_BYTES.max(), "configuration")?
            .into_bytes()
    } else {
        Vec::new()
    };
    let base_digest = crate::identity::ContentHash::from_bytes(&base_bytes);
    let (root_table, current_policy) = read_managed_policy_from_toml(config_path)?;
    let previous_prior_snapshot =
        load_active_prior_snapshot(state_dir)?.map(|snap| snap.to_serialized());

    let revisions_dir = state_dir.join("policy-revisions");
    ensure_private_dir(&revisions_dir)?;
    if !base_bytes.is_empty() {
        let backup_path = revisions_dir.join(format!("{}.bak.toml", report.revision_id));
        write_private_atomic(&backup_path, &base_bytes)?;
    }

    let rev_path = revisions_dir.join(format!("{}.json", report.revision_id));
    let record = PolicyRevisionRecord {
        schema_version: POLICY_REVISION_SCHEMA_VERSION,
        revision_id: report.revision_id.clone(),
        evaluation_digest: report.evaluation_digest.clone(),
        previous_policy: current_policy,
        applied_policy: report.selected_policy.clone(),
        expected_managed_digest: report.selected_policy.digest(),
        previous_prior_snapshot,
        applied_prior_snapshot: report.prior_snapshot.clone(),
    };
    let rev_bytes =
        serde_json::to_vec_pretty(&record).map_err(|e| LearningError::Storage(e.to_string()))?;
    write_private_atomic(&rev_path, &rev_bytes)?;
    if report.selected_policy.w_prior > 0.0 {
        if let Some(raw_snap) = &report.prior_snapshot {
            let snap = PriorSnapshot::from_serialized(raw_snap.clone())?;
            save_active_prior_snapshot(state_dir, &snap)?;
        }
    } else if state_dir.join("prior_snapshot.json").exists() {
        save_active_prior_snapshot(state_dir, &PriorSnapshot::default())?;
    }
    write_managed_policy_to_toml(
        config_path,
        root_table,
        &report.selected_policy,
        Some(&base_digest),
    )?;
    report.mode = "applied".to_owned();
    report.applied = true;
    Ok(report)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PolicyRollbackReport {
    pub schema: String,
    pub mode: String,
    pub applied: bool,
    pub revision_id: String,
    pub current_policy: ManagedRankingPolicy,
    pub restored_policy: ManagedRankingPolicy,
    pub preserved_unrelated_sections: Vec<String>,
}

pub fn rollback_managed_policy(
    revision_id: &str,
    apply: bool,
    config_path: &Path,
    state_dir: &Path,
) -> Result<PolicyRollbackReport, LearningError> {
    if revision_id.trim().is_empty()
        || revision_id.contains('/')
        || revision_id.contains('\\')
        || revision_id.contains("..")
    {
        return Err(LearningError::MalformedInput(
            "invalid policy revision identifier".into(),
        ));
    }
    let rev_path = state_dir
        .join("policy-revisions")
        .join(format!("{revision_id}.json"));
    if !rev_path.exists() {
        return Err(LearningError::IncompatibleRevision(format!(
            "policy revision {revision_id} does not exist"
        )));
    }
    let text =
        read_bounded_regular_file_string(&rev_path, CONFIG_FILE_BYTES.max(), "policy revision")?;
    let val = strict_json(&text)
        .map_err(|_| LearningError::IncompatibleRevision("malformed revision JSON".into()))?;
    let record: PolicyRevisionRecord = serde_json::from_value(val).map_err(|e| {
        LearningError::IncompatibleRevision(format!("incompatible revision schema: {e}"))
    })?;
    if record.schema_version != POLICY_REVISION_SCHEMA_VERSION {
        return Err(LearningError::IncompatibleRevision(format!(
            "unsupported policy revision schema_version {}",
            record.schema_version
        )));
    }
    record.previous_policy.validate()?;
    record.applied_policy.validate()?;
    if record.applied_policy.digest() != record.expected_managed_digest {
        return Err(LearningError::IncompatibleRevision(
            "recorded revision digest mismatch".into(),
        ));
    }

    let base_bytes = if config_path.exists() {
        read_bounded_regular_file_string(config_path, CONFIG_FILE_BYTES.max(), "configuration")?
            .into_bytes()
    } else {
        Vec::new()
    };
    let base_digest = crate::identity::ContentHash::from_bytes(&base_bytes);
    let (root_table, current_policy) = read_managed_policy_from_toml(config_path)?;
    if current_policy.digest() != record.expected_managed_digest {
        return Err(LearningError::PolicyConflict(format!(
            "current managed ranking policy digest ({}) does not match revision {} expected digest ({}); refusing conflicting rollback",
            current_policy.digest(),
            revision_id,
            record.expected_managed_digest
        )));
    }

    let preserved_unrelated_sections: Vec<String> = root_table
        .keys()
        .filter(|k| k.as_str() != "ranking")
        .cloned()
        .collect();

    if apply {
        write_managed_policy_to_toml(
            config_path,
            root_table,
            &record.previous_policy,
            Some(&base_digest),
        )?;
        match record.previous_prior_snapshot {
            Some(raw_snap) if record.previous_policy.w_prior > 0.0 => {
                let snap = PriorSnapshot::from_serialized(raw_snap)?;
                save_active_prior_snapshot(state_dir, &snap)?;
            }
            _ => {
                if state_dir.join("prior_snapshot.json").exists() {
                    save_active_prior_snapshot(state_dir, &PriorSnapshot::default())?;
                }
            }
        }
    }

    Ok(PolicyRollbackReport {
        schema: CALIBRATION_SCHEMA.to_owned(),
        mode: if apply {
            "rollback-applied".to_owned()
        } else {
            "rollback-preview".to_owned()
        },
        applied: apply,
        revision_id: revision_id.to_owned(),
        current_policy,
        restored_policy: record.previous_policy,
        preserved_unrelated_sections,
    })
}

// =============================================================================
// 3. Composite-Risk Supermartingale & Nonrefundable Alpha (sr-roadmap-l1i.9.5..9.7)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CompositeRiskAccumulator {
    pub tau: f64,
    pub alpha_monitor: f64,
    pub log_e: [f64; 3],
    pub judged_harm_units: usize,
    pub unresolved_units: usize,
    pub unflagged_units: usize,
    pub alarm_triggered: bool,
}

impl CompositeRiskAccumulator {
    pub fn new(alpha_monitor: f64) -> Result<Self, LearningError> {
        Self::with_tau(DEFAULT_MONITOR_TAU, alpha_monitor)
    }

    pub fn with_tau(tau: f64, alpha_monitor: f64) -> Result<Self, LearningError> {
        if !(0.0..MONITOR_ALTERNATIVES[0]).contains(&tau) || !tau.is_finite() {
            return Err(LearningError::MonitorInvalid(
                "tau must be finite and strictly between 0 and the smallest alternative (0.05)"
                    .into(),
            ));
        }
        if !(0.0..1.0).contains(&alpha_monitor)
            || !alpha_monitor.is_finite()
            || alpha_monitor == 0.0
        {
            return Err(LearningError::MonitorInvalid(
                "alpha_monitor must be finite in (0, 1)".into(),
            ));
        }
        Ok(Self {
            tau,
            alpha_monitor,
            log_e: [0.0; 3],
            judged_harm_units: 0,
            unresolved_units: 0,
            unflagged_units: 0,
            alarm_triggered: false,
        })
    }

    pub fn threshold(&self) -> f64 {
        1.0 / self.alpha_monitor
    }

    pub fn component_e_values(&self) -> [f64; 3] {
        [
            self.log_e[0].exp(),
            self.log_e[1].exp(),
            self.log_e[2].exp(),
        ]
    }

    pub fn mixture_e_value(&self) -> f64 {
        let lse = logsumexp(&self.log_e);
        (lse - (3.0_f64).ln()).exp()
    }

    /// Observe one prospectively ordered composite unit:
    /// `X_t = 1` when `judged_harm || unresolved`, `X_t = 0` otherwise.
    pub fn observe_unit(&mut self, judged_harm: bool, unresolved: bool) -> f64 {
        let flagged = judged_harm || unresolved;
        if judged_harm {
            self.judged_harm_units += 1;
        } else if unresolved {
            self.unresolved_units += 1;
        } else {
            self.unflagged_units += 1;
        }
        for (idx, &q) in MONITOR_ALTERNATIVES.iter().enumerate() {
            let step = if flagged {
                (q / self.tau).ln()
            } else {
                ((1.0 - q) / (1.0 - self.tau)).ln()
            };
            self.log_e[idx] += step;
        }
        let e_val = self.mixture_e_value();
        if e_val >= self.threshold() {
            self.alarm_triggered = true;
        }
        e_val
    }
}

/// Exhaustively enumerate all `2^horizon` Bernoulli paths under the boundary
/// conditional null `P(X_t = 1) = tau` and compute the exact probability of
/// ever crossing `1 / alpha_monitor` at any step `1 <= t <= horizon`.
///
/// By Ville's inequality for nonnegative supermartingales starting at 1, this
/// crossing probability is guaranteed to be `<= alpha_monitor`.
pub fn verify_supermartingale_ville_bound(
    horizon: usize,
    tau: f64,
    alpha_monitor: f64,
) -> Result<(f64, bool), LearningError> {
    if horizon == 0 || horizon > 20 {
        return Err(LearningError::MonitorInvalid(
            "exhaustive Bernoulli path horizon must be in 1..=20".into(),
        ));
    }
    let mut crossing_probability = 0.0_f64;
    let total_paths = 1_u64 << horizon;
    for mask in 0..total_paths {
        let mut acc = CompositeRiskAccumulator::with_tau(tau, alpha_monitor)?;
        let mut path_prob = 1.0_f64;
        let mut crossed = false;
        for step in 0..horizon {
            let x_t = ((mask >> step) & 1) == 1;
            path_prob *= if x_t { tau } else { 1.0 - tau };
            acc.observe_unit(x_t, false);
            if acc.alarm_triggered {
                crossed = true;
            }
        }
        if crossed {
            crossing_probability += path_prob;
        }
    }
    Ok((crossing_probability, crossing_probability <= alpha_monitor))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FinalizedMonitorUnit {
    pub unit_id: String,
    pub label_revision: u64,
    pub judged_harm: bool,
    pub unresolved: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HistoricalEpochTrace {
    pub epoch_id: u64,
    pub allocated_alpha: f64,
    pub status: String,
    pub final_e_value: f64,
    pub alarm_triggered: bool,
    pub judged_harm_units: usize,
    pub unresolved_units: usize,
    pub unflagged_units: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MonitorLedgerState {
    pub schema: String,
    pub status: String,
    pub current_epoch: u64,
    pub alpha_total: f64,
    pub alpha_spent: f64,
    pub current_epoch_alpha: f64,
    pub tau: f64,
    pub log_e: [f64; 3],
    pub e_value: f64,
    pub threshold: f64,
    pub alarm_triggered: bool,
    pub judged_harm_units: usize,
    pub unresolved_units: usize,
    pub unflagged_units: usize,
    pub prespecified_order: Vec<String>,
    pub next_order_index: usize,
    pub finalized_units: BTreeMap<String, FinalizedMonitorUnit>,
    pub historical_epochs: Vec<HistoricalEpochTrace>,
    pub interpretation_notice: String,
    pub recommended_action: String,
}

impl MonitorLedgerState {
    pub fn initialize(
        alpha_total: f64,
        first_epoch_alpha: f64,
        prespecified_order: Vec<String>,
    ) -> Result<Self, LearningError> {
        if !(0.0..1.0).contains(&alpha_total) || !alpha_total.is_finite() || alpha_total == 0.0 {
            return Err(LearningError::MonitorInvalid(
                "alpha_total must be finite in (0, 1)".into(),
            ));
        }
        if first_epoch_alpha <= 0.0
            || !first_epoch_alpha.is_finite()
            || first_epoch_alpha > alpha_total
        {
            return Err(LearningError::AlphaBudgetExhausted {
                requested: format!("{first_epoch_alpha:.6}"),
                remaining: format!("{alpha_total:.6}"),
            });
        }
        if prespecified_order.is_empty() {
            return Err(LearningError::MonitorInvalid(
                "prespecified_order must be non-empty".into(),
            ));
        }
        let mut unique = BTreeSet::new();
        for unit in &prespecified_order {
            if unit.trim().is_empty() || !unique.insert(unit.as_str()) {
                return Err(LearningError::MonitorInvalid(
                    "prespecified_order must contain unique non-empty unit IDs".into(),
                ));
            }
        }
        Ok(Self {
            schema: MONITOR_SCHEMA.to_owned(),
            status: "active".to_owned(),
            current_epoch: 1,
            alpha_total,
            alpha_spent: first_epoch_alpha,
            current_epoch_alpha: first_epoch_alpha,
            tau: DEFAULT_MONITOR_TAU,
            log_e: [0.0; 3],
            e_value: 1.0,
            threshold: 1.0 / first_epoch_alpha,
            alarm_triggered: false,
            judged_harm_units: 0,
            unresolved_units: 0,
            unflagged_units: 0,
            prespecified_order,
            next_order_index: 0,
            finalized_units: BTreeMap::new(),
            historical_epochs: Vec::new(),
            interpretation_notice:
                "E_t is a composite-risk supermartingale e-value, not a posterior probability; absence of alarm is not certification that risk <= tau."
                    .to_owned(),
            recommended_action: "none".to_owned(),
        })
    }

    pub fn remaining_alpha(&self) -> f64 {
        (self.alpha_total - self.alpha_spent).max(0.0)
    }

    /// Finalize a unit or detect a post-finalization label correction.
    ///
    /// - Units must arrive in the prespecified prospective order (`prespecified_order[next_order_index]`),
    ///   unless revising an already-finalized unit.
    /// - Revising an already-finalized unit with a changed outcome invalidates
    ///   the current epoch (`status = "invalidated"`), preserves historical
    ///   trace/alarms and nonrefundable `alpha_spent`, and suspends inference
    ///   until `restart_epoch` allocates fresh remaining alpha.
    pub fn record_unit(&mut self, unit: FinalizedMonitorUnit) -> Result<(), LearningError> {
        if let Some(existing) = self.finalized_units.get(&unit.unit_id) {
            if unit.label_revision < existing.label_revision {
                return Err(LearningError::MonitorInvalid(format!(
                    "stale label_revision {} < {} for unit {}",
                    unit.label_revision, existing.label_revision, unit.unit_id
                )));
            }
            if unit.label_revision == existing.label_revision
                && unit.judged_harm == existing.judged_harm
                && unit.unresolved == existing.unresolved
            {
                return Ok(());
            }
            // Post-finalization correction invalidates the current epoch without refunding alpha.
            self.historical_epochs.push(HistoricalEpochTrace {
                epoch_id: self.current_epoch,
                allocated_alpha: self.current_epoch_alpha,
                status: "invalidated".to_owned(),
                final_e_value: self.e_value,
                alarm_triggered: self.alarm_triggered,
                judged_harm_units: self.judged_harm_units,
                unresolved_units: self.unresolved_units,
                unflagged_units: self.unflagged_units,
            });
            self.finalized_units.insert(unit.unit_id.clone(), unit);
            self.status = "invalidated".to_owned();
            self.recommended_action =
                "epoch_invalidated_by_post_finalization_correction; allocate_new_epoch_for_prospective_units"
                    .to_owned();
            return Ok(());
        }

        if self.status != "active" && self.status != "alarmed" {
            return Err(LearningError::MonitorInvalid(format!(
                "cannot record prospective unit while monitor status is '{}'",
                self.status
            )));
        }
        let expected_unit = self
            .prespecified_order
            .get(self.next_order_index)
            .ok_or_else(|| {
                LearningError::MonitorInvalid(
                    "all prespecified units in this epoch have already been finalized".into(),
                )
            })?;
        if &unit.unit_id != expected_unit {
            return Err(LearningError::MonitorInvalid(format!(
                "out-of-order unit finalization: expected {}, got {}",
                expected_unit, unit.unit_id
            )));
        }

        let mut acc = CompositeRiskAccumulator {
            tau: self.tau,
            alpha_monitor: self.current_epoch_alpha,
            log_e: self.log_e,
            judged_harm_units: self.judged_harm_units,
            unresolved_units: self.unresolved_units,
            unflagged_units: self.unflagged_units,
            alarm_triggered: self.alarm_triggered,
        };
        self.e_value = acc.observe_unit(unit.judged_harm, unit.unresolved);
        self.log_e = acc.log_e;
        self.judged_harm_units = acc.judged_harm_units;
        self.unresolved_units = acc.unresolved_units;
        self.unflagged_units = acc.unflagged_units;
        if acc.alarm_triggered {
            self.alarm_triggered = true;
            self.status = "alarmed".to_owned();
            self.recommended_action =
                "block_promotion_and_return_to_baseline_or_shadow_via_explicit_apply".to_owned();
        }
        self.finalized_units.insert(unit.unit_id.clone(), unit);
        self.next_order_index += 1;
        Ok(())
    }

    /// Start a new prospective epoch with a fresh nonrefundable slice of the
    /// remaining total alpha budget (`alpha_spent + new_alpha <= alpha_total`).
    /// Never refunds previously spent alpha or erases historical alarms.
    pub fn start_new_epoch(
        &mut self,
        new_alpha: f64,
        prespecified_order: Vec<String>,
    ) -> Result<(), LearningError> {
        if new_alpha <= 0.0 || !new_alpha.is_finite() {
            return Err(LearningError::MonitorInvalid(
                "new_alpha must be positive and finite".into(),
            ));
        }
        let next_spent = self.alpha_spent + new_alpha;
        if next_spent > self.alpha_total + 1e-15 {
            return Err(LearningError::AlphaBudgetExhausted {
                requested: format!("{new_alpha:.6}"),
                remaining: format!("{:.6}", self.remaining_alpha()),
            });
        }
        if prespecified_order.is_empty() {
            return Err(LearningError::MonitorInvalid(
                "new epoch prespecified_order must be non-empty".into(),
            ));
        }
        let mut unique = BTreeSet::new();
        for unit in &prespecified_order {
            if unit.trim().is_empty() || !unique.insert(unit.as_str()) {
                return Err(LearningError::MonitorInvalid(
                    "new epoch prespecified_order must contain unique non-empty unit IDs".into(),
                ));
            }
        }
        if self.status != "invalidated" {
            self.historical_epochs.push(HistoricalEpochTrace {
                epoch_id: self.current_epoch,
                allocated_alpha: self.current_epoch_alpha,
                status: self.status.clone(),
                final_e_value: self.e_value,
                alarm_triggered: self.alarm_triggered,
                judged_harm_units: self.judged_harm_units,
                unresolved_units: self.unresolved_units,
                unflagged_units: self.unflagged_units,
            });
        }
        self.current_epoch += 1;
        self.alpha_spent = next_spent;
        self.current_epoch_alpha = new_alpha;
        self.status = "active".to_owned();
        self.log_e = [0.0; 3];
        self.e_value = 1.0;
        self.threshold = 1.0 / new_alpha;
        self.alarm_triggered = false;
        self.judged_harm_units = 0;
        self.unresolved_units = 0;
        self.unflagged_units = 0;
        self.prespecified_order = prespecified_order;
        self.next_order_index = 0;
        self.finalized_units.clear();
        self.recommended_action = "none".to_owned();
        Ok(())
    }
}

pub fn save_monitor_state(path: &Path, state: &MonitorLedgerState) -> Result<(), LearningError> {
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|e| LearningError::Storage(e.to_string()))?;
    write_private_atomic(path, &bytes)
}

pub fn load_monitor_state(path: &Path) -> Result<MonitorLedgerState, LearningError> {
    let text =
        read_bounded_regular_file_string(path, EVALUATION_DATASET_BYTES.max(), "monitor state")?;
    let val = strict_json(&text).map_err(|_| {
        LearningError::MonitorInvalid("corrupt or duplicate-key monitor state".into())
    })?;
    serde_json::from_value(val)
        .map_err(|e| LearningError::MonitorInvalid(format!("invalid monitor state schema: {e}")))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorRestartStep {
    pub new_alpha: f64,
    pub prespecified_order: Vec<String>,
    #[serde(default)]
    pub units: Vec<FinalizedMonitorUnit>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorBatchInput {
    pub schema_version: u32,
    pub alpha_total: f64,
    pub first_epoch_alpha: f64,
    pub prespecified_order: Vec<String>,
    #[serde(default)]
    pub units: Vec<FinalizedMonitorUnit>,
    #[serde(default)]
    pub restarts: Vec<MonitorRestartStep>,
    #[serde(default)]
    pub ville_horizon: Option<usize>,
}

pub fn run_monitor_from_bytes(
    bytes: &[u8],
    apply: bool,
    state_dir: &Path,
) -> Result<Value, LearningError> {
    if bytes.len() > EVALUATION_DATASET_BYTES.max() {
        return Err(LearningError::OversizedInput {
            len: bytes.len(),
            max: EVALUATION_DATASET_BYTES.max(),
        });
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| LearningError::MalformedInput("monitor input is not UTF-8".into()))?;
    let val = strict_json(text)?;
    let input: MonitorBatchInput = serde_json::from_value(val)
        .map_err(|e| LearningError::MalformedInput(format!("invalid monitor batch schema: {e}")))?;
    if input.schema_version != 1 {
        return Err(LearningError::MalformedInput(format!(
            "unsupported monitor batch schema_version {}",
            input.schema_version
        )));
    }
    let mut state = MonitorLedgerState::initialize(
        input.alpha_total,
        input.first_epoch_alpha,
        input.prespecified_order,
    )?;
    for unit in input.units {
        state.record_unit(unit)?;
    }
    for restart in input.restarts {
        state.start_new_epoch(restart.new_alpha, restart.prespecified_order)?;
        for unit in restart.units {
            state.record_unit(unit)?;
        }
    }
    let ville_verification = if let Some(horizon) = input.ville_horizon {
        let (prob, holds) =
            verify_supermartingale_ville_bound(horizon, state.tau, state.current_epoch_alpha)?;
        Some(json!({
            "horizon": horizon,
            "tau": state.tau,
            "alpha_monitor": state.current_epoch_alpha,
            "crossing_probability": prob,
            "ville_bound_satisfied": holds,
        }))
    } else {
        None
    };
    if apply {
        let monitor_path = state_dir.join("monitor_state.json");
        save_monitor_state(&monitor_path, &state)?;
    }
    let mut out =
        serde_json::to_value(&state).map_err(|e| LearningError::Storage(e.to_string()))?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert("applied".into(), json!(apply));
        obj.insert("remaining_alpha".into(), json!(state.remaining_alpha()));
        obj.insert("ville_verification".into(), json!(ville_verification));
    }
    Ok(out)
}
