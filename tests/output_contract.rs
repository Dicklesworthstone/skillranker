use serde_json::{Value, json};
use skillranker::identity::ContentHash;
use skillranker::output::{
    ArtifactKind, CliExit, ContractError, Decision, ErrorKind, MAX_OUTPUT_BYTES, MAX_OUTPUT_DEPTH,
    OutputDocument, OutputKind, TraceCursor,
};

const RANKED: &str = include_str!("fixtures/output-ranked.v1.json");
const EXPLICIT: &str = include_str!("fixtures/output-explicit.v1.json");
const ABSTAIN: &str = include_str!("fixtures/output-abstain.v1.json");
const UNAVAILABLE: &str = include_str!("fixtures/output-unavailable.v1.json");
const REPLAY: &str = include_str!("fixtures/output-replay.v1.json");
const DEMO: &str = include_str!("fixtures/output-demo.v1.json");
const REPORT: &str = include_str!("fixtures/output-report.v1.json");
const PREVIEW: &str = include_str!("fixtures/output-preview.v1.json");

fn value(input: &str) -> Value {
    serde_json::from_str(input).unwrap()
}

fn valid(v: Value) -> OutputDocument {
    let document = OutputDocument::from_value(v.clone()).expect("valid contract example");
    let wire = document.to_json().unwrap();
    let decoded = OutputDocument::from_json(&wire).unwrap();
    assert_eq!(decoded.as_value(), &v);
    assert_eq!(decoded.kind(), document.kind());
    assert_eq!(decoded.exit_code(), document.exit_code());
    document
}

fn invalid(v: Value) {
    assert!(OutputDocument::from_value(v.clone()).is_err());
    assert!(OutputDocument::from_json(&serde_json::to_vec(&v).unwrap()).is_err());
}

#[test]
fn all_decisions_and_artifact_kinds_round_trip_without_becoming_hook_output() {
    for (fixture, kind, exit) in [
        (
            RANKED,
            OutputKind::Decision(Decision::Ranked),
            CliExit::Success,
        ),
        (
            EXPLICIT,
            OutputKind::Decision(Decision::Explicit),
            CliExit::Success,
        ),
        (
            ABSTAIN,
            OutputKind::Decision(Decision::Abstain),
            CliExit::Success,
        ),
        (
            UNAVAILABLE,
            OutputKind::Decision(Decision::Unavailable),
            CliExit::Provider,
        ),
        (
            DEMO,
            OutputKind::Artifact(ArtifactKind::Demo),
            CliExit::Success,
        ),
        (
            REPLAY,
            OutputKind::Artifact(ArtifactKind::Replay),
            CliExit::Success,
        ),
        (
            REPORT,
            OutputKind::Artifact(ArtifactKind::Report),
            CliExit::Success,
        ),
        (
            PREVIEW,
            OutputKind::Artifact(ArtifactKind::Preview),
            CliExit::Success,
        ),
    ] {
        let document = valid(value(fixture));
        assert_eq!(document.kind(), kind);
        assert_eq!(document.exit_code(), exit);
        assert!(document.as_value().get("hookSpecificOutput").is_none());
        if matches!(kind, OutputKind::Artifact(_)) {
            assert_eq!(document.as_value()["actionable"], false);
            assert!(document.as_value().get("decision").is_none());
        }
    }
}

#[test]
fn documented_rank_examples_are_valid_contracts() {
    for document in [
        include_str!("../README.md"),
        include_str!("../COMPREHENSIVE_PLAN_TO_DESIGN_SKILLRANKER.md"),
    ] {
        let mut rank_examples = 0;
        for block in document.split("```json\n").skip(1) {
            let json = block.split("```").next().unwrap();
            let v: Value = serde_json::from_str(json).expect("illustrative JSON syntax");
            if v.get("decision").is_some() {
                valid(v);
                rank_examples += 1;
            }
        }
        assert_eq!(rank_examples, 1);
    }
}

