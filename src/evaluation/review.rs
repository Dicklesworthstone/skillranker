//! Diagnostic review queues (P5, sr-roadmap-l1i.6.23).
//!
//! A review queue picks cases worth a closer look: gate decisions near the
//! threshold, stages that disagree, overflow retrieval misses, sparse strata
//! and cases whose judgment is still unresolved. Selection depends on outcomes,
//! so a queue is never a sample. Its entries carry no inclusion probability,
//! keep their original split, and never enter a representative denominator or
//! change live advice. Building a queue reads its inputs and returns a new
//! value; the evaluation it came from is unchanged.

use crate::evaluation::{CaseKey, EvaluationSplit, JudgedLabel};
use crate::pipeline::StageEvidence;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Thresholds that decide what enters the queue, fixed before looking.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewPolicy {
    /// The production gate threshold.
    pub gate: f64,
    /// A gate mean within this distance of `gate` is near-threshold.
    pub near_gate_margin: f64,
    /// A stratum with fewer cases than this is sparse.
    pub sparse_below: usize,
}

impl Default for ReviewPolicy {
    fn default() -> Self {
        Self {
            gate: 0.30,
            near_gate_margin: 0.05,
            sparse_below: 3,
        }
    }
}

/// One evaluated case as the queue sees it.
#[derive(Clone, Copy, Debug)]
pub struct ReviewInput<'a> {
    pub key: &'a CaseKey,
    pub split: EvaluationSplit,
    /// The observable stratum the case belongs to, when one was assigned.
    pub stratum: Option<&'a str>,
    pub evidence: Option<&'a StageEvidence>,
    /// `None` while no independent judgment is resolved for the case.
    pub judgment: Option<&'a JudgedLabel>,
}

/// Why a case entered the queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum ReviewReason {
    /// The wide gate mean fell within the policy margin of the threshold.
    NearGate { needs_skill: f64, gate: f64 },
    /// The wide stage's top candidate differs from the rerank's.
    StageDisagreement {
        wide_top: String,
        rerank_top: String,
    },
    /// Quill ranked an overflowing roster and admitted no acceptable skill.
    OverflowMiss,
    /// The case's stratum has too few cases to say much on its own.
    SparseStratum { stratum: String, cases: usize },
    /// No independent judgment is resolved yet.
    UnresolvedJudgment,
}

/// One queued case, with every reason it was selected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewEntry {
    pub key: CaseKey,
    /// The case's original split; a queue never moves a case between splits.
    pub split: EvaluationSplit,
    pub reasons: Vec<ReviewReason>,
}

/// A diagnostic queue. It is outcome-selected by construction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewQueue {
    /// Always "diagnostic-outcome-selected": never a probability sample.
    pub selection: String,
    /// Always "none": queue entries never enter a representative denominator.
    pub denominator_effect: String,
    pub policy: ReviewPolicy,
    pub entries: Vec<ReviewEntry>,
}

/// Build the queue. Each case appears at most once, with its reasons merged,
/// in the order the inputs first name it.
pub fn review_queue(inputs: &[ReviewInput<'_>], policy: ReviewPolicy) -> ReviewQueue {
    // Sizes count distinct cases: a duplicate delivery cannot fill a stratum.
    let mut members: BTreeMap<&str, std::collections::BTreeSet<&CaseKey>> = BTreeMap::new();
    for input in inputs {
        if let Some(stratum) = input.stratum {
            members.entry(stratum).or_default().insert(input.key);
        }
    }
    let stratum_sizes: BTreeMap<&str, usize> = members
        .into_iter()
        .map(|(stratum, keys)| (stratum, keys.len()))
        .collect();
    let mut order: Vec<&CaseKey> = Vec::new();
    let mut by_key: BTreeMap<&CaseKey, ReviewEntry> = BTreeMap::new();
    for input in inputs {
        let reasons = reasons_for(input, policy, &stratum_sizes);
        if reasons.is_empty() {
            continue;
        }
        let entry = by_key.entry(input.key).or_insert_with(|| {
            order.push(input.key);
            ReviewEntry {
                key: input.key.clone(),
                split: input.split,
                reasons: Vec::new(),
            }
        });
        for reason in reasons {
            if !entry.reasons.contains(&reason) {
                entry.reasons.push(reason);
            }
        }
    }
    ReviewQueue {
        selection: "diagnostic-outcome-selected".into(),
        denominator_effect: "none".into(),
        policy,
        entries: order
            .into_iter()
            .filter_map(|key| by_key.remove(key))
            .collect(),
    }
}

fn reasons_for(
    input: &ReviewInput<'_>,
    policy: ReviewPolicy,
    stratum_sizes: &BTreeMap<&str, usize>,
) -> Vec<ReviewReason> {
    let mut reasons = Vec::new();
    if let Some(wide) = input.evidence.and_then(|evidence| evidence.wide.as_ref())
        && (wide.needs_skill - policy.gate).abs() <= policy.near_gate_margin
    {
        reasons.push(ReviewReason::NearGate {
            needs_skill: wide.needs_skill,
            gate: policy.gate,
        });
    }
    if let Some(evidence) = input.evidence
        && let (Some(wide), Some(rerank)) = (&evidence.wide, &evidence.rerank)
        && let Some((wide_top, _)) = wide.shortlist.first()
        && let Some((rerank_top, _, _)) = rerank
            .candidates
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        && wide_top != rerank_top
    {
        reasons.push(ReviewReason::StageDisagreement {
            wide_top: wide_top.clone(),
            rerank_top: rerank_top.clone(),
        });
    }
    if let (Some(evidence), Some(label)) = (input.evidence, input.judgment)
        && evidence.quill_ranked
        && !label.no_skill_needed
        && !label.acceptable_skills.is_empty()
        && !evidence
            .admitted
            .iter()
            .any(|id| label.acceptable_skills.contains(id))
    {
        reasons.push(ReviewReason::OverflowMiss);
    }
    if let Some(stratum) = input.stratum {
        let cases = stratum_sizes.get(stratum).copied().unwrap_or(0);
        if cases < policy.sparse_below {
            reasons.push(ReviewReason::SparseStratum {
                stratum: stratum.to_owned(),
                cases,
            });
        }
    }
    if input.judgment.is_none() {
        reasons.push(ReviewReason::UnresolvedJudgment);
    }
    reasons
}
