//! Bounded compiler contracts exercised through the shipping Quill engine.
use asupersync::{
    Budget, CancelKind, Cx,
    runtime::{Runtime, RuntimeBuilder},
};
use frankensearch_core::IndexableDocument;
use frankensearch_quill::{QuillConfig, QuillIndex, QuillIndexError};
use skillranker::roster::retrieval::{
    MAX_QUERY_SCALARS, MAX_QUERY_TERMS, MAX_SOURCE_SCALARS, QUERY_COMPILER_VERSION,
    QueryCompilation, QueryCompileError, QueryInput, compile_query,
};
use std::time::Instant;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread().build().unwrap()
}

fn compile(cx: &Cx, request: &str, task: &str, errors: &str) -> QueryCompilation {
    compile_query(
        cx,
        QueryInput {
            latest_request: request,
            active_task: task,
            recent_errors: errors,
        },
    )
    .unwrap()
}

fn index(runtime: &Runtime, cx: &Cx, docs: &[(&str, &str)], fuel: u64) -> QuillIndex {
    let index = QuillIndex::in_memory(QuillConfig {
        scribe_shard_budget_bytes: 2 * 1024 * 1024,
        delta_budget_bytes: 512 * 1024,
        max_ingest_shards: 1,
        deterministic_ingest: true,
        query_fuel_budget: fuel,
        glob_expansion_limit: 128,
        max_visibility_lag_ms: u64::MAX,
        ..QuillConfig::default()
    })
    .unwrap();
    let docs: Vec<_> = docs
        .iter()
        .map(|(id, text)| IndexableDocument::new(*id, *text))
        .collect();
    runtime.block_on(async {
        index.index_documents(cx, &docs).await.unwrap();
        index.commit(cx).await.unwrap();
    });
    index
}

fn matched(index: &QuillIndex, cx: &Cx, compiled: &QueryCompilation) -> Vec<String> {
    let page = index
        .search_paginated(cx, compiled.query.as_ref().unwrap().as_str(), 254, 0, false)
        .unwrap();
    assert!(page.diagnostics.is_empty());
    assert_eq!(page.total_count, None);
    let mut ids: Vec<_> = page
        .hits
        .iter()
        .map(|hit| hit.document_id.clone())
        .collect();
    ids.sort_unstable();
    ids
}

fn record(case: &str, started: Instant, compiled: &QueryCompilation) {
    eprintln!(
        "{}",
        serde_json::json!({
            "schema_version": 1,
            "case_id": case,
            "stage": "literal_query",
            "compiler_version": QUERY_COMPILER_VERSION,
            "elapsed_us": started.elapsed().as_micros(),
            "terms": compiled.diagnostics.emitted_terms,
            "query_scalars": compiled.diagnostics.query_scalars,
            "analyzed_tokens": compiled.diagnostics.analyzed_tokens,
            "omitted_terms": compiled.diagnostics.omitted_terms,
            "input_truncated": compiled.diagnostics.input_truncated,
            "outcome": "passed",
        })
    );
}

#[test]
fn terms_are_deduplicated_and_disjoined_across_context_sources() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let started = Instant::now();
    let compiled = compile(&cx, "CONTINUE continue", "rust compiler", "borrow error");
    assert_eq!(
        compiled.query.as_ref().unwrap().as_str(),
        "\"CONTINUE\" OR \"rust\" OR \"borrow\" OR \"compiler\" OR \"error\""
    );
    assert_eq!(compiled.diagnostics.duplicate_terms, 1);
    let index = index(
        &runtime,
        &cx,
        &[("a", "rust"), ("b", "borrow"), ("c", "gardening")],
        100_000,
    );
    assert_eq!(matched(&index, &cx, &compiled), ["a", "b"]);
    assert_eq!(
        compiled,
        compile(&cx, "CONTINUE continue", "rust compiler", "borrow error")
    );
    record("context_disjunction", started, &compiled);
}

