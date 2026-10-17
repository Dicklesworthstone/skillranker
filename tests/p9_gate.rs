#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P9 acceptance gate: bounded Quill passage retrieval, multi-query RRF
//! overflow fusion, content-hash-bound description overlays, description
//! diagnostics, coverage-gap analysis, FrankenTUI inspection, and explicit
//! per-feature dispositions (sr-roadmap-l1i.10.13).
//!
//! Satisfies contract boundary `p9_acceptance_gate` mapped in
//! `tests/contract_matrix.toml`.

use skillranker::experiments::{
    DescriptionOverlaySpec, GapObservationInput, SkillDescriptionEntry, analyze_coverage_gaps,
    apply_description_overlay, fuse_overflow_rankings, inspect_roster_descriptions,
    p9_experiment_dispositions, run_tui_session, select_skill_passage_excerpt,
};
use skillranker::roster::retrieval::QueryInput;
use skillranker::runtime::ProcessInvocation;

#[test]
fn all_p9_contracts_verified() {
    // 1. Bounded Quill passage selection (`p9_quill_passages`).
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let body = "Lead instructions.\n# Section Alpha\nAlpha details.\n## Section Beta\nBeta crash diagnostics and stack trace.";
    let passage_res = invocation
        .runtime()
        .block_on(select_skill_passage_excerpt(
            &cx,
            "sk-alpha-beta",
            "blake3:aaaa",
            body,
            QueryInput {
                latest_request: "crash diagnostics stack trace",
                active_task: "",
                recent_errors: "",
            },
        ))
        .unwrap();
    assert_eq!(passage_res.selection_mode, "quill-heading-passage");
    assert_eq!(passage_res.selected_heading.as_deref(), Some("Section Beta"));
    assert!(passage_res.budget_neutral);
    assert!(invocation.shutdown());

    // 2. Multi-query overflow RRF fusion (`1 / (60 + rank_v)`), deduplicating identical views.
    let fused = fuse_overflow_rankings(&[
        (
            "request",
            "\"rust\" OR \"panic\"",
            vec!["sk-b".into(), "sk-a".into()],
        ),
        (
            "task",
            "\"rust\" OR \"panic\"", // duplicate normalized query view -> deduplicated
            vec!["sk-b".into(), "sk-a".into()],
        ),
        (
            "error",
            "\"backtrace\"",
            vec!["sk-a".into(), "sk-c".into()],
        ),
    ])
    .unwrap();
    // sk-a has rank 2 in request (1/62) + rank 1 in error (1/61), beating sk-b (rank 1 in request only = 1/61).
    assert_eq!(fused[0].skill_id, "sk-a");
    assert_eq!(fused[1].skill_id, "sk-b");
    assert_eq!(fused[2].skill_id, "sk-c");

    // 3. Content-hash-bound description overlays & roster description diagnostics.
    let base_entry = SkillDescriptionEntry {
        skill_id: "sk-1".into(),
        invocation_name: "skill-one".into(),
        content_hash: "blake3:1111".into(),
        description: "Runs systematic Rust test triage and reproduces failures.".into(),
        tags: vec!["rust".into(), "testing".into()],
    };
    let stale_overlay = DescriptionOverlaySpec {
        skill_id: "sk-1".into(),
        expected_content_hash: "blake3:stale".into(),
        proposed_description: "Updated description.".into(),
    };
    assert!(apply_description_overlay(&base_entry, &stale_overlay).is_err());
    let oversized_overlay = DescriptionOverlaySpec {
        skill_id: "sk-1".into(),
        expected_content_hash: "blake3:1111".into(),
        proposed_description: "x".repeat(1_001),
    };
    assert!(apply_description_overlay(&base_entry, &oversized_overlay).is_err());
    let valid_overlay = DescriptionOverlaySpec {
        skill_id: "sk-1".into(),
        expected_content_hash: "blake3:1111".into(),
        proposed_description: "Updated evaluation-only description for Rust test triage.".into(),
    };
    let overlaid = apply_description_overlay(&base_entry, &valid_overlay).unwrap();
    assert!(overlaid.description.contains("Updated evaluation-only"));

    let diag = inspect_roster_descriptions(&[
        base_entry.clone(),
        SkillDescriptionEntry {
            skill_id: "sk-2".into(),
            invocation_name: "skill-two".into(),
            content_hash: "blake3:2222".into(),
            description: base_entry.description.clone(), // duplicate 160-scalar wide prefix
            tags: vec![],                                // missing tags
        },
        SkillDescriptionEntry {
            skill_id: "sk-3".into(),
            invocation_name: "skill-three".into(),
            content_hash: "blake3:3333".into(),
            description: "Too short".into(),
            tags: vec!["misc".into()],
        },
    ]);
    assert_eq!(diag.duplicate_wide_prefixes, 1);
    assert_eq!(diag.missing_or_short_descriptions, 1);
    assert_eq!(diag.missing_tags, 1);

    // 4. Suspected coverage-gap analysis (`sr gaps`).
    let gaps = analyze_coverage_gaps(&[
        GapObservationInput {
            event_id: "ev-1".into(),
            decision: "abstain".into(),
            reason: "below-fit".into(),
            needs_skill: Some(0.80),
            max_fit: Some(0.15),
            negative_feedback: false,
            context_complete: true,
            phase: Some("debugging".into()),
        },
        GapObservationInput {
            event_id: "ev-2".into(),
            decision: "unavailable".into(),
            reason: "degraded-input".into(),
            needs_skill: None,
            max_fit: None,
            negative_feedback: false,
            context_complete: false,
            phase: None,
        },
    ]);
    assert_eq!(gaps.total_events_inspected, 2);
    assert_eq!(gaps.degraded_context_excluded, 1);
    assert_eq!(gaps.suspected_gap_events, 1);
    assert_eq!(gaps.clusters[0].category, "high-need-no-fitting-candidate");

    // 5. FrankenTUI session & explicit per-feature P9 dispositions.
    let tui_out = run_tui_session(true, Some("2q"), 9).unwrap();
    assert_eq!(tui_out["terminal_state_restored"], true);
    assert_eq!(tui_out["selected_skill"]["skill_id"], "sk-cargo-nextest");

    let dispositions = p9_experiment_dispositions();
    assert_eq!(dispositions["phase"], "P9");
    let items = dispositions["experiments"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert!(
        items
            .iter()
            .all(|item| item["budget_neutral"] == true
                && item["default_production_selector_changed"] == false)
    );
}