#[test]
fn every_stable_error_kind_has_the_documented_exit_category() {
    let expected = [
        (2, "invalid-usage invalid-configuration"),
        (3, "missing-session ambiguous-session superseded"),
        (
            4,
            "provider-failure authentication credential-absent network-failure request-budget provider-cooldown budget-state",
        ),
        (
            5,
            "empty-roster unusable-roster unresolved-explicit incomplete-roster roster-changed retrieval-empty retrieval-failure",
        ),
        (6, "timeout"),
        (
            7,
            "malformed-input oversized-input unsupported-input unsupported-source-mode insufficient-context output-limit",
        ),
        (8, "network-denied"),
        (9, "storage-failure"),
        (10, "invalid-provider-response"),
        (11, "cache-miss"),
    ];
    let mut tested = 0;
    for (exit, kinds) in expected {
        for name in kinds.split_whitespace() {
            let kind: ErrorKind = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(kind.as_str(), name);
            assert_eq!(kind.exit_code() as u8, exit);
            for retryable in [false, true] {
                let document = OutputDocument::failure(kind, retryable);
                valid(document.as_value().clone());
                assert_eq!(document.exit_code() as u8, exit);
                let mut wrong = document.as_value().clone();
                wrong["error"]["code"] = json!(0);
                invalid(wrong);
            }
            tested += 1;
        }
    }
    assert_eq!(tested, ErrorKind::ALL.len());
}

#[test]
fn skipped_stages_are_null_and_explicit_requests_have_no_provider_work() {
    let baseline = value(EXPLICIT);
    for field in [
        "needs_skill",
        "choice_confidence",
        "none_probability",
        "omitted_rank_mass",
    ] {
        let mut v = baseline.clone();
        v[field] = json!(0);
        invalid(v);
    }
    for field in [
        "rank_score",
        "wide_probability",
        "rerank_probability",
        "fits",
    ] {
        let mut v = baseline.clone();
        v["skills"][0][field] = json!(0);
        invalid(v);
    }
    for field in [
        "requested",
        "wide_returned",
        "rerank_returned",
        "immutable_revision",
    ] {
        let mut v = baseline.clone();
        v["model"][field] = json!("fabricated-model");
        invalid(v);
    }
    let mut v = baseline.clone();
    v["usage"]["requests"] = json!(1);
    v["usage"]["http_attempts"] = json!(1);
    invalid(v);
    let mut evaluated_abstention = value(ABSTAIN);
    evaluated_abstention["needs_skill"] = json!(0.0);
    evaluated_abstention["phase"] = json!("conversing");
    let d = valid(evaluated_abstention);
    assert_eq!(d.as_value()["needs_skill"], 0.0);
    assert!(d.as_value()["none_probability"].is_null());
    valid(baseline);
}

#[test]
fn raw_probabilities_are_distinct_from_normalized_local_scores() {
    let mut v = value(RANKED);
    assert_ne!(
        v["skills"][0]["rank_score"],
        v["skills"][0]["rerank_probability"]
    );
    v["skills"].as_array_mut().unwrap().pop();
    v["omitted_rank_mass"] = json!(0.111111);
    valid(v.clone());
    v["omitted_rank_mass"] = json!(0.0);
    invalid(v);
    for field in [
        "rank_score",
        "rerank_probability",
        "wide_probability",
        "fits",
    ] {
        for bad in [json!(-0.1), json!(1.1), Value::Null, json!("0.7")] {
            let mut v = value(RANKED);
            v["skills"][0][field] = bad;
            invalid(v);
        }
    }
    let mut tie = value(RANKED);
    tie["skills"][0]["rerank_probability"] = tie["none_probability"].clone();
    invalid(tie);
    for field in ["wide_probability", "rerank_probability"] {
        let mut v = value(RANKED);
        v["skills"][0][field] = json!(0.95);
        invalid(v);
    }
}

#[test]
fn mandatory_versions_discriminants_and_types_cannot_silently_downgrade() {
    for (field, replacement) in [
        ("schema_version", json!(2)),
        ("decision", json!("invented")),
        ("context_quality", json!("invented")),
        ("phase", json!("invented")),
        ("needs_skill", json!(true)),
        ("event_id", Value::Null),
    ] {
        let mut v = value(RANKED);
        v[field] = replacement;
        invalid(v);
    }
    for field in [
        "needs_skill",
        "quality",
        "warnings_omitted",
        "omitted_rank_mass",
    ] {
        let mut v = value(RANKED);
        v.as_object_mut().unwrap().remove(field);
        invalid(v);
    }
    let mut v = value(UNAVAILABLE);
    v["error"]["retryable"] = json!("false");
    invalid(v);
    let mut v = value(UNAVAILABLE);
    v["error"]["kind"] = json!("not-a-supported-kind");
    invalid(v);
}

#[test]
fn additive_fields_preserve_nested_nulls_unicode_and_exact_large_integers() {
    let mut v = value(RANKED);
    v["future"] = json!({"text":"未来 🦀", "null":null, "large":u64::MAX, "signed":i64::MIN});
    v["skills"][0]["future"] = json!([null, false, {"x":17}]);
    v["quality"]["future"] = json!(true);
    let bytes = valid(v.clone()).to_json().unwrap();
    assert_eq!(OutputDocument::from_json(&bytes).unwrap().as_value(), &v);
}

