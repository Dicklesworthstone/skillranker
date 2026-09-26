//! Diagnostic review queues stay out of representative estimates
//! (sr-roadmap-l1i.6.23).

use skillranker::evaluation::design_weighted::{
    SampledCaseLoss, StratumCaseLoss, compute_design_weighted_loss,
};
use skillranker::evaluation::review::{
    ReviewInput, ReviewPolicy, ReviewQueue, ReviewReason, review_queue,
};
use skillranker::evaluation::stratified::StratumAllocation;
use skillranker::evaluation::{CaseKey, EvaluationSplit, JudgedLabel};
use skillranker::pipeline::{RerankEvidence, StageEvidence, WideEvidence};
use std::collections::{BTreeMap, BTreeSet};

fn key(id: &str) -> CaseKey {
    CaseKey::new("frame", format!("fam-{id}"), id, 0, "policy")
}

fn label(case: &str, acceptable: &[&str]) -> JudgedLabel {
    JudgedLabel {
        schema_version: 1,
        case_id: case.into(),
        revision: 1,
        acceptable_skills: acceptable
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>(),
        explicit_directive: None,
        no_skill_needed: acceptable.is_empty(),
        constraints: vec![],
        adjudicator: "judge".into(),
        created_at_unix_ms: 1,
        notes: None,
    }
}

fn stages(
    needs_skill: f64,
    shortlist: &[&str],
    rerank: &[(&str, f64)],
    admitted: &[&str],
    quill_ranked: bool,
) -> StageEvidence {
    StageEvidence {
        admitted: admitted.iter().map(|s| (*s).to_owned()).collect(),
        quill_ranked,
        lexical: None,
        wide: Some(WideEvidence {
            needs_skill,
            none_probability: 0.1,
            low_need: needs_skill < 0.3,
            shortlist: shortlist.iter().map(|s| ((*s).to_owned(), 0.5)).collect(),
            intrinsic_shortlist: shortlist.iter().map(|s| (*s).to_owned()).collect(),
        }),
        rerank: (!rerank.is_empty()).then(|| RerankEvidence {
            none_probability: 0.1,
            candidates: rerank
                .iter()
                .map(|(id, p)| ((*id).to_owned(), *p, 0.8))
                .collect(),
        }),
    }
}

