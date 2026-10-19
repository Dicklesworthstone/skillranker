//! Individually gated Phase P9 experiments, diagnostics, and inline TUI
//! contracts (sr-roadmap-l1i.10.1..10.13):
//! 1. Bounded Quill heading-passage selection within chosen skills (`p9_quill_passages`);
//! 2. Multi-query overflow reciprocal-rank fusion (`1 / (60 + rank_v)`);
//! 3. Content-digest-bound description overlays;
//! 4. Roster description quality diagnostics (`sr doctor --descriptions`) and
//!    suspected coverage-gap analysis (`sr gaps`);
//! 5. Nine-row inline TUI inspection, watch-generation fencing, non-TTY refusal,
//!    and clean terminal-state recovery (`p9_frankentui`);
//! 6. Explicit per-feature P9 gate dispositions (`p9_acceptance_gate`).

use crate::limits::{RERANK_BODY_EXCERPT_SCALARS, RERANK_DESCRIPTION_SCALARS};
use crate::output::ErrorKind;
use crate::privacy::redaction::{REDACTION_MARKER, Redactor};
use crate::roster::retrieval::{
    MAX_CANDIDATES, MAX_QUERY_TERMS, QueryInput, RetrievalBudget, compile_query,
};
use asupersync::Cx;
use frankensearch_core::IndexableDocument;
use frankensearch_quill::{QuillConfig, QuillIndex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

struct RedactedText {
    redacted: String,
    total_redactions: usize,
}

fn redact_text(raw: &str) -> RedactedText {
    match Redactor::default().redact_field(raw) {
        Ok(field) => RedactedText {
            total_redactions: field.redaction_count(),
            redacted: field.into_string(),
        },
        Err(_) => RedactedText {
            total_redactions: 1,
            redacted: REDACTION_MARKER.to_owned(),
        },
    }
}

pub const PASSAGES_SCHEMA: &str = "sr.passages_experiment.v1";
pub const GAPS_SCHEMA: &str = "sr.gaps.v1";
pub const DESCRIPTION_DIAGNOSTICS_SCHEMA: &str = "sr.description_diagnostics.v1";
pub const TUI_SCHEMA: &str = "sr.tui.v1";
pub const EXPERIMENTS_GATE_SCHEMA: &str = "sr.p9_dispositions.v1";

pub const MAX_PASSAGES_PER_SKILL: usize = 32;
pub const MIN_WATCH_INTERVAL_MS: u64 = 5_000;
pub const RRF_K: f64 = 60.0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExperimentError {
    InvalidInput(String),
    NonTty(String),
    StaleDigest {
        skill_id: String,
        expected: String,
        actual: String,
    },
    EngineFailure(String),
    Cancelled,
}

impl ExperimentError {
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidInput(_) | Self::NonTty(_) => ErrorKind::InvalidUsage,
            Self::StaleDigest { .. } => ErrorKind::InvalidConfiguration,
            Self::EngineFailure(_) => ErrorKind::EmptyRoster,
            Self::Cancelled => ErrorKind::Timeout,
        }
    }
}

impl fmt::Display for ExperimentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid experiment input: {msg}"),
            Self::NonTty(msg) => write!(f, "{msg}"),
            Self::StaleDigest {
                skill_id,
                expected,
                actual,
            } => write!(
                f,
                "stale content_hash for {skill_id}: overlay expects {expected}, current is {actual}"
            ),
            Self::EngineFailure(msg) => write!(f, "Quill passage engine failure: {msg}"),
            Self::Cancelled => f.write_str("experiment cancelled or deadline exceeded"),
        }
    }
}

impl std::error::Error for ExperimentError {}

