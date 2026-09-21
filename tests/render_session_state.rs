//! Session evidence is provider input, not an exemption from bounded redaction.
//! These exercise both production rendering APIs and the actual receipt verifier.

use skillranker::context::render::{
    RenderContextError, RenderContextOptions, RenderedLoadedReference, SESSION_REFERENCE_RECORDS,
    SESSION_REFERENCE_SUMMARY_SCALARS, SESSION_STATE_SCALARS, render_context,
    render_context_and_receipt,
};
use skillranker::context::{CurrentRequest, NormalizedContext, PrivateText};
use skillranker::identity::HarnessId;
use skillranker::limits::{DISCOVERY_FILES, NORMALIZED_CONTEXT_JSON_BYTES};
use skillranker::output::ContextQuality;
use skillranker::privacy::redaction::Redactor;
use skillranker::privacy::{ContextProfile, SourceCategory};

fn context() -> NormalizedContext {
    NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/private-local-workspace"),
        session_id: None,
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: None,
            text: PrivateText::new("Help repair the failing Rust tests"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: Vec::new(),
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    }
}

fn reference(name: &str, summary: &str) -> RenderedLoadedReference {
    RenderedLoadedReference {
        name: name.to_owned(),
        summary: summary.to_owned(),
    }
}

#[test]
fn secrets_in_reference_names_summaries_and_exclusions_are_redacted_not_rejected() {
    let key = "ghp_123456789012345678901234567890123456";
    let aws = "AKIAIOSFODNN7EXAMPLE";
    for profile in [ContextProfile::Standard, ContextProfile::Minimal] {
        let options = RenderContextOptions {
            context_profile: profile,
            loaded_references: vec![reference(&format!("docs-{key}"), &format!("Use {aws} here"))],
            explicit_exclusions: vec![key.to_owned()],
            ..RenderContextOptions::default()
        };
        let original_references = options.loaded_references.clone();
        let input = context();
        let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
        assert_eq!(payload, render_context(&input, &options).unwrap());
        receipt.verify_against_payload(&payload).unwrap();
        let bytes = payload.to_json_bytes().unwrap();
        let wire = String::from_utf8(bytes).unwrap();
        assert!(!wire.contains(key));
        assert!(!wire.contains(aws));
        assert!(!wire.contains("private-local-workspace"));
        assert!(wire.contains("[REDACTED]"));
        let state = receipt.category(SourceCategory::SessionState).unwrap();
        assert_eq!(state.included_count, 2);
        assert_eq!(state.omitted_count, 0);
        assert_eq!(state.truncated_count, 0);
        assert_eq!(state.redaction_count, 3);
        assert_eq!(receipt.total_redactions, 3);
        assert_eq!(options.loaded_references, original_references);
        assert_eq!(options.explicit_exclusions, vec![key]);
        assert!(!serde_json::to_string(&receipt).unwrap().contains(key));
    }
}