fn reasons(queue: &ReviewQueue, id: &str) -> Vec<String> {
    queue
        .entries
        .iter()
        .find(|entry| entry.key.case_id == id)
        .map(|entry| {
            entry
                .reasons
                .iter()
                .map(|reason| {
                    serde_json::to_value(reason).unwrap()["reason"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn each_diagnostic_reason_selects_only_its_cases() {
    let keys: Vec<CaseKey> = [
        "near", "clear", "disagree", "agree", "miss", "found", "open",
    ]
    .iter()
    .map(|id| key(id))
    .collect();
    let evidence = [
        stages(0.33, &["a"], &[("a", 0.6)], &["a"], false),
        stages(0.9, &["a"], &[("a", 0.6)], &["a"], false),
        stages(
            0.9,
            &["a", "b"],
            &[("a", 0.2), ("b", 0.7)],
            &["a", "b"],
            false,
        ),
        stages(
            0.9,
            &["a", "b"],
            &[("a", 0.7), ("b", 0.2)],
            &["a", "b"],
            false,
        ),
        stages(0.9, &["x"], &[("x", 0.6)], &["x"], true),
        stages(0.9, &["a"], &[("a", 0.6)], &["a"], true),
        stages(0.9, &["a"], &[("a", 0.6)], &["a"], false),
    ];
    let labels: Vec<JudgedLabel> = ["near", "clear", "disagree", "agree", "miss", "found"]
        .iter()
        .map(|id| label(id, &["a"]))
        .collect();
    let inputs: Vec<ReviewInput<'_>> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| ReviewInput {
            key,
            split: EvaluationSplit::Holdout,
            stratum: Some("normal:complete"),
            evidence: Some(&evidence[i]),
            judgment: labels.get(i),
        })
        .collect();
    let queue = review_queue(&inputs, ReviewPolicy::default());
    assert_eq!(queue.selection, "diagnostic-outcome-selected");
    assert_eq!(queue.denominator_effect, "none");
    assert_eq!(reasons(&queue, "near"), ["near-gate"]);
    assert_eq!(reasons(&queue, "disagree"), ["stage-disagreement"]);
    assert_eq!(reasons(&queue, "miss"), ["overflow-miss"]);
    assert_eq!(reasons(&queue, "open"), ["unresolved-judgment"]);
    // Honest counterparts: far from the gate, agreeing stages, an overflow
    // that admitted an acceptable skill.
    for quiet in ["clear", "agree", "found"] {
        assert!(reasons(&queue, quiet).is_empty(), "{quiet}");
    }
}

#[test]
fn a_case_is_queued_once_with_merged_reasons_in_its_original_split() {
    let near = key("near");
    let sparse_and_near = stages(0.28, &["a"], &[("a", 0.6)], &["a"], false);
    let judgment = label("near", &["a"]);
    let input = ReviewInput {
        key: &near,
        split: EvaluationSplit::Validation,
        stratum: Some("overflow:degraded"),
        evidence: Some(&sparse_and_near),
        judgment: Some(&judgment),
    };
    // The same case delivered twice cannot become two queue entries.
    let queue = review_queue(&[input, input], ReviewPolicy::default());
    assert_eq!(queue.entries.len(), 1);
    let entry = &queue.entries[0];
    assert_eq!(entry.split, EvaluationSplit::Validation);
    assert!(entry.reasons.contains(&ReviewReason::NearGate {
        needs_skill: 0.28,
        gate: 0.30
    }));
    // The stratum's size counts the case once, however often it arrives.
    assert!(entry.reasons.contains(&ReviewReason::SparseStratum {
        stratum: "overflow:degraded".into(),
        cases: 1
    }));
    assert_eq!(entry.reasons.len(), 2);
    // No inclusion probability is invented for an outcome-selected entry.
    let serialized = serde_json::to_string(&queue).unwrap();
    assert!(
        !serialized.contains("inclusion_probability"),
        "{serialized}"
    );
    assert!(!serialized.contains("weight"), "{serialized}");
}

#[test]
fn building_a_queue_leaves_the_population_estimate_unchanged() {
    let mut strata = BTreeMap::new();
    for (stratum, population) in [("routine", 900), ("overflow", 100)] {
        strata.insert(
            stratum.to_owned(),
            StratumAllocation {
                stratum_key: stratum.to_owned(),
                population_size: population,
                sample_size: 10,
                weight: population as f64 / 1000.0,
                inclusion_probability: Some(10.0 / population as f64),
            },
        );
    }
    let losses: Vec<StratumCaseLoss> = (0..20)
        .map(|i| {
            StratumCaseLoss::new(
                if i < 10 { "routine" } else { "overflow" },
                SampledCaseLoss::Observed(if i % 5 == 0 { 1.0 } else { 0.0 }),
            )
        })
        .collect();
    let before = compute_design_weighted_loss(&strata, &losses, 0.05).unwrap();
    // A queue over the same sampled cases, most of them selected.
    let keys: Vec<CaseKey> = (0..20).map(|i| key(&format!("c{i}"))).collect();
    let near = stages(0.31, &["a"], &[("b", 0.9)], &["a"], true);
    let inputs: Vec<ReviewInput<'_>> = keys
        .iter()
        .map(|key| ReviewInput {
            key,
            split: EvaluationSplit::Holdout,
            stratum: Some("overflow"),
            evidence: Some(&near),
            judgment: None,
        })
        .collect();
    let queue = review_queue(&inputs, ReviewPolicy::default());
    assert_eq!(queue.entries.len(), 20);
    let after = compute_design_weighted_loss(&strata, &losses, 0.05).unwrap();
    assert_eq!(before, after);
}
