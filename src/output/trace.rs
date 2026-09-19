//! Bounded stage traces for explainability and why-not diagnostics.
//!
//! Satisfies contract boundary `p4_explain_exclusion_stages` (sr-roadmap-l1i.5.14).
//!
//! Traces evaluate candidates across 8 ordered pipeline stages:
//! 1. discovery: whether the candidate was present in the snapshot.
//! 2. visibility: whether invocation is verified, shadowed, ambiguous, manual-only, or forbidden.
//! 3. local-policy: whether local policy excluded or already loaded the candidate.
//! 4. quill-admission: whether bounded Quill BM25 retrieval admitted the candidate.
//! 5. wide-shortlist: whether the candidate passed the wide gate and shortlist cutoff.
//! 6. fit-none: whether the candidate met the minimum fit threshold and beat the none option.
//! 7. ordering: whether the candidate was selected in the top-K ranking.
//! 8. publication: whether the candidate passed final publication revalidation.

use crate::identity::SkillId;
use serde::{Deserialize, Serialize};

use super::TraceCursor;

/// The 8 ordered stages in the ranking pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceStage {
    Discovery,
    Visibility,
    LocalPolicy,
    QuillAdmission,
    WideShortlist,
    FitNone,
    Ordering,
    Publication,
}

impl TraceStage {
    pub const ALL: [Self; 8] = [
        Self::Discovery,
        Self::Visibility,
        Self::LocalPolicy,
        Self::QuillAdmission,
        Self::WideShortlist,
        Self::FitNone,
        Self::Ordering,
        Self::Publication,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Visibility => "visibility",
            Self::LocalPolicy => "local-policy",
            Self::QuillAdmission => "quill-admission",
            Self::WideShortlist => "wide-shortlist",
            Self::FitNone => "fit-none",
            Self::Ordering => "ordering",
            Self::Publication => "publication",
        }
    }
}

/// Evaluation status of a candidate at a particular stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceStatus {
    Passed,
    Excluded,
    NotEvaluated,
    NotInSnapshot,
}

impl TraceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Excluded => "excluded",
            Self::NotEvaluated => "not-evaluated",
            Self::NotInSnapshot => "not-in-snapshot",
        }
    }
}

/// An evaluation entry for one skill at one pipeline stage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceEntry {
    pub skill_id: SkillId,
    pub stage: TraceStage,
    pub status: TraceStatus,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl TraceEntry {
    /// Constructs a passed stage entry with an identifier reason and optional value/threshold.
    pub fn passed(
        skill_id: SkillId,
        stage: TraceStage,
        reason: impl Into<String>,
        value: Option<f64>,
        threshold: Option<f64>,
    ) -> Self {
        Self {
            skill_id,
            stage,
            status: TraceStatus::Passed,
            value,
            threshold,
            reason: Some(reason.into()),
            hint: None,
        }
    }

    /// Constructs an excluded stage entry with an identifier reason, optional value/threshold,
    /// and an optional allowlisted recovery hint.
    pub fn excluded(
        skill_id: SkillId,
        stage: TraceStage,
        reason: impl Into<String>,
        value: Option<f64>,
        threshold: Option<f64>,
        hint: Option<String>,
    ) -> Self {
        Self {
            skill_id,
            stage,
            status: TraceStatus::Excluded,
            value,
            threshold,
            reason: Some(reason.into()),
            hint,
        }
    }

    /// Constructs an unevaluated stage entry with null value, threshold, and reason.
    pub fn not_evaluated(skill_id: SkillId, stage: TraceStage) -> Self {
        Self {
            skill_id,
            stage,
            status: TraceStatus::NotEvaluated,
            value: None,
            threshold: None,
            reason: None,
            hint: None,
        }
    }

    /// Constructs a not-in-snapshot discovery entry with null value, threshold, and reason.
    pub fn not_in_snapshot(skill_id: SkillId) -> Self {
        Self {
            skill_id,
            stage: TraceStage::Discovery,
            status: TraceStatus::NotInSnapshot,
            value: None,
            threshold: None,
            reason: None,
            hint: Some("inspect-roster".to_string()),
        }
    }
}

/// A bounded, versioned stage trace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageTrace {
    pub cursor: TraceCursor,
    pub total: u64,
    pub next_offset: Option<u64>,
    pub entries: Vec<TraceEntry>,
}