#[test]
fn duplicate_keys_trailing_input_and_private_parser_errors_are_refused_safely() {
    for wire in [
        r#"{"schema_version":1,"schema_version":1}"#,
        r#"{"future":{"private-secret":1,"private-secret":2}}"#,
        r#"{"private-secret":"unterminated}"#,
    ] {
        let error = OutputDocument::from_json(wire.as_bytes()).unwrap_err();
        assert!(!error.to_string().contains("private-secret"));
        assert!(!format!("{error:?}").contains("private-secret"));
    }
    assert!(OutputDocument::from_json(format!("{UNAVAILABLE} false").as_bytes()).is_err());
    let mut v = value(UNAVAILABLE);
    v["private-secret"] = json!("private-secret");
    let document = valid(v);
    assert!(!format!("{document:?}").contains("private-secret"));
}

#[test]
fn quality_flags_allow_windowed_success_without_certifying_missing_essentials() {
    for summary in ["complete", "prompt_only", "partial"] {
        let mut v = value(RANKED);
        v["context_quality"] = json!(summary);
        v["quality"]["history_windowed"] = json!(true);
        v["quality"]["attachments_omitted"] = json!(true);
        v["roster"]["partial"] = json!(true);
        valid(v);
    }
    for field in ["prompt_complete", "task_anchor_known"] {
        let mut v = value(RANKED);
        v["quality"][field] = json!(false);
        invalid(v);
    }
    let mut v = value(RANKED);
    v["context_quality"] = json!("insufficient");
    invalid(v);
    let mut unavailable = value(ABSTAIN);
    unavailable["decision"] = json!("unavailable");
    unavailable["context_quality"] = json!("insufficient");
    unavailable["quality"]["prompt_complete"] = json!(false);
    unavailable["error"] =
        OutputDocument::failure(ErrorKind::InsufficientContext, false).as_value()["error"].clone();
    assert_eq!(valid(unavailable).exit_code(), CliExit::Input);
}

#[test]
fn identity_provenance_and_roster_caps_are_not_display_name_aliases() {
    for (pointer, bad) in [
        ("/skills/0/skill_id", json!("__none__")),
        ("/skills/0/invocation_name", json!("run; evil")),
        ("/skills/0/content_hash", json!("placeholder")),
        ("/roster/provenance/snapshot_id", Value::Null),
        ("/roster/provenance/wide_set_id", Value::Null),
        ("/roster/wide_candidates", json!(255)),
        ("/roster/shortlist", json!(33)),
        ("/roster/total", json!(10001)),
    ] {
        let mut v = value(RANKED);
        *v.pointer_mut(pointer).unwrap() = bad;
        invalid(v);
    }
    let mut v = value(RANKED);
    v["skills"][1]["skill_id"] = v["skills"][0]["skill_id"].clone();
    invalid(v);
    let mut v = value(RANKED);
    v["roster"]["eligible"] = json!(255);
    v["roster"]["total"] = json!(255);
    v["roster"]["wide_candidates"] = json!(254);
    v["roster"]["retrieval"] = json!("quill-bm25");
    valid(v);
}

#[test]
fn unresolved_explicit_references_are_errors_and_never_substituted_suggestions() {
    let mut v = OutputDocument::failure(ErrorKind::UnresolvedExplicit, false)
        .as_value()
        .clone();
    v["unresolved"] = json!([{"reference":"missing-skill", "reason":"missing"}]);
    valid(v.clone());
    v["skills"] = value(RANKED)["skills"].clone();
    invalid(v);
    let mut v = value(EXPLICIT);
    v["unresolved"] = json!([{"reference":"ambiguous-skill", "reason":"ambiguous"}]);
    invalid(v);
}

#[test]
fn warning_counts_and_encoded_bytes_have_hard_limits() {
    let warning =
        json!({"kind":"partial-roster", "count":4000, "message":"Some records were excluded."});
    let mut v = value(RANKED);
    v["warnings"] = json!(vec![warning.clone(); 32]);
    v["warnings_omitted"] = json!(17);
    valid(v.clone());
    v["warnings"].as_array_mut().unwrap().push(warning);
    invalid(v);
    let mut v = value(UNAVAILABLE);
    v["padding"] = json!("");
    let empty_size = serde_json::to_vec(&v).unwrap().len();
    v["padding"] = json!("x".repeat(MAX_OUTPUT_BYTES - empty_size));
    assert_eq!(valid(v.clone()).to_json().unwrap().len(), MAX_OUTPUT_BYTES);
    v["padding"] = json!("x".repeat(MAX_OUTPUT_BYTES - empty_size + 1));
    invalid(v);
    assert_eq!(
        OutputDocument::from_json(&vec![b' '; MAX_OUTPUT_BYTES + 1]).unwrap_err(),
        ContractError::LimitExceeded
    );
}