#[test]
fn boolean_field_range_wildcard_and_phrase_text_cannot_supply_operators() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let index = index(
        &runtime,
        &cx,
        &[
            ("a", "alpha"),
            ("b", "beta"),
            ("c", "wildflower"),
            ("d", "gamma"),
            ("e", "or"),
            ("f", "alpha filler beta"),
        ],
        100_000,
    );
    for (input, expected) in [
        ("alpha AND beta", vec!["a", "b", "f"]),
        ("alpha NOT beta", vec!["a", "b", "f"]),
        ("title:alpha", vec!["a", "f"]),
        ("[alpha TO beta]", vec!["a", "b", "f"]),
        ("alpha*", vec!["a", "f"]),
        ("alpha?", vec!["a", "f"]),
        ("\"alpha beta\"~20^9", vec!["a", "b", "f"]),
        ("-alpha +beta", vec!["a", "b", "f"]),
        ("OR", vec!["e"]),
        ("alpha\\\" OR *:*", vec!["a", "e", "f"]),
    ] {
        let started = Instant::now();
        let compiled = compile(&cx, input, "", "");
        assert_eq!(matched(&index, &cx, &compiled), expected);
        record("operator_injection", started, &compiled);
    }
}

#[test]
fn multilingual_analysis_preserves_lowercase_expansion_and_real_matches() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let started = Instant::now();
    let compiled = compile(&cx, "İSTANBUL CAFÉ 日本語", "РУСТ", "");
    let index = index(
        &runtime,
        &cx,
        &[
            ("a", "İSTANBUL"),
            ("b", "café"),
            ("c", "日本語"),
            ("d", "руст"),
            ("e", "istanbul"),
            ("f", "irrelevant"),
        ],
        100_000,
    );
    // Dotted-I is not the same indexed term as plain ASCII i; do not silently
    // split it by feeding the lowercase expansion through the analyzer twice.
    assert_eq!(matched(&index, &cx, &compiled), ["a", "b", "c", "d"]);
    record("unicode_analysis", started, &compiled);
}

#[test]
fn punctuation_and_empty_sources_produce_retrieval_empty() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    for input in ["", "  ", "*:* - + \" \\ {} [] () ? ^ ~ && ||", "😀 🦀 …"] {
        let started = Instant::now();
        let compiled = compile(&cx, input, "", "");
        assert!(compiled.query.is_none());
        assert_eq!(compiled.diagnostics.analyzed_tokens, 0);
        record("retrieval_empty", started, &compiled);
    }
}

#[test]
fn source_bounds_do_not_create_partial_terms_or_starve_other_sources() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let started = Instant::now();
    let huge = "x".repeat(MAX_SOURCE_SCALARS * 20);
    let compiled = compile(&cx, &huge, "rust", "error");
    assert_eq!(
        compiled.query.as_ref().unwrap().as_str(),
        "\"rust\" OR \"error\""
    );
    assert_eq!(compiled.diagnostics.input_truncated, [true, false, false]);
    let input = format!("{}猫猫", " ".repeat(MAX_SOURCE_SCALARS - 1));
    let clipped = compile(&cx, &input, "", "");
    assert!(
        clipped.query.is_none(),
        "the partial CJK token must be dropped"
    );
    let boundary = format!("{}猫!", " ".repeat(MAX_SOURCE_SCALARS - 1));
    assert_eq!(
        compile(&cx, &boundary, "", "").query.unwrap().as_str(),
        "\"猫\""
    );
    record("input_bounds", started, &compiled);
}

#[test]
fn term_cap_is_exact_and_does_not_drop_task_and_error_evidence() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let started = Instant::now();
    let request = (0..MAX_QUERY_TERMS + 1)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let compiled = compile(&cx, &request, "taskterm", "errorterm");
    assert_eq!(compiled.diagnostics.emitted_terms, MAX_QUERY_TERMS);
    assert_eq!(compiled.diagnostics.omitted_terms, 3);
    assert!(compiled.diagnostics.term_limit_reached);
    assert!(!compiled.diagnostics.scalar_limit_reached);
    let index = index(
        &runtime,
        &cx,
        &[("a", "taskterm"), ("b", "errorterm"), ("c", "word128")],
        100_000,
    );
    assert_eq!(matched(&index, &cx, &compiled), ["a", "b"]);
    record("term_cap", started, &compiled);
}

