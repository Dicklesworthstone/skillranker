//! Consented evaluation frames, bounded streaming, and safe label resolution (P5, sr-roadmap-l1i.6.17).
//!
//! Provides:
//! 1. Bounded streaming import for evaluation case frames and judged labels.
//! 2. Safe label revision resolution ensuring monotonic revision ordering and detecting collisions.
//! 3. Cardinality verification: one-to-one or many-to-one join with strict pre/post count reconciliation.
//! 4. Split isolation ensuring task families are never contaminated across evaluation splits.
//! 5. Null-key and unmatched case preservation (never silently dropped or fabricated).
//! 6. Independent metrics computation with honest denominators and explicit unknown/unjudged tracking.

pub mod batch;
pub mod design_weighted;
pub mod numerics;
pub mod review;
pub mod sampling;
pub mod stratified;

pub use batch::*;
pub use design_weighted::*;
pub use numerics::{
    BACKEND_PROVENANCE, BackendProvenance, BetaDist, NumericsError, clopper_pearson_ci,
    clopper_pearson_one_sided_upper, ln_beta, log_gamma, logsumexp, standard_normal_ppf, wilson_ci,
    zero_event_upper_bound,
};
pub use sampling::{Pcg64Dxsm, SamplingError, choice_indices, shuffle_slice};
pub use stratified::{
    AllocationMethod, DesignStatus, EstimandWeighting, FamilyRepresentativeRule,
    FrozenSampleManifest, InputQuality, ObservableStratum, RandomizationProvenance,
    RetrievalProfile, SampledCaseEntry, StratumAllocation, allocate_sample_sizes,
    compute_frame_digest, draw_os_seed, draw_stratified_sample, partition_strata,
    replay_manifest_sample, select_family_representatives, verify_manifest_against_frame,
};

use crate::limits::{EVALUATION_CASE_RECORDS, EVALUATION_DATASET_BYTES, EVALUATION_DATASET_DEPTH};
use crate::output::{ErrorKind, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::BufRead;

/// Evaluation error kinds with structured diagnostics.
#[derive(Debug)]
pub enum EvaluationError {
    OversizedDataset {
        len: usize,
        max: usize,
    },
    RecordLimitReached {
        count: usize,
        max: usize,
    },
    ExcessiveDepth,
    InvalidJson(String),
    DuplicateKey(String),
    UnsupportedVersion(u64),
    InvalidField(String),
    CardinalityViolation(String),
    SplitContamination(String),
    SamplingFailure(String),
    InsufficientSampleBudgetForStrata {
        budget: usize,
        required_floor: usize,
    },
    InvalidStratumAllocation(String),
    FrameDigestMismatch {
        expected: String,
        actual: String,
    },
    ManifestVerificationFailure(String),
    Io(std::io::Error),
}

impl EvaluationError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::OversizedDataset { .. } | Self::RecordLimitReached { .. } => {
                ErrorKind::OversizedInput
            }
            Self::ExcessiveDepth
            | Self::InvalidJson(_)
            | Self::DuplicateKey(_)
            | Self::UnsupportedVersion(_)
            | Self::InvalidField(_)
            | Self::FrameDigestMismatch { .. }
            | Self::ManifestVerificationFailure(_)
            | Self::CardinalityViolation(_) => ErrorKind::MalformedInput,
            Self::SplitContamination(_) | Self::InvalidStratumAllocation(_) => {
                ErrorKind::InvalidConfiguration
            }
            Self::SamplingFailure(_) | Self::InsufficientSampleBudgetForStrata { .. } => {
                ErrorKind::InvalidUsage
            }
            Self::Io(err) => match err.kind() {
                std::io::ErrorKind::NotFound => ErrorKind::InvalidUsage,
                std::io::ErrorKind::PermissionDenied => ErrorKind::MalformedInput,
                _ => ErrorKind::StorageFailure,
            },
        }
    }
}

