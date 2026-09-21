//! Select the active request's history before rendering or disclosing any prose.
//! Payload formatting and redaction retain their existing implementation.

mod payload;

pub use payload::{
    IMAGE_OMISSION_MARKER, MEDIA_OMISSION_MARKER, RenderContextError, RenderContextOptions,
    RenderedContextPayload, RenderedLoadedReference, RenderedMessage, RenderedProjectSignals,
    RenderedSessionState, detect_languages_from_markers, is_sr_advisory_text,
    sanitize_media_data, strip_advisory_from_non_user, strip_thinking_blocks,
};

use crate::context::{EventKind, NormalizedContext, NormalizedEvent, Role};
use crate::output::ContextQuality;
use crate::privacy::{DisclosureReceipt, SourceCategory};

fn category_counts<'a>(events: impl Iterator<Item = &'a NormalizedEvent>) -> [usize; 2] {
    let mut counts = [0usize; 2];
    for event in events {
        match event.kind {
            EventKind::ToolInvocation | EventKind::ToolResult => counts[1] += 1,
            EventKind::Message if event.role == Role::Tool => counts[1] += 1,
            EventKind::Message => counts[0] += 1,
            _ => {}
        }
    }
    counts
}

/// Keep the original envelope unchanged. Do not clone all sibling event bodies
/// into a replacement envelope before overwriting its events field.
fn project(
    context: &NormalizedContext,
) -> Result<(NormalizedContext, bool, [usize; 2]), RenderContextError> {
    let (events, incomplete) = super::anchor::history::select(context)
        .map_err(|reason| RenderContextError::UnsupportedContext(reason.to_owned()))?;
    let selected = category_counts(events.iter());
    let original = category_counts(context.events.iter().filter(|event| {
        // Only the actual current event is excluded from history accounting;
        // a foreign event reusing its bare ID is still an omitted event.
        !(context.current_request.event_id.is_some()
            && event.event_id == context.current_request.event_id
            && event.agent_id == context.agent_id
            && event.branch_id == context.branch_id)
    }));
    let omitted = [
        original[0].saturating_sub(selected[0]),
        original[1].saturating_sub(selected[1]),
    ];
    let projected = NormalizedContext {
        schema_version: context.schema_version,
        harness: context.harness.clone(),
        producer_id: context.producer_id.clone(),
        workspace_root: context.workspace_root.clone(),
        session_id: context.session_id.clone(),
        agent_id: context.agent_id.clone(),
        branch_id: context.branch_id.clone(),
        context_epoch: context.context_epoch.clone(),
        current_request: context.current_request.clone(),
        events,
        explicit_skill_references: context.explicit_skill_references.clone(),
        supplied_loads: context.supplied_loads.clone(),
    };
    Ok((projected, incomplete, omitted))
}

fn finalize(
    payload: &mut RenderedContextPayload,
    incomplete: bool,
    options: &RenderContextOptions<'_>,
) -> Result<usize, RenderContextError> {
    if incomplete && payload.context_quality != ContextQuality::Insufficient {
        payload.context_quality = ContextQuality::Partial;
    }
    let bytes = serde_json::to_vec(payload)
        .map_err(|error| RenderContextError::Serialization(error.to_string()))?;
    options.redactor.inspect_payload(&bytes)?;
    Ok(bytes.len())
}

/// Render only selected history, and account for excluded messages by category
/// without inspecting or disclosing their bodies, identifiers, or arguments.
/// Unresolved history returns an error before any payload can be published.
pub fn render_context_and_receipt(
    context: &NormalizedContext,
    options: &RenderContextOptions<'_>,
) -> Result<(RenderedContextPayload, DisclosureReceipt), RenderContextError> {
    let (context, incomplete, omitted) = project(context)?;
    let (mut rendered, mut receipt) = payload::render_context_and_receipt(&context, options)?;
    receipt.disclosed_bytes = finalize(&mut rendered, incomplete, options)?;
    receipt.context_quality = rendered.context_quality;
    for category in &mut receipt.categories {
        let count = match category.category {
            SourceCategory::MessageHistory => omitted[0],
            SourceCategory::ToolEvents => omitted[1],
            _ => 0,
        };
        category.omitted_count += count;
    }
    receipt.total_omitted = receipt.categories.iter().map(|category| category.omitted_count).sum();
    Ok((rendered, receipt))
}

/// Payload-only entry point with the same history and privacy checks as the
/// receipt-producing API. Neither entry point exposes the unprojected renderer.
pub fn render_context(
    context: &NormalizedContext,
    options: &RenderContextOptions<'_>,
) -> Result<RenderedContextPayload, RenderContextError> {
    let (context, incomplete, _) = project(context)?;
    let mut rendered = payload::render_context(&context, options)?;
    finalize(&mut rendered, incomplete, options)?;
    Ok(rendered)
}