#[test]
fn unknown_additive_nesting_is_bounded_too() {
    let mut v = value(UNAVAILABLE);
    let mut nested = Value::Null;
    for _ in 0..MAX_OUTPUT_DEPTH - 1 {
        nested = json!([nested]);
    }
    v["future"] = nested.clone();
    valid(v.clone());
    v["future"] = json!([[nested]]);
    invalid(v);
}

#[test]
fn cache_hits_preserve_stage_metadata_without_claiming_fresh_usage() {
    let mut v = value(RANKED);
    v["cache"] = json!({"hit":true,"wide_hit":true,"rerank_hit":true,"stale":false,"age_ms":12});
    for key in [
        "requests",
        "http_attempts",
        "input_tokens",
        "output_tokens",
        "unknown_usage_attempts",
    ] {
        v["usage"][key] = json!(0);
    }
    valid(v.clone());
    v["usage"]["input_tokens"] = json!(1);
    invalid(v);
    let mut v = value(RANKED);
    v["cache"]["stale"] = json!(true);
    invalid(v);
    let mut v = value(RANKED);
    v["usage"]["unknown_usage_attempts"] = json!(3);
    invalid(v);
}

#[test]
fn report_status_table_separates_execution_from_quality() {
    for run in ["complete", "partial"] {
        for gate in ["passed", "failed", "not-established", "not-applicable"] {
            let mut v = value(REPORT);
            v["run_status"] = json!(run);
            v["gate_status"] = json!(gate);
            if run == "partial" {
                v["completeness"]["stages_completed"] = json!(0);
            }
            if run == "partial" && gate == "passed" {
                invalid(v);
            } else {
                assert_eq!(valid(v).exit_code(), CliExit::Success);
            }
        }
    }
    for (pointer, replacement) in [
        ("/completeness/stages_completed", json!(0)),
        ("/completeness/cases_completed", json!(0)),
        ("/completeness/evidence_compatible", json!(false)),
        ("/evidence_origin", json!("synthetic")),
        ("/actionable", json!(true)),
    ] {
        let mut v = value(REPORT);
        *v.pointer_mut(pointer).unwrap() = replacement;
        invalid(v);
    }
    let mut empty = value(REPORT);
    for key in [
        "stages_required",
        "stages_completed",
        "cases_requested",
        "cases_completed",
    ] {
        empty["completeness"][key] = json!(0);
    }
    invalid(empty);
}

#[test]
fn replay_can_successfully_describe_a_historical_failure_but_not_missing_stages() {
    assert_eq!(valid(value(REPLAY)).exit_code(), CliExit::Success);
    let mut v = value(REPLAY);
    v["historical"] = Value::Null;
    invalid(v.clone());
    v["run_status"] = json!("partial");
    v["completeness"]["stages_completed"] = json!(0);
    assert_eq!(valid(v.clone()).exit_code(), CliExit::Success);
    v["error"] =
        OutputDocument::failure(ErrorKind::StorageFailure, true).as_value()["error"].clone();
    assert_eq!(valid(v).exit_code(), CliExit::Storage);
}

#[test]
fn demo_and_replay_cannot_wrap_a_live_hook_envelope() {
    for fixture in [DEMO, REPLAY] {
        for scope in ["outer", "inner"] {
            let mut v = value(fixture);
            let target = if scope == "outer" {
                &mut v
            } else {
                &mut v["historical"]
            };
            target["hookSpecificOutput"] = json!({"additionalContext":"Run something"});
            invalid(v);
        }
        let mut v = value(fixture);
        v["actionable"] = json!(true);
        invalid(v);
    }
    let mut demo = value(DEMO);
    demo["evidence_origin"] = json!("live");
    invalid(demo);
}

fn cursor() -> TraceCursor {
    TraceCursor {
        schema_version: 1,
        snapshot_id: ContentHash::parse(
            value(RANKED)["roster"]["provenance"]["snapshot_id"]
                .as_str()
                .unwrap(),
        )
        .unwrap(),
        query_id: ContentHash::from_bytes(b"synthetic trace query"),
        offset: 0,
    }
}