impl fmt::Display for EvaluationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OversizedDataset { len, max } => {
                write!(f, "evaluation dataset exceeds size limit ({len} > {max})")
            }
            Self::RecordLimitReached { count, max } => {
                write!(f, "evaluation record limit exceeded ({count} > {max})")
            }
            Self::ExcessiveDepth => f.write_str("evaluation input exceeds maximum nesting depth"),
            Self::InvalidJson(msg) => write!(f, "invalid evaluation JSON: {msg}"),
            Self::DuplicateKey(key) => write!(f, "duplicate JSON key in evaluation record: {key}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported evaluation schema version: {v}"),
            Self::InvalidField(msg) => write!(f, "invalid evaluation field: {msg}"),
            Self::CardinalityViolation(msg) => {
                write!(f, "evaluation join cardinality violation: {msg}")
            }
            Self::SplitContamination(msg) => {
                write!(f, "evaluation split contamination: {msg}")
            }
            Self::SamplingFailure(msg) => write!(f, "evaluation sampling failure: {msg}"),
            Self::InsufficientSampleBudgetForStrata {
                budget,
                required_floor,
            } => {
                write!(
                    f,
                    "insufficient sample budget ({budget}) to satisfy stratum floor requirements ({required_floor})"
                )
            }
            Self::InvalidStratumAllocation(msg) => {
                write!(f, "invalid stratum allocation: {msg}")
            }
            Self::FrameDigestMismatch { expected, actual } => {
                write!(
                    f,
                    "evaluation frame digest mismatch (expected {expected}, actual {actual})"
                )
            }
            Self::ManifestVerificationFailure(msg) => {
                write!(
                    f,
                    "manifest verification failed against evaluation frame: {msg}"
                )
            }
            Self::Io(err) => write!(f, "evaluation I/O error: {err}"),
        }
    }
}

impl std::error::Error for EvaluationError {}

impl From<std::io::Error> for EvaluationError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Evaluation dataset split partitions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvaluationSplit {
    Train,
    Validation,
    Holdout,
    Adversarial,
}

impl fmt::Display for EvaluationSplit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Train => "train",
            Self::Validation => "validation",
            Self::Holdout => "holdout",
            Self::Adversarial => "adversarial",
        })
    }
}

/// Stable compound identifier for an evaluation case run.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct CaseKey {
    pub frame_id: String,
    pub family_id: String,
    pub case_id: String,
    pub replicate: u32,
    pub policy_id: String,
}

impl CaseKey {
    pub fn new(
        frame_id: impl Into<String>,
        family_id: impl Into<String>,
        case_id: impl Into<String>,
        replicate: u32,
        policy_id: impl Into<String>,
    ) -> Self {
        Self {
            frame_id: frame_id.into(),
            family_id: family_id.into(),
            case_id: case_id.into(),
            replicate,
            policy_id: policy_id.into(),
        }
    }

    /// True if any primary identifier component is empty.
    pub fn is_null(&self) -> bool {
        self.frame_id.trim().is_empty()
            || self.family_id.trim().is_empty()
            || self.case_id.trim().is_empty()
    }
}

/// A recorded evaluation case candidate and decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationCaseRecord {
    pub schema_version: u64,
    pub key: CaseKey,
    pub split: EvaluationSplit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_summary: Option<String>,
    #[serde(default)]
    pub roster_skills: Vec<String>,
    pub decision: String,
    #[serde(default)]
    pub suggested_skills: Vec<String>,
    #[serde(default)]
    pub fits: BTreeMap<String, f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_score: Option<f64>,
    #[serde(default)]
    pub relevance_abstention: bool,
    #[serde(default)]
    pub operational_failure: bool,
}

/// Ground-truth label independently adjudicated for a case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JudgedLabel {
    pub schema_version: u64,
    pub case_id: String,
    pub revision: u64,
    #[serde(default)]
    pub acceptable_skills: BTreeSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_directive: Option<String>,
    #[serde(default)]
    pub no_skill_needed: bool,
    #[serde(default)]
    pub constraints: Vec<String>,
    pub adjudicator: String,
    pub created_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Label association status after join.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum LabelStatus {
    Resolved { label: JudgedLabel },
    Unmatched,
    NullKey,
    Ambiguous { revisions: Vec<u64> },
}

/// Classification of a case's advice outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelevanceClass {
    ExplicitMatch,
    ExplicitMismatch,
    TruePositive,
    FalsePositive,
    NeedlessSuggestion,
    TrueAbstain,
    FalseAbstain,
    OperationalFailure,
    Unjudged,
}