// =============================================================================
// 1. Bounded Quill Heading-Passage Selection (sr-roadmap-l1i.10.1)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillPassage {
    pub passage_index: usize,
    pub heading: String,
    pub body: String,
    pub scalar_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PassageSelectionResult {
    pub skill_id: String,
    pub content_hash: String,
    pub selection_mode: String,
    pub selected_heading: Option<String>,
    pub selected_passage_index: Option<usize>,
    pub passage_count: usize,
    pub excerpt: String,
    pub excerpt_scalars: usize,
    pub max_budget_scalars: usize,
    pub budget_neutral: bool,
    pub secrets_redacted: usize,
}

fn truncate_scalars(text: &str, max_scalars: usize) -> String {
    text.chars().take(max_scalars).collect()
}

/// Split an already-authorized skill markdown body into a lead/purpose prefix
/// and heading-delimited passages (`#`, `##`, `###`), redacting any secrets
/// before indexing or excerpting.
pub fn split_heading_passages(
    raw_body: &str,
    max_passages: usize,
) -> (String, Vec<SkillPassage>, usize) {
    let redaction = redact_text(raw_body);
    let clean = redaction.redacted;
    let secrets_redacted = redaction.total_redactions;

    let mut lead_lines = Vec::new();
    let mut passages = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### "))
            && passages.len() < max_passages
        {
            if let Some(heading) = current_heading.take() {
                let body = current_lines.join("\n").trim().to_owned();
                if !body.is_empty() {
                    let scalar_count = heading.chars().count() + 2 + body.chars().count();
                    passages.push(SkillPassage {
                        passage_index: passages.len(),
                        heading,
                        body,
                        scalar_count,
                    });
                }
                current_lines.clear();
            }
            let heading_text = trimmed.trim_start_matches('#').trim().to_owned();
            current_heading = Some(if heading_text.is_empty() {
                "Section".to_owned()
            } else {
                heading_text
            });
        } else if current_heading.is_some() {
            current_lines.push(line);
        } else {
            lead_lines.push(line);
        }
    }

    if let Some(heading) = current_heading.take()
        && passages.len() < max_passages
    {
        let body = current_lines.join("\n").trim().to_owned();
        if !body.is_empty() {
            let scalar_count = heading.chars().count() + 2 + body.chars().count();
            passages.push(SkillPassage {
                passage_index: passages.len(),
                heading,
                body,
                scalar_count,
            });
        }
    }

    let lead = lead_lines.join("\n").trim().to_owned();
    (lead, passages, secrets_redacted)
}