#[test]
fn full_field_redaction_precedes_reference_summary_truncation() {
    let secret = "ghp_123456789012345678901234567890123456";
    let summary = format!("{} {secret} END", "x".repeat(SESSION_REFERENCE_SUMMARY_SCALARS));
    let options = RenderContextOptions {
        loaded_references: vec![reference("docs", &summary)],
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&context(), &options).unwrap();
    let text = &payload.session_state.loaded_references[0].summary;
    assert!(text.chars().count() <= SESSION_REFERENCE_SUMMARY_SCALARS);
    assert!(text.contains("chars omitted]"));
    assert!(text.ends_with("END"));
    assert!(!text.contains(secret));
    assert!(!text.contains("1234567890"));
    assert_eq!(payload.context_quality, ContextQuality::Partial);
    let state = receipt.category(SourceCategory::SessionState).unwrap();
    assert_eq!(state.redaction_count, 1);
    assert_eq!(state.truncated_count, 1);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn mandatory_exclusions_are_preserved_before_optional_reference_allocation() {
    let exclusion = "z".repeat(SESSION_STATE_SCALARS - "observed".len());
    let options = RenderContextOptions {
        explicit_exclusions: vec![exclusion.clone()],
        loaded_references: vec![reference("docs", "Optional evidence")],
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&context(), &options).unwrap();
    assert_eq!(payload.session_state.explicit_exclusions, vec![exclusion.clone()]);
    assert!(payload.session_state.loaded_references.is_empty());
    assert_eq!(options.explicit_exclusions, vec![exclusion]);
    let state = receipt.category(SourceCategory::SessionState).unwrap();
    assert_eq!(state.included_count, 1);
    assert_eq!(state.omitted_count, 1);
    assert_eq!(state.truncated_count, 0);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn unrepresentable_mandatory_constraints_fail_without_truncating_their_names() {
    let options = RenderContextOptions {
        explicit_exclusions: vec!["q".repeat(SESSION_STATE_SCALARS)],
        ..RenderContextOptions::default()
    };
    let first = render_context(&context(), &options).unwrap_err();
    let second = render_context_and_receipt(&context(), &options).unwrap_err();
    assert_eq!(first, second);
    assert!(matches!(first, RenderContextError::UnsupportedContext(_)));
    assert!(!first.to_string().contains(&"q".repeat(50)));
}

#[test]
fn reference_names_are_whole_and_a_large_name_does_not_starve_later_evidence() {
    let options = RenderContextOptions {
        loaded_references: vec![
            reference(&"x".repeat(SESSION_STATE_SCALARS), "Not a shortened invocation"),
            reference("compiler-reference", "Useful bounded evidence"),
        ],
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&context(), &options).unwrap();
    assert_eq!(payload.session_state.loaded_references.len(), 1);
    assert_eq!(payload.session_state.loaded_references[0].name, "compiler-reference");
    let state = receipt.category(SourceCategory::SessionState).unwrap();
    assert_eq!(state.omitted_count, 1);
    assert_eq!(state.truncated_count, 0);
    assert_eq!(payload.context_quality, ContextQuality::Partial);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn unicode_session_text_has_a_shared_scalar_budget_and_honest_omissions() {
    let references: Vec<_> = (0..40)
        .map(|index| reference(&format!("参考-{index}"), &"🦀".repeat(1000)))
        .collect();
    let options = RenderContextOptions {
        loaded_references: references,
        explicit_exclusions: vec!["禁止".to_owned()],
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&context(), &options).unwrap();
    assert_eq!(payload, render_context(&context(), &options).unwrap());
    let state = &payload.session_state;
    let scalars = state.loaded_state.chars().count()
        + state
            .explicit_exclusions
            .iter()
            .map(|text| text.chars().count())
            .sum::<usize>()
        + state
            .loaded_references
            .iter()
            .map(|reference| reference.name.chars().count() + reference.summary.chars().count())
            .sum::<usize>();
    assert!(scalars <= SESSION_STATE_SCALARS);
    assert!(!state.loaded_references.is_empty());
    let counts = receipt.category(SourceCategory::SessionState).unwrap();
    assert_eq!(counts.included_count + counts.omitted_count, 41);
    assert_eq!(counts.truncated_count, state.loaded_references.len());
    assert!(counts.omitted_count > 0);
    assert_eq!(payload.latest_user_request, "Help repair the failing Rust tests");
    assert_eq!(payload.context_quality, ContextQuality::Partial);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn a_reference_record_bound_also_limits_zero_length_summaries() {
    let options = RenderContextOptions {
        loaded_references: (0..=SESSION_REFERENCE_RECORDS)
            .map(|index| reference(&format!("ref-{index}"), ""))
            .collect(),
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&context(), &options).unwrap();
    assert_eq!(payload.session_state.loaded_references.len(), SESSION_REFERENCE_RECORDS);
    let state = receipt.category(SourceCategory::SessionState).unwrap();
    assert_eq!(state.omitted_count, 1);
    assert_eq!(state.truncated_count, 0);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn source_byte_and_item_bounds_apply_before_cloning_or_serialization() {
    let input = context();
    for options in [
        RenderContextOptions {
            loaded_references: vec![reference(
                "private-canary",
                &"x".repeat(NORMALIZED_CONTEXT_JSON_BYTES.max()),
            )],
            ..RenderContextOptions::default()
        },
        RenderContextOptions {
            loaded_references: vec![reference("a", ""); DISCOVERY_FILES.max() + 1],
            ..RenderContextOptions::default()
        },
        RenderContextOptions {
            loaded_references: vec![
                reference("a", &"x".repeat(NORMALIZED_CONTEXT_JSON_BYTES.max() / 2));
                2
            ],
            ..RenderContextOptions::default()
        },
    ] {
        let error = render_context_and_receipt(&input, &options).unwrap_err();
        assert!(matches!(error, RenderContextError::UnsupportedContext(_)));
        assert!(!error.to_string().contains("private-canary"));
        assert_eq!(error, render_context(&input, &options).unwrap_err());
    }
}

#[test]
fn evidence_label_entropy_and_media_obey_the_same_redaction_policy() {
    let options = RenderContextOptions {
        redactor: Redactor::with_entropy(true),
        loaded_state: "token=private-value-canary".to_owned(),
        loaded_references: vec![reference(
            "docs",
            concat!(
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ ",
                "data:application/octet-stream;base64,AQIDBA=="
            ),
        )],
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&context(), &options).unwrap();
    let wire = String::from_utf8(payload.to_json_bytes().unwrap()).unwrap();
    assert!(!wire.contains("private-value-canary"));
    assert!(!wire.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
    assert!(!wire.contains("AQIDBA"));
    assert!(wire.contains("[media omitted]"));
    assert_eq!(
        receipt
            .category(SourceCategory::SessionState)
            .unwrap()
            .redaction_count,
        2
    );
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn partial_session_disclosure_never_upgrades_insufficient_context() {
    let mut input = context();
    input.current_request.essential_attachment_missing = true;
    let options = RenderContextOptions {
        loaded_references: vec![reference("docs", &"a".repeat(1000))],
        ..RenderContextOptions::default()
    };
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    assert_eq!(payload.context_quality, ContextQuality::Insufficient);
    assert_eq!(receipt.context_quality, ContextQuality::Insufficient);
    assert_eq!(
        receipt
            .category(SourceCategory::SessionState)
            .unwrap()
            .truncated_count,
        1
    );
    receipt.verify_against_payload(&payload).unwrap();
}