impl RelevanceClass {
    /// Tuning loss from the frozen `evaluation_policy.v1` table. Explicit
    /// requests are checked separately from advisory quality, and an unjudged
    /// case has no loss; both are `None`, never an invented zero.
    #[must_use]
    pub const fn policy_loss(self) -> Option<u32> {
        match self {
            Self::TruePositive | Self::TrueAbstain => Some(0),
            Self::FalseAbstain => Some(1),
            Self::FalsePositive | Self::NeedlessSuggestion | Self::OperationalFailure => Some(2),
            Self::ExplicitMatch | Self::ExplicitMismatch | Self::Unjudged => None,
        }
    }
}

/// Case record paired with its resolved ground-truth label and evaluation outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedEvaluationCase {
    pub record: EvaluationCaseRecord,
    pub label_status: LabelStatus,
    pub relevance_class: RelevanceClass,
    pub top1_match: bool,
    pub candidate_coverage: bool,
    pub set_recall: Option<f64>,
}

/// Reconciliation manifest verifying pre/post join integrity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReconciliationManifest {
    pub pre_join_case_count: usize,
    pub post_join_case_count: usize,
    pub resolved_labels_count: usize,
    pub unmatched_cases_count: usize,
    pub null_key_cases_count: usize,
    pub families_count: usize,
    pub splits_distribution: BTreeMap<String, usize>,
    pub reconciled: bool,
}

/// Aggregate performance metrics calculated over a resolved evaluation frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationMetrics {
    pub total_cases: usize,
    pub judged_cases: usize,
    pub unjudged_cases: usize,
    pub operational_failures: usize,
    pub explicit_cases: usize,
    pub advisory_cases: usize,
    pub positive_advisory_cases: usize,
    pub no_match_advisory_cases: usize,
    pub candidate_coverage_rate: Option<f64>,
    pub top1_precision: Option<f64>,
    pub positive_suggestion_rate: Option<f64>,
    pub needless_suggestion_rate: Option<f64>,
    pub false_abstention_rate: Option<f64>,
}