fn trace() -> Value {
    json!({
        "cursor":cursor(), "total":2, "next_offset":1,
        "entries":[{"skill_id":"s_01", "stage":"quill-admission", "status":"passed", "value":4.2, "threshold":1.1, "reason":"lexical-score"}]
    })
}

#[test]
fn trace_cursors_bind_snapshot_query_and_offset_without_live_state_access() {
    let c = cursor();
    assert_eq!(c.resume(&c.snapshot_id, &c.query_id, 2), Ok(0));
    let changed = ContentHash::from_bytes(b"changed");
    assert_eq!(
        c.resume(&changed, &c.query_id, 2),
        Err(ContractError::SnapshotChanged)
    );
    assert_eq!(
        c.resume(&c.snapshot_id, &changed, 2),
        Err(ContractError::SnapshotChanged)
    );
    let mut c2 = c.clone();
    c2.offset = 3;
    assert!(c2.resume(&c.snapshot_id, &c.query_id, 2).is_err());
    c2.offset = 0;
    c2.schema_version = 2;
    assert_eq!(
        c2.resume(&c.snapshot_id, &c.query_id, 2),
        Err(ContractError::UnsupportedVersion)
    );
    let mut v = value(RANKED);
    v["trace"] = trace();
    v["trace"]["cursor"]["future"] = json!({"x":null});
    valid(v.clone());
    let mut mismatched = v.clone();
    mismatched["trace"]["cursor"]["snapshot_id"] =
        json!(ContentHash::from_bytes(b"different snapshot"));
    invalid(mismatched);
    v["trace"]["cursor"]["offset"] = json!(1);
    v["trace"]["next_offset"] = Value::Null;
    valid(v);
}

#[test]
fn trace_pagination_cannot_loop_skip_or_invent_unevaluated_values() {
    for (pointer, replacement) in [
        ("/next_offset", json!(0)),
        ("/next_offset", json!(2)),
        ("/entries", json!([])),
        ("/cursor/offset", json!(u64::MAX)),
        ("/entries/0/status", json!("not-evaluated")),
        ("/entries/0/stage", json!("imagined-reasoning")),
    ] {
        let mut v = value(RANKED);
        v["trace"] = trace();
        *v["trace"].pointer_mut(pointer).unwrap() = replacement;
        invalid(v);
    }
    let mut v = value(RANKED);
    v["trace"] = trace();
    v["trace"]["entries"][0] = json!({"skill_id":"s_01", "stage":"fit-none", "status":"not-evaluated", "value":null,"threshold":null,"reason":null});
    valid(v.clone());
    v["trace"]["entries"][0]["value"] = json!(0);
    invalid(v);
}

#[test]
fn a_dry_run_preview_is_never_actionable_and_carries_exactly_one_result() {
    let preview = value(PREVIEW);
    assert_eq!(
        valid(preview.clone()).kind(),
        OutputKind::Artifact(ArtifactKind::Preview)
    );
    // A local result that makes no request instead of a request.
    let mut local = preview.clone();
    local["provider_request"] = Value::Null;
    local["disclosure"] = Value::Null;
    local["local_decision"] = value(EXPLICIT);
    assert_eq!(valid(local.clone()).exit_code(), CliExit::Success);
    // An unavailable local result keeps its error exit.
    let mut failed = local.clone();
    failed["local_decision"] = value(UNAVAILABLE);
    assert_eq!(valid(failed).exit_code(), CliExit::Provider);
    // Refused: both results, neither, an actionable or stateful preview, a
    // top-level decision, a request whose byte count is not its length, a
    // request that does not start with the wide stage, and a request without
    // its disclosure receipt.
    let mut both = preview.clone();
    both["local_decision"] = value(EXPLICIT);
    invalid(both);
    let mut neither = local;
    neither["local_decision"] = Value::Null;
    invalid(neither);
    let mut actionable = preview.clone();
    actionable["actionable"] = json!(true);
    invalid(actionable);
    let mut stateful = preview.clone();
    stateful["stateless"] = json!(false);
    invalid(stateful);
    let mut decided = preview.clone();
    decided["decision"] = json!("ranked");
    invalid(decided);
    let mut miscounted = preview.clone();
    miscounted["provider_request"]["stages"][0]["request_bytes"] = json!(1);
    invalid(miscounted);
    let mut reordered = preview.clone();
    reordered["provider_request"]["stages"][0]["stage"] = json!("rerank");
    invalid(reordered);
    let mut undisclosed = preview;
    undisclosed["disclosure"] = Value::Null;
    invalid(undisclosed);
}
