use skillranker::context::render::RenderedContextPayload;
use skillranker::output::ContextQuality;
use skillranker::privacy::{CategoryReceipt, ContextProfile, DisclosureReceipt, SourceCategory};

fn fixture() -> (RenderedContextPayload, DisclosureReceipt) {
    let payload = RenderedContextPayload {
        schema_version: 1,
        context_profile: ContextProfile::Standard,
        harness: "normalized".into(),
        context_quality: ContextQuality::Complete,
        project_signals: Default::default(),
        session_state: Default::default(),
        recent_messages: vec![],
        latest_user_request: "Review the parser".into(),
    };
    let categories = [
        SourceCategory::UserRequest,
        SourceCategory::MessageHistory,
        SourceCategory::ToolEvents,
        SourceCategory::ProjectSignals,
        SourceCategory::SessionState,
    ]
    .into_iter()
    .map(|category| CategoryReceipt {
        category,
        included_count: usize::from(category == SourceCategory::UserRequest),
        ..Default::default()
    })
    .collect();
    let receipt = DisclosureReceipt {
        schema_version: 1,
        context_profile: payload.context_profile,
        context_quality: payload.context_quality,
        no_tools: false,
        categories,
        total_included: 1,
        total_omitted: 0,
        total_truncated: 0,
        total_redactions: 0,
        disclosed_bytes: payload.disclosed_bytes(),
        disclosed_scalars: payload.total_message_scalars(),
    };
    (payload, receipt)
}

#[test]
fn complete_receipt_accepts_any_category_order() {
    let (payload, mut receipt) = fixture();
    receipt.verify_against_payload(&payload).unwrap();
    receipt.categories.reverse();
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn missing_categories_cannot_hide_disclosed_items() {
    let (payload, mut receipt) = fixture();
    receipt.categories.clear();
    receipt.total_included = 0;
    assert!(receipt.verify_against_payload(&payload).is_err());
}

#[test]
fn duplicate_empty_category_is_not_a_valid_inventory() {
    let (payload, mut receipt) = fixture();
    receipt.categories[4] = receipt.categories[3].clone();
    assert!(receipt.verify_against_payload(&payload).is_err());
}

#[test]
fn overflowing_category_counts_return_error_without_panicking() {
    for field in 0..4 {
        let (payload, mut receipt) = fixture();
        for (i, count) in [usize::MAX, 1].into_iter().enumerate() {
            let category = &mut receipt.categories[i];
            match field {
                0 => category.included_count = count,
                1 => category.omitted_count = count,
                2 => category.truncated_count = count,
                _ => category.redaction_count = count,
            }
        }
        assert!(receipt.verify_against_payload(&payload).is_err());
    }
}