/// Parse bounded JSON with nesting depth limit and duplicate key detection.
pub fn parse_bounded_json(bytes: &[u8], max_depth: usize) -> Result<Value, EvaluationError> {
    if bytes.is_empty() {
        return Err(EvaluationError::InvalidJson("empty input".into()));
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;

    for &b in bytes {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match b {
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                if depth > max_depth {
                    return Err(EvaluationError::ExcessiveDepth);
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    let text = std::str::from_utf8(bytes)
        .map_err(|e| EvaluationError::InvalidJson(format!("invalid UTF-8: {e}")))?;
    let value: Value =
        serde_json::from_str(text).map_err(|e| EvaluationError::InvalidJson(e.to_string()))?;
    check_no_duplicate_keys(text)?;
    Ok(value)
}

fn check_no_duplicate_keys(json_str: &str) -> Result<(), EvaluationError> {
    let mut parser = json_strip_parser::JsonKeyTracker::new();
    parser.scan(json_str)
}

mod json_strip_parser {
    use super::EvaluationError;
    use std::collections::BTreeSet;

    pub struct JsonKeyTracker {
        stack: Vec<BTreeSet<String>>,
    }

    impl JsonKeyTracker {
        pub fn new() -> Self {
            Self { stack: Vec::new() }
        }

        pub fn scan(&mut self, json_str: &str) -> Result<(), EvaluationError> {
            let mut in_string = false;
            let mut escape = false;
            let mut collecting_key = false;
            let mut current_key = String::new();
            let mut expect_colon = false;

            let chars: Vec<char> = json_str.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                if escape {
                    if collecting_key {
                        current_key.push(c);
                    }
                    escape = false;
                    i += 1;
                    continue;
                }
                if c == '\\' && in_string {
                    escape = true;
                    i += 1;
                    continue;
                }
                if c == '"' {
                    in_string = !in_string;
                    if in_string {
                        if !expect_colon && self.stack.last().is_some() {
                            collecting_key = true;
                            current_key.clear();
                        }
                    } else if collecting_key {
                        collecting_key = false;
                        expect_colon = true;
                    }
                    i += 1;
                    continue;
                }
                if in_string {
                    if collecting_key {
                        current_key.push(c);
                    }
                    i += 1;
                    continue;
                }
                match c {
                    '{' => {
                        self.stack.push(BTreeSet::new());
                        expect_colon = false;
                    }
                    '}' => {
                        self.stack.pop();
                        expect_colon = false;
                    }
                    ':' if expect_colon => {
                        if let Some(set) = self.stack.last_mut()
                            && !set.insert(current_key.clone())
                        {
                            return Err(EvaluationError::DuplicateKey(current_key));
                        }
                        expect_colon = false;
                    }
                    ',' => {
                        expect_colon = false;
                    }
                    _ => {}
                }
                i += 1;
            }
            Ok(())
        }
    }
}

/// Bound the read itself: an unterminated line must not allocate beyond the
/// remaining dataset allowance. One lookahead byte distinguishes exact EOF
/// from oversized input. Count blank lines and delimiters against the same cap.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    total_bytes: &mut usize,
    max_bytes: usize,
) -> Result<usize, EvaluationError> {
    line.clear();
    let remaining = max_bytes.saturating_sub(*total_bytes);
    let mut bounded = std::io::Read::take(reader, remaining.saturating_add(1) as u64);
    let read = bounded.read_line(line)?;
    *total_bytes = total_bytes.saturating_add(read);
    if *total_bytes > max_bytes {
        return Err(EvaluationError::OversizedDataset {
            len: *total_bytes,
            max: max_bytes,
        });
    }
    Ok(read)
}

/// Stream evaluation case records from a reader with size, record count, and depth bounds.
pub fn parse_case_records_streaming<R: BufRead>(
    mut reader: R,
) -> Result<Vec<EvaluationCaseRecord>, EvaluationError> {
    let mut records = Vec::new();
    let mut total_bytes = 0usize;
    let mut line = String::new();

    while read_bounded_line(
        &mut reader,
        &mut line,
        &mut total_bytes,
        EVALUATION_DATASET_BYTES.max(),
    )? > 0
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        if records.len() >= EVALUATION_CASE_RECORDS.max() {
            return Err(EvaluationError::RecordLimitReached {
                count: records.len() + 1,
                max: EVALUATION_CASE_RECORDS.max(),
            });
        }
        let value = parse_bounded_json(trimmed.as_bytes(), EVALUATION_DATASET_DEPTH.max())?;
        let record: EvaluationCaseRecord = serde_json::from_value(value)
            .map_err(|e| EvaluationError::InvalidField(format!("malformed case record: {e}")))?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(EvaluationError::UnsupportedVersion(record.schema_version));
        }
        records.push(record);
        line.clear();
    }

    Ok(records)
}

/// Stream judged labels from a reader with size, record count, and depth bounds.
pub fn parse_labels_streaming<R: BufRead>(
    mut reader: R,
) -> Result<Vec<JudgedLabel>, EvaluationError> {
    let mut labels = Vec::new();
    let mut total_bytes = 0usize;
    let mut line = String::new();

    while read_bounded_line(
        &mut reader,
        &mut line,
        &mut total_bytes,
        EVALUATION_DATASET_BYTES.max(),
    )? > 0
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        if labels.len() >= EVALUATION_CASE_RECORDS.max() {
            return Err(EvaluationError::RecordLimitReached {
                count: labels.len() + 1,
                max: EVALUATION_CASE_RECORDS.max(),
            });
        }
        let value = parse_bounded_json(trimmed.as_bytes(), EVALUATION_DATASET_DEPTH.max())?;
        let label: JudgedLabel = serde_json::from_value(value)
            .map_err(|e| EvaluationError::InvalidField(format!("malformed judged label: {e}")))?;
        if label.schema_version != SCHEMA_VERSION {
            return Err(EvaluationError::UnsupportedVersion(label.schema_version));
        }
        labels.push(label);
        line.clear();
    }

    Ok(labels)
}