#[test]
fn scalar_cap_counts_quotes_separators_and_unicode_scalars_exactly() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let started = Instant::now();
    // 4087 CJK scalars + 2 quotes + 4 separator + 1 letter + 2 quotes = 4096.
    let long = "猫".repeat(MAX_QUERY_SCALARS - 9);
    let compiled = compile(&cx, &long, "z", "extra");
    assert_eq!(compiled.diagnostics.query_scalars, MAX_QUERY_SCALARS);
    assert_eq!(compiled.diagnostics.emitted_terms, 2);
    assert_eq!(compiled.diagnostics.omitted_terms, 1);
    assert!(compiled.diagnostics.scalar_limit_reached);
    assert!(compiled.query.as_ref().unwrap().as_str().len() > MAX_QUERY_SCALARS);
    let exact_one = "x".repeat(MAX_QUERY_SCALARS - 2);
    assert_eq!(
        compile(&cx, &exact_one, "", "").diagnostics.query_scalars,
        MAX_QUERY_SCALARS
    );
    let too_long = "x".repeat(MAX_QUERY_SCALARS - 1);
    assert_eq!(
        compile_query(
            &cx,
            QueryInput {
                latest_request: &too_long,
                active_task: "",
                recent_errors: ""
            }
        ),
        Err(QueryCompileError::UnrepresentableTerms)
    );
    let rescued = compile(&cx, &too_long, "valid", "");
    assert_eq!(rescued.query.unwrap().as_str(), "\"valid\"");
    record("escaped_scalar_cap", started, &compiled);
}

#[test]
fn cancellation_and_engine_fuel_failure_never_return_padded_candidates() {
    let runtime = runtime();
    let live = runtime.request_cx_with_budget(Budget::new());
    let cancelled = runtime.request_cx_with_budget(Budget::new());
    cancelled.cancel_with(CancelKind::User, Some("fixture cancellation"));
    assert_eq!(
        compile_query(
            &cancelled,
            QueryInput {
                latest_request: "alpha",
                active_task: "",
                recent_errors: ""
            }
        ),
        Err(QueryCompileError::Cancelled)
    );
    let compiled = compile(&live, "alpha", "", "");
    let limited = index(&runtime, &live, &[("a", "alpha")], 1);
    assert!(matches!(
        limited.search_paginated(
            &live,
            compiled.query.as_ref().unwrap().as_str(),
            254,
            0,
            false
        ),
        Err(QuillIndexError::QueryFuelExhausted { .. })
    ));
    let sufficient = index(&runtime, &live, &[("a", "alpha"), ("b", "other")], 100_000);
    assert_eq!(matched(&sufficient, &live, &compiled), ["a"]);
    assert!(matched(&sufficient, &live, &compile(&live, "nohits", "", "")).is_empty());
}

#[test]
fn debug_and_error_surfaces_do_not_reveal_private_query_text() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::new());
    let secret = "privatecanarycredential";
    let input = QueryInput {
        latest_request: secret,
        active_task: secret,
        recent_errors: secret,
    };
    assert!(!format!("{input:?}").contains(secret));
    let compiled = compile_query(&cx, input).unwrap();
    assert!(!format!("{compiled:?}").contains(secret));
    for error in [
        QueryCompileError::Cancelled,
        QueryCompileError::SchemaMismatch,
        QueryCompileError::UnrepresentableTerms,
        QueryCompileError::ParserRejected {
            diagnostic_count: 1,
            was_truncated: false,
        },
        QueryCompileError::MeaningChanged,
    ] {
        assert!(!format!("{error:?}: {error}").contains(secret));
    }
}
