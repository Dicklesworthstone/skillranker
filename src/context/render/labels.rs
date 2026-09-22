//! Tool labels are transcript text too. Sanitize only the projected provider
//! view, never the names used for local dispatch or call/result association.

use super::{RenderContextError, RenderContextOptions, sanitize_media_data};
use crate::context::tool::head_tail_truncate;
use crate::context::{EventKind, NormalizedContext, PrivateText};
use crate::limits::NORMALIZED_CONTEXT_JSON_BYTES;
use crate::privacy::ContextProfile;

pub const TOOL_LABEL_SCALARS: usize = 128;

#[derive(Default)]
pub(super) struct LabelReceipt {
    pub(super) redactions: usize,
    pub(super) truncated: usize,
}

pub(super) fn prepare(
    context: &mut NormalizedContext,
    options: &RenderContextOptions<'_>,
) -> Result<LabelReceipt, RenderContextError> {
    let mut receipt = LabelReceipt::default();
    if options.no_tools || options.context_profile == ContextProfile::Minimal {
        return Ok(receipt); // Omitted labels are neither inspected nor disclosed.
    }
    let mut bytes = 0usize;
    for event in &mut context.events {
        if !matches!(
            event.kind,
            EventKind::ToolInvocation | EventKind::ToolResult
        ) {
            continue;
        }
        let Some(tool) = event.tool.as_mut() else {
            continue;
        };
        bytes = bytes
            .checked_add(tool.name.as_str().len())
            .filter(|total| *total <= NORMALIZED_CONTEXT_JSON_BYTES.max())
            .ok_or_else(|| {
                RenderContextError::UnsupportedContext(
                    "Tool labels exceed their input bound".to_owned(),
                )
            })?;
        let clean = sanitize_media_data(tool.name.as_str());
        let label = options.redactor.redact_field(&clean)?;
        receipt.redactions += label.redaction_count();
        let label = label.into_string();
        tool.name = PrivateText::new(if label.chars().count() > TOOL_LABEL_SCALARS {
            receipt.truncated += 1;
            head_tail_truncate(&label, TOOL_LABEL_SCALARS)
        } else {
            label
        });
    }
    Ok(receipt)
}
