//! Shipping dependency integration, independent of the later roster adapter.
//! These fixtures use no oracle backend, filesystem index, or provider service.
use asupersync::{
    Budget, CancelKind, Cx,
    runtime::{Runtime, RuntimeBuilder},
};
use frankensearch_core::IndexableDocument;
use frankensearch_quill::{
    FRANKENSEARCH_QUILL_CRATE_VERSION, QuillConfig, QuillIndex, QuillIndexError, QuillSearchResult,
};
use std::time::Instant;

fn config() -> QuillConfig {
    QuillConfig {
        scribe_shard_budget_bytes: 2 * 1024 * 1024,
        delta_budget_bytes: 512 * 1024,
        max_ingest_shards: 1,
        deterministic_ingest: true,
        query_fuel_budget: 100_000,
        glob_expansion_limit: 128,
        max_visibility_lag_ms: u64::MAX,
        ..QuillConfig::default()
    }
}

fn record(case: &str, stage: &str, started: Instant, count: usize, outcome: &str) {
    eprintln!(
        "{}",
        serde_json::json!({
            "schema_version": 1,
            "case_id": case,
            "stage": stage,
            "engine": "quill",
            "engine_version": FRANKENSEARCH_QUILL_CRATE_VERSION,
            "elapsed_us": started.elapsed().as_micros(),
            "count": count,
            "outcome": outcome,
        })
    );
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread().build().unwrap()
}

fn index(
    case: &str,
    runtime: &Runtime,
    cx: &Cx,
    config: QuillConfig,
    docs: &[IndexableDocument],
) -> QuillIndex {
    let started = Instant::now();
    let index = QuillIndex::in_memory(config).unwrap();
    runtime.block_on(async {
        // A direct SkillRanker Asupersync Cx must type-check at both Quill seams.
        index.index_documents(cx, docs).await.unwrap();
        index.commit(cx).await.unwrap();
    });
    record(case, "build_commit", started, docs.len(), "passed");
    index
}

fn search(case: &str, index: &QuillIndex, cx: &Cx, query: &str, limit: usize) -> QuillSearchResult {
    let started = Instant::now();
    let page = index.search_paginated(cx, query, limit, 0, false).unwrap();
    assert!(page.diagnostics.is_empty());
    assert_eq!(page.total_count, None, "no exact-count work requested");
    assert!(page.hits.iter().all(|hit| hit.score.is_finite()));
    record(case, "search", started, page.hits.len(), "passed");
    page
}

fn ids(page: &QuillSearchResult) -> Vec<&str> {
    page.hits
        .iter()
        .map(|hit| hit.document_id.as_str())
        .collect()
}

#[test]
fn shipping_schema_searches_titles_and_content_but_not_stored_metadata() {
    let case = "quill_shipping_schema";
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let docs = [
        IndexableDocument::new("skill-a", "rust compiler ownership lifetimes")
            .with_title("borrowcheck")
            .with_metadata("private_annotation", "metadatacanary"),
        IndexableDocument::new("skill-b", "database transaction locking")
            .with_title("sqlitehelper"),
        IndexableDocument::new("skill-c", "css browser layout").with_title("frontendstyle"),
    ];
    let index = index(case, &runtime, &cx, config(), &docs);
    assert_eq!(
        ids(&search(case, &index, &cx, "borrowcheck", 254)),
        ["skill-a"]
    );
    assert_eq!(
        ids(&search(case, &index, &cx, "transaction", 254)),
        ["skill-b"]
    );
    assert!(
        search(case, &index, &cx, "metadatacanary", 254)
            .hits
            .is_empty()
    );
    assert!(
        search(case, &index, &cx, "unmatchedword", 254)
            .hits
            .is_empty()
    );
    assert_eq!(search(case, &index, &cx, "layout", 254).doc_count, 3);
}

