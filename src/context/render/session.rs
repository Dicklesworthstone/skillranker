//! Bounded disclosure of locally resolved session evidence. This produces only
//! provider prose: it never changes the local exclusions or loaded-state facts.

use super::{RenderContextError, RenderContextOptions, RenderedLoadedReference};
use crate::context::tool::head_tail_truncate;
use crate::limits::{DISCOVERY_FILES, NORMALIZED_CONTEXT_JSON_BYTES};
use crate::privacy::{CategoryReceipt, SourceCategory};

/// Separate from the request/history budget. Final JSON still shares the
/// existing serialized-request ceiling with messages, signals, and questions.
pub const SESSION_STATE_SCALARS: usize = 4096;
pub const SESSION_REFERENCE_SUMMARY_SCALARS: usize = 700;
pub const SESSION_REFERENCE_RECORDS: usize = 32;

fn too_large() -> RenderContextError {
    RenderContextError::UnsupportedContext("Session state exceeds its input bound".to_owned())
}

/// Preflight lengths before copying, sanitizing, or redacting any session body.
/// Whole source fields remain available to the scanner; no unscanned prefix can
/// be mistaken for a complete, safe name or constraint.
fn check_input(options: &RenderContextOptions<'_>) -> Result<(), RenderContextError> {
    let count = options
        .loaded_references
        .len()
        .checked_add(options.explicit_exclusions.len())
        .ok_or_else(too_large)?;
    if count > DISCOVERY_FILES.max() {
        return Err(too_large());
    }
    let fields = std::iter::once(options.loaded_state.as_str())
        .chain(options.explicit_exclusions.iter().map(String::as_str))
        .chain(
            options
                .loaded_references
                .iter()
                .flat_map(|reference| [reference.name.as_str(), reference.summary.as_str()]),
        );
    let mut total = 0usize;
    for field in fields {
        total = total.checked_add(field.len()).ok_or_else(too_large)?;
        if total > NORMALIZED_CONTEXT_JSON_BYTES.max() {
            return Err(too_large());
        }
    }
    Ok(())
}

/// Return a private provider view and its accounting without cloning the raw
/// option vectors. Exclusions and the evidence-state label are mandatory and
/// remain whole after redaction; optional reference summaries use the remainder.
pub(super) fn prepare<'a>(
    options: &RenderContextOptions<'a>,
) -> Result<(RenderContextOptions<'a>, CategoryReceipt), RenderContextError> {
    check_input(options)?;
    let mut receipt = CategoryReceipt {
        category: SourceCategory::SessionState,
        included_count: 0,
        omitted_count: 0,
        truncated_count: 0,
        redaction_count: 0,
    };
    let loaded_state = redact(options, &options.loaded_state, &mut receipt.redaction_count)?;
    let mut remaining = SESSION_STATE_SCALARS
        .checked_sub(loaded_state.chars().count())
        .ok_or_else(constraints_too_large)?;
    let mut explicit_exclusions = Vec::with_capacity(options.explicit_exclusions.len());
    for exclusion in &options.explicit_exclusions {
        let exclusion = redact(options, exclusion, &mut receipt.redaction_count)?;
        remaining = remaining
            .checked_sub(exclusion.chars().count())
            .ok_or_else(constraints_too_large)?;
        explicit_exclusions.push(exclusion);
    }

    let mut loaded_references = Vec::new();
    for reference in &options.loaded_references {
        if loaded_references.len() == SESSION_REFERENCE_RECORDS || remaining == 0 {
            continue;
        }
        let name = redact(options, &reference.name, &mut receipt.redaction_count)?;
        let name_size = name.chars().count();
        if name.is_empty() || name_size > remaining {
            continue; // Never truncate a name into a different invocation.
        }
        let summary = redact(options, &reference.summary, &mut receipt.redaction_count)?;
        let allowance = (remaining - name_size).min(SESSION_REFERENCE_SUMMARY_SCALARS);
        let truncated = summary.chars().count() > allowance;
        let summary = if truncated {
            head_tail_truncate(&summary, allowance)
        } else {
            summary
        };
        remaining -= name_size + summary.chars().count();
        loaded_references.push(RenderedLoadedReference { name, summary });
        receipt.truncated_count += usize::from(truncated);
    }
    receipt.omitted_count = options.loaded_references.len() - loaded_references.len();
    receipt.included_count = loaded_references.len() + explicit_exclusions.len();

    let prepared = RenderContextOptions {
        no_tools: options.no_tools,
        context_profile: options.context_profile,
        max_messages: options.max_messages,
        max_total_scalars: options.max_total_scalars,
        tool_excerpt_chars: options.tool_excerpt_chars,
        redactor: options.redactor,
        project_signals: options.project_signals,
        loaded_references,
        loaded_state,
        explicit_exclusions,
        fail_on_unsupported_context: options.fail_on_unsupported_context,
        essential_tool_missing: options.essential_tool_missing,
    };
    Ok((prepared, receipt))
}

fn redact(
    options: &RenderContextOptions<'_>,
    text: &str,
    redactions: &mut usize,
) -> Result<String, RenderContextError> {
    let clean = super::sanitize_media_data(text);
    let field = options.redactor.redact_field(&clean)?;
    *redactions += field.redaction_count();
    Ok(field.into_string())
}

fn constraints_too_large() -> RenderContextError {
    RenderContextError::UnsupportedContext(
        "Session constraints exceed the bounded context allowance".to_owned(),
    )
}
