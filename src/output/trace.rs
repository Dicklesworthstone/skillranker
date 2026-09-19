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

use crate::identity::{ContentHash, SkillId};
use serde::{Deserialize, Serialize};

use super::{ContractError, SCHEMA_VERSION, TraceCursor};

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
    #[serde(default)]
    pub next_cursor: Option<String>,
    pub entries: Vec<TraceEntry>,
}

impl StageTrace {
    /// Formats a continuation cursor token if there is a subsequent page.
    pub fn next_cursor(&self) -> Option<String> {
        self.next_cursor.clone()
    }
}

impl TraceCursor {
    pub const TOKEN_PREFIX: &'static str = "t1";

    /// Formats this cursor as a string token: `t1.<snapshot_hex>.<query_hex>.<offset>`.
    pub fn to_token(&self) -> String {
        format!(
            "{}.{}.{}.{}",
            Self::TOKEN_PREFIX,
            self.snapshot_id.as_str(),
            self.query_id.as_str(),
            self.offset
        )
    }

    /// Parses a trace cursor from a string token: `t1.<snapshot_hex>.<query_hex>.<offset>`.
    pub fn from_token(token: &str) -> Result<Self, ContractError> {
        let mut parts = token.split('.');
        let (Some(prefix), Some(snapshot_hex), Some(query_hex), Some(offset_str), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(ContractError::InvalidField);
        };
        if prefix != Self::TOKEN_PREFIX {
            return Err(ContractError::UnsupportedVersion);
        }
        let snapshot_id =
            ContentHash::parse(snapshot_hex).map_err(|_| ContractError::InvalidField)?;
        let query_id = ContentHash::parse(query_hex).map_err(|_| ContractError::InvalidField)?;
        let offset = offset_str
            .parse::<u64>()
            .map_err(|_| ContractError::InvalidField)?;
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            snapshot_id,
            query_id,
            offset,
        })
    }
}

/// Scope parameters that uniquely identify a frozen trace query.
///
/// Binds the user request text, target filter (`--why-not`), ranking policy thresholds,
/// size limits, and explicit requirement/exclusion directives into a deterministic hash.
#[derive(Clone, Debug)]
pub struct TraceQueryScope<'a> {
    pub request_text: &'a str,
    pub why_not: Option<&'a SkillId>,
    pub gate_threshold: f64,
    pub fits_threshold: f64,
    pub top: usize,
    pub shortlist: usize,
    pub require_skills: &'a [SkillId],
    pub exclude_skills: &'a [SkillId],
}

impl<'a> TraceQueryScope<'a> {
    /// Computes the deterministic, frozen ContentHash for this query scope.
    pub fn compute_id(&self) -> ContentHash {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"sr.trace-query.v1\0");
        bytes.extend_from_slice(&(self.request_text.len() as u64).to_le_bytes());
        bytes.extend_from_slice(self.request_text.as_bytes());

        match self.why_not {
            Some(id) => {
                bytes.push(1);
                bytes.extend_from_slice(&(id.as_str().len() as u64).to_le_bytes());
                bytes.extend_from_slice(id.as_str().as_bytes());
            }
            None => bytes.push(0),
        }

        bytes.extend_from_slice(&self.gate_threshold.to_bits().to_le_bytes());
        bytes.extend_from_slice(&self.fits_threshold.to_bits().to_le_bytes());
        bytes.extend_from_slice(&(self.top as u64).to_le_bytes());
        bytes.extend_from_slice(&(self.shortlist as u64).to_le_bytes());

        let mut sorted_req: Vec<&str> = self.require_skills.iter().map(|s| s.as_str()).collect();
        sorted_req.sort_unstable();
        bytes.extend_from_slice(&(sorted_req.len() as u64).to_le_bytes());
        for s in sorted_req {
            bytes.extend_from_slice(&(s.len() as u64).to_le_bytes());
            bytes.extend_from_slice(s.as_bytes());
        }

        let mut sorted_excl: Vec<&str> = self.exclude_skills.iter().map(|s| s.as_str()).collect();
        sorted_excl.sort_unstable();
        bytes.extend_from_slice(&(sorted_excl.len() as u64).to_le_bytes());
        for s in sorted_excl {
            bytes.extend_from_slice(&(s.len() as u64).to_le_bytes());
            bytes.extend_from_slice(s.as_bytes());
        }

        ContentHash::from_bytes(&bytes)
    }
}
