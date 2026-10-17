#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P9 bounded Quill passage selection within chosen skills
//! (sr-roadmap-l1i.10.1).
//!
//! Satisfies contract boundary `p9_quill_passages` mapped in
//! `tests/contract_matrix.toml`.

use skillranker::experiments::{select_skill_passage_excerpt, split_heading_passages};
use skillranker::limits::RERANK_BODY_EXCERPT_SCALARS;
use skillranker::roster::retrieval::QueryInput;
use skillranker::runtime::ProcessInvocation;

#[test]
fn heading_passages_retrieval() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();

    // Skill body with an opening purpose prefix, multiple heading sections, and an embedded secret canary
    // that must be redacted before indexing or excerpting.
    let raw_body = "\
Purpose: Diagnose and repair complex Rust workspace failures under strict deadlines.
Restrictions: Never run destructive git commands.

# Compilation & Borrow Checker Errors
When rustc reports E0502 or E0382 lifetime/borrow conflicts, inspect mutable aliasing spans
and scope temporary borrows before refactoring ownership.

## Sanitizer & ASan Crash Triage
When AddressSanitizer or ThreadSanitizer triggers a heap-use-after-free or data race in CI,
symbolize the backtrace and isolate the concurrent mutation window.
Token canary: sk-ant-api03-secretcanaryvalue1234567890abcdef

### Cargo Nextest & Flaky Timing
Use deterministic lab seeds and bounded timeouts when reproducing intermittent test timeouts.
";

    let (lead, passages, redacted_count) = split_heading_passages(raw_body, 32);
    assert!(lead.contains("Purpose: Diagnose and repair"));
    assert_eq!(passages.len(), 3);
    assert!(redacted_count >= 1, "embedded secret must be redacted");
    for p in &passages {
        assert!(!p.body.contains("sk-ant-api03-secretcanaryvalue"));
    }

    // 1. Equal-budget comparison: query targeting ASan / sanitizer selects the middle passage
    //    while preserving the lead prefix and staying strictly within RERANK_BODY_EXCERPT_SCALARS (700).
    let hit = invocation
        .runtime()
        .block_on(select_skill_passage_excerpt(
            &cx,
            "sk-rust-triage",
            "blake3:1111111111111111111111111111111111111111111111111111111111111111",
            raw_body,
            QueryInput {
                latest_request: "AddressSanitizer reported a heap-use-after-free in CI",
                active_task: "triage sanitizer crash",
                recent_errors: "SUMMARY: AddressSanitizer: heap-use-after-free",
            },
        ))
        .unwrap();

    assert_eq!(hit.selection_mode, "quill-heading-passage");
    assert_eq!(
        hit.selected_heading.as_deref(),
        Some("Sanitizer & ASan Crash Triage")
    );
    assert_eq!(hit.selected_passage_index, Some(1));
    assert!(hit.budget_neutral);
    assert!(hit.excerpt_scalars <= RERANK_BODY_EXCERPT_SCALARS.max());
    assert!(hit.excerpt.contains("Purpose: Diagnose and repair"));
    assert!(hit.excerpt.contains("Sanitizer & ASan Crash Triage"));
    assert!(!hit.excerpt.contains("sk-ant-api03-secretcanaryvalue"));

    // 2. No-match lead fallback: query with no matching passage terms falls back cleanly
    //    to the declared opening excerpt within the same 700-scalar budget.
    let fallback = invocation
        .runtime()
        .block_on(select_skill_passage_excerpt(
            &cx,
            "sk-rust-triage",
            "blake3:1111111111111111111111111111111111111111111111111111111111111111",
            raw_body,
            QueryInput {
                latest_request: "kubernetes helm chart ingress controller",
                active_task: "deploy cluster",
                recent_errors: "",
            },
        ))
        .unwrap();

    assert_eq!(fallback.selection_mode, "no-match-lead-fallback");
    assert!(fallback.selected_heading.is_none());
    assert!(fallback.budget_neutral);
    assert!(fallback.excerpt_scalars <= RERANK_BODY_EXCERPT_SCALARS.max());
    assert!(fallback.excerpt.starts_with("Purpose: Diagnose and repair"));
    assert!(invocation.shutdown());
}