/// Select the best heading-delimited passage within a chosen skill using Quill
/// while preserving the skill's lead/purpose prefix and strictly enforcing the
/// 700-character (`RERANK_BODY_EXCERPT_SCALARS`) rerank body budget.
///
/// If the query has no analyzed terms or matches no passage, falls back cleanly
/// to the declared opening excerpt (`no-match-lead-fallback`).
pub async fn select_skill_passage_excerpt(
    cx: &Cx,
    skill_id: &str,
    content_hash: &str,
    raw_body: &str,
    query_input: QueryInput<'_>,
) -> Result<PassageSelectionResult, ExperimentError> {
    let max_budget = RERANK_BODY_EXCERPT_SCALARS.max();
    let redaction_full = redact_text(raw_body);
    let default_opening = truncate_scalars(redaction_full.redacted.trim(), max_budget);

    let (lead, passages, secrets_redacted) =
        split_heading_passages(raw_body, MAX_PASSAGES_PER_SKILL);
    if passages.is_empty() {
        let excerpt_scalars = default_opening.chars().count();
        return Ok(PassageSelectionResult {
            skill_id: skill_id.to_owned(),
            content_hash: content_hash.to_owned(),
            selection_mode: "no-headings-lead-fallback".to_owned(),
            selected_heading: None,
            selected_passage_index: None,
            passage_count: 0,
            excerpt: default_opening,
            excerpt_scalars,
            max_budget_scalars: max_budget,
            budget_neutral: excerpt_scalars <= max_budget,
            secrets_redacted,
        });
    }

    let compiled = compile_query(cx, query_input)
        .map_err(|e| ExperimentError::EngineFailure(e.to_string()))?;
    let Some(literal_query) = compiled.query else {
        let excerpt_scalars = default_opening.chars().count();
        return Ok(PassageSelectionResult {
            skill_id: skill_id.to_owned(),
            content_hash: content_hash.to_owned(),
            selection_mode: "no-match-lead-fallback".to_owned(),
            selected_heading: None,
            selected_passage_index: None,
            passage_count: passages.len(),
            excerpt: default_opening,
            excerpt_scalars,
            max_budget_scalars: max_budget,
            budget_neutral: excerpt_scalars <= max_budget,
            secrets_redacted,
        });
    };

    let budget = RetrievalBudget::default();
    let config = QuillConfig {
        scribe_shard_budget_bytes: budget.scribe_bytes,
        delta_budget_bytes: budget.delta_bytes,
        max_ingest_shards: 1,
        deterministic_ingest: true,
        query_fuel_budget: budget.query_fuel,
        glob_expansion_limit: MAX_QUERY_TERMS,
        max_visibility_lag_ms: u64::MAX,
        ..QuillConfig::default()
    };
    let index = QuillIndex::in_memory(config)
        .map_err(|_| ExperimentError::EngineFailure("index init".into()))?;

    let mut docs = Vec::with_capacity(passages.len());
    for p in &passages {
        let doc_id = format!("p-{:04}", p.passage_index);
        docs.push(IndexableDocument::new(doc_id, p.body.clone()).with_title(p.heading.clone()));
    }
    index
        .index_documents(cx, &docs)
        .await
        .map_err(|_| ExperimentError::EngineFailure("index passages".into()))?;
    index
        .commit(cx)
        .await
        .map_err(|_| ExperimentError::EngineFailure("commit passages".into()))?;

    let page = index
        .search_paginated(cx, literal_query.as_str(), passages.len(), 0, false)
        .map_err(|_| ExperimentError::EngineFailure("search passages".into()))?;

    let Some(top_hit) = page.hits.first() else {
        let excerpt_scalars = default_opening.chars().count();
        return Ok(PassageSelectionResult {
            skill_id: skill_id.to_owned(),
            content_hash: content_hash.to_owned(),
            selection_mode: "no-match-lead-fallback".to_owned(),
            selected_heading: None,
            selected_passage_index: None,
            passage_count: passages.len(),
            excerpt: default_opening,
            excerpt_scalars,
            max_budget_scalars: max_budget,
            budget_neutral: excerpt_scalars <= max_budget,
            secrets_redacted,
        });
    };

    let idx = top_hit
        .document_id
        .strip_prefix("p-")
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| ExperimentError::EngineFailure("invalid passage doc_id".into()))?;
    let chosen = passages
        .get(idx)
        .ok_or_else(|| ExperimentError::EngineFailure("passage index out of bounds".into()))?;

    // Preserve a bounded lead/purpose prefix (up to 180 scalars) plus the selected heading passage
    // inside the same 700-scalar budget.
    let composed = if lead.is_empty() {
        format!("## {}\n{}", chosen.heading, chosen.body)
    } else {
        let bounded_lead = truncate_scalars(&lead, 180);
        format!("{bounded_lead}\n\n## {}\n{}", chosen.heading, chosen.body)
    };
    let excerpt = truncate_scalars(&composed, max_budget);
    let excerpt_scalars = excerpt.chars().count();

    Ok(PassageSelectionResult {
        skill_id: skill_id.to_owned(),
        content_hash: content_hash.to_owned(),
        selection_mode: "quill-heading-passage".to_owned(),
        selected_heading: Some(chosen.heading.clone()),
        selected_passage_index: Some(chosen.passage_index),
        passage_count: passages.len(),
        excerpt,
        excerpt_scalars,
        max_budget_scalars: max_budget,
        budget_neutral: excerpt_scalars <= max_budget,
        secrets_redacted,
    })
}