/// Verify that no task family (`family_id`) spans across multiple evaluation splits.
pub fn verify_split_isolation(cases: &[EvaluationCaseRecord]) -> Result<(), EvaluationError> {
    let mut family_to_split: BTreeMap<&str, EvaluationSplit> = BTreeMap::new();
    for case in cases {
        if case.key.is_null() {
            continue;
        }
        let fam = case.key.family_id.as_str();
        if let Some(&existing_split) = family_to_split.get(fam) {
            if existing_split != case.split {
                return Err(EvaluationError::SplitContamination(format!(
                    "task family '{}' spans both {:?} and {:?}",
                    fam, existing_split, case.split
                )));
            }
        } else {
            family_to_split.insert(fam, case.split);
        }
    }
    Ok(())
}

/// Resolve ground-truth labels by case_id, selecting the latest valid revision.
/// Detects duplicate labels at the same revision with conflicting contents (cardinality violation).
pub fn resolve_label_revisions(
    labels: &[JudgedLabel],
) -> Result<BTreeMap<String, JudgedLabel>, EvaluationError> {
    let mut revisions_by_case: BTreeMap<String, BTreeMap<u64, JudgedLabel>> = BTreeMap::new();

    for label in labels {
        if label.case_id.trim().is_empty() {
            return Err(EvaluationError::InvalidField(
                "label case_id cannot be empty".into(),
            ));
        }
        let case_map = revisions_by_case.entry(label.case_id.clone()).or_default();
        if let Some(existing) = case_map.get(&label.revision) {
            if existing != label {
                return Err(EvaluationError::CardinalityViolation(format!(
                    "conflicting labels for case '{}' at revision {}",
                    label.case_id, label.revision
                )));
            }
        } else {
            case_map.insert(label.revision, label.clone());
        }
    }

    let mut resolved = BTreeMap::new();
    for (case_id, rev_map) in revisions_by_case {
        // Select highest revision monotonically
        if let Some((_, highest_label)) = rev_map.into_iter().next_back() {
            resolved.insert(case_id, highest_label);
        }
    }

    Ok(resolved)
}

/// Join case records with ground-truth labels.
///
/// Guarantees:
/// 1. Split isolation (task families never span splits).
/// 2. Safe monotonic label resolution.
/// 3. Strict cardinality checks (one-to-one or many-to-one, never many-to-many).
/// 4. Zero dropped or fabricated records: pre-join case count == post-join case count.
/// 5. Null-key and unmatched cases are preserved as explicit unknown states.
pub fn join_evaluation_frame(
    cases: &[EvaluationCaseRecord],
    labels: &[JudgedLabel],
) -> Result<(Vec<ResolvedEvaluationCase>, ReconciliationManifest), EvaluationError> {
    verify_split_isolation(cases)?;
    let resolved_labels = resolve_label_revisions(labels)?;

    let pre_join_case_count = cases.len();
    let mut resolved_cases = Vec::with_capacity(pre_join_case_count);
    let mut unmatched_cases_count = 0usize;
    let mut null_key_cases_count = 0usize;
    let mut distinct_families = BTreeSet::new();
    let mut splits_distribution = BTreeMap::new();

    for case in cases {
        *splits_distribution
            .entry(case.split.to_string())
            .or_insert(0usize) += 1;

        if case.key.is_null() {
            null_key_cases_count += 1;
            resolved_cases.push(ResolvedEvaluationCase {
                record: case.clone(),
                label_status: LabelStatus::NullKey,
                relevance_class: RelevanceClass::Unjudged,
                top1_match: false,
                candidate_coverage: false,
                set_recall: None,
            });
            continue;
        }

        distinct_families.insert(case.key.family_id.clone());

        match resolved_labels.get(&case.key.case_id) {
            Some(label) => {
                let outcome = classify_relevance(case, label);
                resolved_cases.push(ResolvedEvaluationCase {
                    record: case.clone(),
                    label_status: LabelStatus::Resolved {
                        label: label.clone(),
                    },
                    relevance_class: outcome.relevance_class,
                    top1_match: outcome.top1_match,
                    candidate_coverage: outcome.candidate_coverage,
                    set_recall: outcome.set_recall,
                });
            }
            None => {
                unmatched_cases_count += 1;
                resolved_cases.push(ResolvedEvaluationCase {
                    record: case.clone(),
                    label_status: LabelStatus::Unmatched,
                    relevance_class: if case.operational_failure {
                        RelevanceClass::OperationalFailure
                    } else {
                        RelevanceClass::Unjudged
                    },
                    top1_match: false,
                    candidate_coverage: false,
                    set_recall: None,
                });
            }
        }
    }

    let post_join_case_count = resolved_cases.len();
    let reconciled = pre_join_case_count == post_join_case_count;

    let manifest = ReconciliationManifest {
        pre_join_case_count,
        post_join_case_count,
        resolved_labels_count: resolved_labels.len(),
        unmatched_cases_count,
        null_key_cases_count,
        families_count: distinct_families.len(),
        splits_distribution,
        reconciled,
    };

    Ok((resolved_cases, manifest))
}