#[test]
fn shipping_title_boost_and_generated_disjunction_have_expected_results() {
    let case = "quill_title_boost";
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let docs = [
        IndexableDocument::new("skill-a", "quasar neutral").with_title("neutral"),
        IndexableDocument::new("skill-b", "neutral neutral").with_title("quasar"),
        IndexableDocument::new("skill-c", "nebula neutral").with_title("neutral"),
    ];
    let index = index(case, &runtime, &cx, config(), &docs);
    let page = search(case, &index, &cx, "quasar", 254);
    assert_eq!(ids(&page), ["skill-b", "skill-a"]);
    assert!(page.hits[0].score > page.hits[1].score);
    // Static adapter-authored syntax, not arbitrary user conversation text.
    let page = search(case, &index, &cx, "quasar OR nebula", 254);
    let mut matched = ids(&page);
    matched.sort_unstable();
    assert_eq!(matched, ["skill-a", "skill-b", "skill-c"]);
}

#[test]
fn stable_id_ingest_preserves_ties_at_the_254_hit_cutoff() {
    let case = "quill_cutoff_tie";
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let docs: Vec<_> = (0..257)
        .map(|id| IndexableDocument::new(format!("skill-{id:03}"), "sharedterm"))
        .collect();
    let index = index(case, &runtime, &cx, config(), &docs);
    let page = search(case, &index, &cx, "sharedterm", 254);
    assert_eq!(page.doc_count, 257);
    assert_eq!(page.hits.len(), 254);
    for (hit, document) in page.hits.iter().zip(&docs) {
        assert_eq!(hit.document_id, document.id);
        assert_eq!(hit.score, page.hits[0].score);
    }
    assert!(
        page.hits
            .windows(2)
            .all(|pair| pair[0].global_docid < pair[1].global_docid)
    );
    let repeated = search(case, &index, &cx, "sharedterm", 254);
    assert_eq!(page, repeated);
}

#[test]
fn cancelled_context_cannot_return_even_a_cached_successful_page() {
    let case = "quill_cancellation";
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let index = index(
        case,
        &runtime,
        &cx,
        config(),
        &[IndexableDocument::new("skill-a", "alpha")],
    );
    assert_eq!(search(case, &index, &cx, "alpha", 254).hits.len(), 1);
    let cancelled = runtime.request_cx_with_budget(Budget::new());
    cancelled.cancel_with(CancelKind::User, Some("fixture cancellation"));
    let started = Instant::now();
    assert!(matches!(
        index.search_paginated(&cancelled, "alpha", 254, 0, false),
        Err(QuillIndexError::Cancelled { .. })
    ));
    record(case, "search", started, 0, "cancelled");
    assert_eq!(search(case, &index, &cx, "alpha", 254).hits.len(), 1);
}

#[test]
fn fuel_exhaustion_is_a_typed_failure_not_an_empty_success() {
    let case = "quill_fuel";
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let docs = [IndexableDocument::new("skill-a", "alpha beta")];
    let limited = index(
        case,
        &runtime,
        &cx,
        QuillConfig {
            query_fuel_budget: 1,
            ..config()
        },
        &docs,
    );
    let started = Instant::now();
    assert!(matches!(
        limited.search_paginated(&cx, "alpha", 254, 0, false),
        Err(QuillIndexError::QueryFuelExhausted { .. })
    ));
    record(case, "search", started, 0, "fuel_exhausted");
    let sufficient = index(case, &runtime, &cx, config(), &docs);
    assert_eq!(
        ids(&search(case, &sufficient, &cx, "alpha", 254)),
        ["skill-a"]
    );
}

#[test]
fn zero_resource_budgets_are_refused_before_index_creation() {
    for invalid in [
        QuillConfig {
            query_fuel_budget: 0,
            ..config()
        },
        QuillConfig {
            max_ingest_shards: 0,
            ..config()
        },
        QuillConfig {
            scribe_shard_budget_bytes: 0,
            ..config()
        },
        QuillConfig {
            delta_budget_bytes: 0,
            ..config()
        },
    ] {
        assert!(QuillIndex::in_memory(invalid).is_err());
    }
    assert!(QuillIndex::in_memory(config()).is_ok());
}