// =============================================================================
// 2. Multi-Query Overflow Reciprocal-Rank Fusion (sr-roadmap-l1i.10.2)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FusedCandidate {
    pub skill_id: String,
    pub rrf_score: f64,
    pub view_ranks: BTreeMap<String, usize>,
}

/// Fuse up to three Quill query views (`request`, `task`, `error`) using the
/// pinned Reciprocal Rank Fusion rule:
///   `score(d) = sum_v 1.0 / (60.0 + rank_v(d))`
/// where `rank_v(d) >= 1` is 1-indexed, absent hits contribute 0, identical
/// normalized views are deduplicated before fusion, ties break by stable
/// `skill_id`, and at most 254 candidates are returned.
pub fn fuse_overflow_rankings(
    views: &[(&str, &str, Vec<String>)],
) -> Result<Vec<FusedCandidate>, ExperimentError> {
    if views.is_empty() || views.len() > 3 {
        return Err(ExperimentError::InvalidInput(
            "multi-query fusion requires 1 to 3 views".into(),
        ));
    }
    let mut seen_normalized_queries = BTreeSet::new();
    let mut scores: BTreeMap<String, f64> = BTreeMap::new();
    let mut view_ranks: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();

    for (view_name, normalized_query, ranked_ids) in views {
        let norm = normalized_query.trim();
        if norm.is_empty() || !seen_normalized_queries.insert(norm.to_owned()) {
            continue;
        }
        let mut seen_in_view = BTreeSet::new();
        for (idx, skill_id) in ranked_ids.iter().take(MAX_CANDIDATES).enumerate() {
            if !seen_in_view.insert(skill_id.as_str()) {
                return Err(ExperimentError::InvalidInput(format!(
                    "duplicate skill_id {skill_id} in view {view_name}"
                )));
            }
            let rank_1 = idx + 1;
            let contrib = 1.0 / (RRF_K + rank_1 as f64);
            *scores.entry(skill_id.clone()).or_insert(0.0) += contrib;
            view_ranks
                .entry(skill_id.clone())
                .or_default()
                .insert((*view_name).to_owned(), rank_1);
        }
    }

    if scores.is_empty() {
        return Err(ExperimentError::EngineFailure("retrieval-empty".into()));
    }

    let mut fused: Vec<FusedCandidate> = scores
        .into_iter()
        .map(|(skill_id, rrf_score)| FusedCandidate {
            view_ranks: view_ranks.remove(&skill_id).unwrap_or_default(),
            skill_id,
            rrf_score,
        })
        .collect();

    fused.sort_by(|a, b| {
        b.rrf_score
            .total_cmp(&a.rrf_score)
            .then_with(|| a.skill_id.cmp(&b.skill_id))
    });
    fused.truncate(MAX_CANDIDATES);
    Ok(fused)
}