struct CaseClassification {
    relevance_class: RelevanceClass,
    top1_match: bool,
    candidate_coverage: bool,
    set_recall: Option<f64>,
}

fn classify_relevance(case: &EvaluationCaseRecord, label: &JudgedLabel) -> CaseClassification {
    if case.operational_failure {
        return CaseClassification {
            relevance_class: RelevanceClass::OperationalFailure,
            top1_match: false,
            candidate_coverage: false,
            set_recall: None,
        };
    }

    // Explicit request handling
    if let Some(directive) = &label.explicit_directive {
        let is_match = case.suggested_skills.first() == Some(directive)
            || (case.decision == "explicit" && case.suggested_skills.contains(directive));
        return CaseClassification {
            relevance_class: if is_match {
                RelevanceClass::ExplicitMatch
            } else {
                RelevanceClass::ExplicitMismatch
            },
            top1_match: is_match,
            candidate_coverage: is_match,
            set_recall: Some(if is_match { 1.0 } else { 0.0 }),
        };
    }

    let top1 = case.suggested_skills.first();
    let y = &label.acceptable_skills;
    let is_y_empty = y.is_empty() || label.no_skill_needed;

    let top1_in_y = top1.is_some_and(|s| y.contains(s));
    let coverage = case.suggested_skills.iter().any(|s| y.contains(s));
    let set_recall = if is_y_empty {
        None
    } else {
        let found = case
            .suggested_skills
            .iter()
            .filter(|s| y.contains(*s))
            .count();
        Some(found as f64 / y.len() as f64)
    };

    let relevance_class = if is_y_empty {
        if top1.is_some() {
            RelevanceClass::NeedlessSuggestion
        } else {
            RelevanceClass::TrueAbstain
        }
    } else if top1_in_y {
        RelevanceClass::TruePositive
    } else if case.relevance_abstention || case.decision == "abstain" {
        RelevanceClass::FalseAbstain
    } else {
        RelevanceClass::FalsePositive
    };

    CaseClassification {
        relevance_class,
        top1_match: top1_in_y,
        candidate_coverage: coverage,
        set_recall,
    }
}