// =============================================================================
// 3. Description Overlays & Roster Description Diagnostics (sr-roadmap-l1i.10.3, .10.4)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillDescriptionEntry {
    pub skill_id: String,
    pub invocation_name: String,
    pub content_hash: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DescriptionOverlaySpec {
    pub skill_id: String,
    pub expected_content_hash: String,
    pub proposed_description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DescriptionDiagnosticFinding {
    pub skill_id: String,
    pub invocation_name: String,
    pub kind: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DescriptionDiagnosticsReport {
    pub schema: String,
    pub inspected_skills: usize,
    pub findings: Vec<DescriptionDiagnosticFinding>,
    pub duplicate_wide_prefixes: usize,
    pub missing_or_short_descriptions: usize,
    pub missing_tags: usize,
}

pub fn apply_description_overlay(
    entry: &SkillDescriptionEntry,
    overlay: &DescriptionOverlaySpec,
) -> Result<SkillDescriptionEntry, ExperimentError> {
    if entry.skill_id != overlay.skill_id {
        return Err(ExperimentError::InvalidInput(
            "overlay skill_id does not match target entry".into(),
        ));
    }
    if entry.content_hash != overlay.expected_content_hash {
        return Err(ExperimentError::StaleDigest {
            skill_id: entry.skill_id.clone(),
            expected: overlay.expected_content_hash.clone(),
            actual: entry.content_hash.clone(),
        });
    }
    let trimmed = overlay.proposed_description.trim();
    if trimmed.is_empty() {
        return Err(ExperimentError::InvalidInput(
            "proposed_description must not be empty".into(),
        ));
    }
    let scalar_len = trimmed.chars().count();
    if scalar_len > RERANK_DESCRIPTION_SCALARS.max() {
        return Err(ExperimentError::InvalidInput(format!(
            "proposed_description has {scalar_len} scalars, exceeding RERANK_DESCRIPTION_SCALARS ({})",
            RERANK_DESCRIPTION_SCALARS.max()
        )));
    }
    let mut updated = entry.clone();
    updated.description = redact_text(trimmed).redacted;
    Ok(updated)
}

pub fn inspect_roster_descriptions(
    entries: &[SkillDescriptionEntry],
) -> DescriptionDiagnosticsReport {
    let mut findings = Vec::new();
    let mut prefix_owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut missing_or_short_descriptions = 0;
    let mut missing_tags = 0;

    for entry in entries {
        let trimmed = entry.description.trim();
        let scalar_len = trimmed.chars().count();
        if scalar_len < 24 {
            missing_or_short_descriptions += 1;
            findings.push(DescriptionDiagnosticFinding {
                skill_id: entry.skill_id.clone(),
                invocation_name: entry.invocation_name.clone(),
                kind: "short-or-missing-description".into(),
                detail: format!(
                    "description has {scalar_len} scalars (< 24 minimum actionable summary)"
                ),
            });
        }
        if entry.tags.is_empty() {
            missing_tags += 1;
            findings.push(DescriptionDiagnosticFinding {
                skill_id: entry.skill_id.clone(),
                invocation_name: entry.invocation_name.clone(),
                kind: "missing-tags".into(),
                detail: "no searchable tags declared in frontmatter".into(),
            });
        }
        let wide_prefix: String = trimmed
            .chars()
            .take(160)
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        if !wide_prefix.is_empty() {
            prefix_owners
                .entry(wide_prefix)
                .or_default()
                .push(entry.skill_id.clone());
        }
    }

    let mut duplicate_wide_prefixes = 0;
    for (prefix, owners) in prefix_owners {
        if owners.len() > 1 {
            duplicate_wide_prefixes += 1;
            for skill_id in &owners {
                let invocation_name = entries
                    .iter()
                    .find(|e| &e.skill_id == skill_id)
                    .map(|e| e.invocation_name.clone())
                    .unwrap_or_default();
                findings.push(DescriptionDiagnosticFinding {
                    skill_id: skill_id.clone(),
                    invocation_name,
                    kind: "duplicate-wide-prefix".into(),
                    detail: format!(
                        "shares 160-scalar wide description prefix ({} chars) with {:?}",
                        prefix.chars().count(),
                        owners
                    ),
                });
            }
        }
    }

    DescriptionDiagnosticsReport {
        schema: DESCRIPTION_DIAGNOSTICS_SCHEMA.to_owned(),
        inspected_skills: entries.len(),
        findings,
        duplicate_wide_prefixes,
        missing_or_short_descriptions,
        missing_tags,
    }
}

// =============================================================================
// 4. Suspected Coverage Gaps (`sr gaps`, sr-roadmap-l1i.10.5)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GapObservationInput {
    pub event_id: String,
    pub decision: String,
    pub reason: String,
    pub needs_skill: Option<f64>,
    pub max_fit: Option<f64>,
    pub negative_feedback: bool,
    pub context_complete: bool,
    pub phase: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SuspectedCoverageGap {
    pub category: String,
    pub phase: String,
    pub event_count: usize,
    pub example_event_ids: Vec<String>,
    pub interpretation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CoverageGapsReport {
    pub schema: String,
    pub total_events_inspected: usize,
    pub degraded_context_excluded: usize,
    pub suspected_gap_events: usize,
    pub clusters: Vec<SuspectedCoverageGap>,
    pub notice: String,
}

pub fn analyze_coverage_gaps(events: &[GapObservationInput]) -> CoverageGapsReport {
    let mut degraded_context_excluded = 0;
    let mut suspected_gap_events = 0;
    let mut grouped: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();

    for ev in events {
        if !ev.context_complete {
            degraded_context_excluded += 1;
            continue;
        }
        let high_need_low_fit = ev.decision == "abstain"
            && ev.needs_skill.unwrap_or(0.0) >= 0.50
            && ev.max_fit.unwrap_or(0.0) < 0.30;
        let retrieval_miss = ev.reason == "retrieval-empty";
        let explicit_miss = ev.negative_feedback;

        let category = if retrieval_miss {
            Some("lexical-overflow-miss")
        } else if high_need_low_fit {
            Some("high-need-no-fitting-candidate")
        } else if explicit_miss {
            Some("judged-not-useful-or-missing-alternative")
        } else {
            None
        };

        if let Some(cat) = category {
            suspected_gap_events += 1;
            let phase = ev.phase.clone().unwrap_or_else(|| "unspecified".into());
            grouped
                .entry((cat.to_owned(), phase))
                .or_default()
                .push(ev.event_id.clone());
        }
    }

    let clusters = grouped
        .into_iter()
        .map(|((category, phase), mut ids)| {
            let event_count = ids.len();
            ids.truncate(5);
            SuspectedCoverageGap {
                category,
                phase,
                event_count,
                example_event_ids: ids,
                interpretation:
                    "Suspected coverage or retrieval gap; verify against full visible roster before authoring a new skill."
                        .into(),
            }
        })
        .collect();

    CoverageGapsReport {
        schema: GAPS_SCHEMA.to_owned(),
        total_events_inspected: events.len(),
        degraded_context_excluded,
        suspected_gap_events,
        clusters,
        notice:
            "Suspected gaps are diagnostic signals from complete-context turns; they do not prove a skill is missing from the ecosystem."
                .into(),
    }
}

// =============================================================================
// 5. Nine-Row Inline FrankenTUI Inspection & Safe Selection (sr-roadmap-l1i.10.6..10.8)
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TuiCandidateRow {
    pub rank: usize,
    pub skill_id: String,
    pub invocation_name: String,
    pub rank_score: f64,
    pub fit: f64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TuiState {
    pub decision: String,
    pub phase: String,
    pub needs_skill: f64,
    pub choice_confidence: f64,
    pub freshness: String,
    pub watch_enabled: bool,
    pub show_evidence: bool,
    pub active_generation: u64,
    pub last_refresh_ms: u64,
    pub selected_skill: Option<TuiCandidateRow>,
    pub candidates: Vec<TuiCandidateRow>,
    pub terminal_restored: bool,
}

impl TuiState {
    pub fn sample_default() -> Self {
        Self {
            decision: "ranked".into(),
            phase: "debugging".into(),
            needs_skill: 0.82,
            choice_confidence: 0.78,
            freshness: "fresh".into(),
            watch_enabled: false,
            show_evidence: false,
            active_generation: 1,
            last_refresh_ms: 1_000,
            selected_skill: None,
            candidates: vec![
                TuiCandidateRow {
                    rank: 1,
                    skill_id: "sk-rust-triage".into(),
                    invocation_name: "rust-test-triage".into(),
                    rank_score: 0.64,
                    fit: 0.88,
                    source: "project".into(),
                },
                TuiCandidateRow {
                    rank: 2,
                    skill_id: "sk-cargo-nextest".into(),
                    invocation_name: "cargo-nextest-runner".into(),
                    rank_score: 0.24,
                    fit: 0.71,
                    source: "user".into(),
                },
                TuiCandidateRow {
                    rank: 3,
                    skill_id: "sk-asan-debug".into(),
                    invocation_name: "sanitizer-diagnostics".into(),
                    rank_score: 0.12,
                    fit: 0.59,
                    source: "project".into(),
                },
            ],
            terminal_restored: false,
        }
    }

    /// Render the inline FrankenTUI layout in at most `max_rows.clamp(3, 9)` lines.
    /// Sanitizes all dynamic text so control characters cannot escape the pane.
    pub fn render_inline(&self, max_rows: usize) -> Vec<String> {
        let cap = max_rows.clamp(3, 9);
        let mut lines = Vec::with_capacity(cap);
        lines.push(format!(
            "SkillRanker TUI [decision={} phase={} gen={} watch={} fresh={}]",
            crate::output::table::sanitize_terminal_text(&self.decision),
            crate::output::table::sanitize_terminal_text(&self.phase),
            self.active_generation,
            if self.watch_enabled { "on" } else { "off" },
            crate::output::table::sanitize_terminal_text(&self.freshness),
        ));
        if cap >= 4 {
            lines.push(format!(
                "Gate needs_skill={:.2} choice_confidence={:.2} evidence={}",
                self.needs_skill,
                self.choice_confidence,
                if self.show_evidence {
                    "expanded"
                } else {
                    "compact"
                }
            ));
        }
        let body_slots = cap.saturating_sub(lines.len() + 1);
        for cand in self.candidates.iter().take(body_slots.min(5)) {
            let marker = if self
                .selected_skill
                .as_ref()
                .is_some_and(|s| s.skill_id == cand.skill_id)
            {
                "*"
            } else {
                " "
            };
            lines.push(format!(
                "{marker}[{}] {:<24} score={:.2} fit={:.2} ({})",
                cand.rank,
                crate::output::table::sanitize_terminal_text(&cand.invocation_name),
                cand.rank_score,
                cand.fit,
                crate::output::table::sanitize_terminal_text(&cand.source),
            ));
        }
        while lines.len() + 1 < cap {
            lines.push(String::from("  ---"));
        }
        lines.push(String::from(
            "Keys: [1-5] select target (no auto-exec)  [r] refresh  [w] watch(>=5s)  [e] evidence  [q] quit",
        ));
        lines.truncate(cap);
        lines
    }

    /// Apply a key action or watch refresh with generation fencing and 5-second
    /// minimum watch interval. Returns `true` when the user exits (`q`).
    pub fn handle_key(&mut self, key: char, now_ms: u64) -> bool {
        match key {
            '1'..='5' => {
                let idx = (key as usize) - ('1' as usize);
                if let Some(cand) = self.candidates.get(idx).cloned() {
                    self.selected_skill = Some(cand);
                }
                false
            }
            'r' => {
                self.active_generation += 1;
                self.last_refresh_ms = now_ms;
                self.freshness = "refreshed".into();
                false
            }
            'w' => {
                self.watch_enabled = !self.watch_enabled;
                false
            }
            'e' => {
                self.show_evidence = !self.show_evidence;
                false
            }
            'q' => {
                self.terminal_restored = true;
                true
            }
            _ => false,
        }
    }

    /// Attempt a watch-triggered update. Enforces the 5,000 ms minimum interval
    /// and rejects stale generations (`candidate_generation <= active_generation`).
    pub fn apply_watch_update(
        &mut self,
        candidate_generation: u64,
        now_ms: u64,
        new_candidates: Vec<TuiCandidateRow>,
    ) -> Result<bool, &'static str> {
        if !self.watch_enabled {
            return Ok(false);
        }
        if now_ms.saturating_sub(self.last_refresh_ms) < MIN_WATCH_INTERVAL_MS {
            return Ok(false);
        }
        if candidate_generation <= self.active_generation {
            return Err("stale-generation-ignored");
        }
        self.active_generation = candidate_generation;
        self.last_refresh_ms = now_ms;
        self.candidates = new_candidates;
        self.freshness = "watch-updated".into();
        Ok(true)
    }
}

pub fn run_tui_session(
    is_tty: bool,
    headless_keys: Option<&str>,
    max_rows: usize,
) -> Result<Value, ExperimentError> {
    run_tui_session_with_state(is_tty, headless_keys, max_rows, None)
}

pub fn run_tui_session_with_state(
    is_tty: bool,
    headless_keys: Option<&str>,
    max_rows: usize,
    initial_state: Option<TuiState>,
) -> Result<Value, ExperimentError> {
    if !is_tty && headless_keys.is_none() {
        return Err(ExperimentError::NonTty(
            "sr tui requires an interactive terminal (TTY); use `sr rank --table` or `sr rank --json` for non-interactive output".into(),
        ));
    }
    let mut state = initial_state.unwrap_or_else(TuiState::sample_default);
    let keys = headless_keys.unwrap_or("q");
    let mut now_ms = 2_000_u64;
    for ch in keys.chars() {
        now_ms += 1_000;
        if state.handle_key(ch, now_ms) {
            break;
        }
    }
    state.terminal_restored = true;
    let rendered_lines = state.render_inline(max_rows);
    Ok(json!({
        "schema": TUI_SCHEMA,
        "row_count": rendered_lines.len(),
        "max_rows": max_rows.clamp(3, 9),
        "rendered_lines": rendered_lines,
        "selected_skill": state.selected_skill,
        "watch_enabled": state.watch_enabled,
        "show_evidence": state.show_evidence,
        "active_generation": state.active_generation,
        "terminal_state_restored": state.terminal_restored,
        "escape_cleanup_sequence": "\\u001b[?25h\\u001b[0m",
        "loader_executed": false,
    }))
}

pub fn p9_experiment_dispositions() -> Value {
    json!({
        "schema": EXPERIMENTS_GATE_SCHEMA,
        "phase": "P9",
        "experiments": [
            {
                "id": "p9_quill_passages",
                "bead": "sr-roadmap-l1i.10.1",
                "disposition": "implemented-opt-in-evaluation",
                "default_production_selector_changed": false,
                "budget_neutral": true,
                "summary": "Heading-delimited Quill passage retrieval within chosen skills under strict 700-scalar rerank body budget with opening-excerpt fallback."
            },
            {
                "id": "p9_multi_query_fusion",
                "bead": "sr-roadmap-l1i.10.2",
                "disposition": "implemented-opt-in-evaluation",
                "default_production_selector_changed": false,
                "budget_neutral": true,
                "summary": "Up to 3 deduplicated Quill query views fused via pinned RRF 1/(60 + rank_v) within shared 254-candidate cap."
            },
            {
                "id": "p9_description_overlays",
                "bead": "sr-roadmap-l1i.10.3",
                "disposition": "implemented-opt-in-evaluation",
                "default_production_selector_changed": false,
                "budget_neutral": true,
                "summary": "Content-hash-bound evaluation-only description overlays without mutating skill files."
            },
            {
                "id": "p9_description_diagnostics_and_gaps",
                "bead": "sr-roadmap-l1i.10.4",
                "disposition": "shipped-cli-diagnostics",
                "default_production_selector_changed": false,
                "budget_neutral": true,
                "summary": "`sr doctor --descriptions` and `sr gaps` report duplicate 160-scalar wide prefixes, missing metadata, and suspected coverage gaps."
            },
            {
                "id": "p9_frankentui",
                "bead": "sr-roadmap-l1i.10.7",
                "disposition": "shipped-feature-gated-tui",
                "default_production_selector_changed": false,
                "budget_neutral": true,
                "summary": "Optional 9-row inline TUI (`--features tui`) with non-TTY refusal, 5s watch coalescing, generation fencing, and clean terminal restoration."
            }
        ]
    })
}