/// Compute aggregate evaluation metrics over resolved evaluation cases.
pub fn compute_metrics(resolved: &[ResolvedEvaluationCase]) -> EvaluationMetrics {
    let mut total_cases = 0usize;
    let mut judged_cases = 0usize;
    let mut unjudged_cases = 0usize;
    let mut operational_failures = 0usize;
    let mut explicit_cases = 0usize;
    let mut advisory_cases = 0usize;
    let mut positive_advisory_cases = 0usize;
    let mut no_match_advisory_cases = 0usize;

    let mut covered_positive_cases = 0usize;
    let mut emitted_advisory_suggestions = 0usize;
    let mut emitted_true_positives = 0usize;
    let mut positive_suggestions_emitted = 0usize;
    let mut needless_suggestions = 0usize;
    let mut false_abstentions = 0usize;

    for c in resolved {
        total_cases += 1;

        if matches!(
            c.label_status,
            LabelStatus::Unmatched | LabelStatus::NullKey
        ) {
            unjudged_cases += 1;
            if c.record.operational_failure {
                operational_failures += 1;
            }
            continue;
        }

        judged_cases += 1;

        if c.record.operational_failure {
            operational_failures += 1;
            continue;
        }

        if matches!(
            c.relevance_class,
            RelevanceClass::ExplicitMatch | RelevanceClass::ExplicitMismatch
        ) {
            explicit_cases += 1;
            continue;
        }

        advisory_cases += 1;

        match c.relevance_class {
            RelevanceClass::TruePositive => {
                positive_advisory_cases += 1;
                emitted_advisory_suggestions += 1;
                emitted_true_positives += 1;
                positive_suggestions_emitted += 1;
                if c.candidate_coverage {
                    covered_positive_cases += 1;
                }
            }
            RelevanceClass::FalsePositive => {
                positive_advisory_cases += 1;
                emitted_advisory_suggestions += 1;
                if c.candidate_coverage {
                    covered_positive_cases += 1;
                }
            }
            RelevanceClass::FalseAbstain => {
                positive_advisory_cases += 1;
                false_abstentions += 1;
                if c.candidate_coverage {
                    covered_positive_cases += 1;
                }
            }
            RelevanceClass::NeedlessSuggestion => {
                no_match_advisory_cases += 1;
                emitted_advisory_suggestions += 1;
                needless_suggestions += 1;
            }
            RelevanceClass::TrueAbstain => {
                no_match_advisory_cases += 1;
            }
            _ => {}
        }
    }

    let candidate_coverage_rate = if positive_advisory_cases > 0 {
        Some(covered_positive_cases as f64 / positive_advisory_cases as f64)
    } else {
        None
    };

    let top1_precision = if emitted_advisory_suggestions > 0 {
        Some(emitted_true_positives as f64 / emitted_advisory_suggestions as f64)
    } else {
        None
    };

    let positive_suggestion_rate = if positive_advisory_cases > 0 {
        Some(positive_suggestions_emitted as f64 / positive_advisory_cases as f64)
    } else {
        None
    };

    let needless_suggestion_rate = if no_match_advisory_cases > 0 {
        Some(needless_suggestions as f64 / no_match_advisory_cases as f64)
    } else {
        None
    };

    let false_abstention_rate = if positive_advisory_cases > 0 {
        Some(false_abstentions as f64 / positive_advisory_cases as f64)
    } else {
        None
    };

    EvaluationMetrics {
        total_cases,
        judged_cases,
        unjudged_cases,
        operational_failures,
        explicit_cases,
        advisory_cases,
        positive_advisory_cases,
        no_match_advisory_cases,
        candidate_coverage_rate,
        top1_precision,
        positive_suggestion_rate,
        needless_suggestion_rate,
        false_abstention_rate,
    }
}

#[cfg(test)]
mod streaming_bounds_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn unterminated_line_stops_after_one_lookahead_byte() {
        let mut input = Cursor::new(b"0123456789abcdef");
        let mut line = String::new();
        let mut total = 0;
        assert!(matches!(
            read_bounded_line(&mut input, &mut line, &mut total, 8),
            Err(EvaluationError::OversizedDataset { len: 9, max: 8 })
        ));
        assert_eq!(input.position(), 9);
        assert_eq!(line.len(), 9);
    }

    #[test]
    fn exact_limit_accepts_final_line_and_counts_blank_lines() {
        for bytes in [b"\n1234567".as_slice(), b"\n123456\n".as_slice()] {
            let mut input = Cursor::new(bytes);
            let mut line = String::new();
            let mut total = 0;
            assert_eq!(
                read_bounded_line(&mut input, &mut line, &mut total, 8).unwrap(),
                1
            );
            assert_eq!(
                read_bounded_line(&mut input, &mut line, &mut total, 8).unwrap(),
                7
            );
            assert_eq!(
                read_bounded_line(&mut input, &mut line, &mut total, 8).unwrap(),
                0
            );
            assert_eq!(total, 8);
        }
    }

    #[test]
    fn consumed_lines_reduce_the_next_line_allowance() {
        let mut input = Cursor::new(b"\n\n123456789abcdef");
        let mut line = String::new();
        let mut total = 0;
        for _ in 0..2 {
            assert_eq!(
                read_bounded_line(&mut input, &mut line, &mut total, 8).unwrap(),
                1
            );
        }
        assert!(matches!(
            read_bounded_line(&mut input, &mut line, &mut total, 8),
            Err(EvaluationError::OversizedDataset { len: 9, max: 8 })
        ));
        assert_eq!(line.len(), 7);
        assert_eq!(input.position(), 9);
    }
}
