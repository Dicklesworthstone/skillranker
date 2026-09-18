//! Bounded field-level disclosure receipts for dry-run/explain and request provenance.
//!
//! Enforces boundary `p3_disclosure_receipts`:
//! - Allowlisted source category breakdown:
//!   `user_request`, `message_history`, `tool_events`, `project_signals`, `session_state`.
//! - Field-level counts: `included_count`, `omitted_count`, `truncated_count`, `redaction_count`.
//! - Privacy & profile version: `schema_version`, `context_profile`.
//! - Disclosed byte volume and scalar counts: `disclosed_bytes`, `disclosed_scalars`.
//! - Zero secret fragments or raw paths: receipt never contains confidential matches or raw paths.
//! - Accurate match with rendered payload (`assertion_id: receipt_accurate`).

use crate::context::render::RenderedContextPayload;
use crate::output::ContextQuality;
use crate::privacy::ContextProfile;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Allowlisted source category for receipt reporting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCategory {
    #[default]
    UserRequest,
    MessageHistory,
    ToolEvents,
    ProjectSignals,
    SessionState,
}

impl fmt::Display for SourceCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserRequest => write!(f, "user_request"),
            Self::MessageHistory => write!(f, "message_history"),
            Self::ToolEvents => write!(f, "tool_events"),
            Self::ProjectSignals => write!(f, "project_signals"),
            Self::SessionState => write!(f, "session_state"),
        }
    }
}

/// Field counts for a single source category.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryReceipt {
    pub category: SourceCategory,
    pub included_count: usize,
    pub omitted_count: usize,
    pub truncated_count: usize,
    pub redaction_count: usize,
}

/// Bounded disclosure receipt recording exact field-level disclosure metrics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisclosureReceipt {
    pub schema_version: u32,
    pub context_profile: ContextProfile,
    pub context_quality: ContextQuality,
    pub no_tools: bool,
    pub categories: Vec<CategoryReceipt>,
    pub total_included: usize,
    pub total_omitted: usize,
    pub total_truncated: usize,
    pub total_redactions: usize,
    pub disclosed_bytes: usize,
    pub disclosed_scalars: usize,
}

/// Errors detected during receipt verification against payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptVerificationError {
    InvalidCategoryInventory,
    CountOverflow,
    ProfileMismatch {
        receipt: ContextProfile,
        payload: ContextProfile,
    },
    QualityMismatch {
        receipt: ContextQuality,
        payload: ContextQuality,
    },
    ByteCountMismatch {
        receipt: usize,
        payload: usize,
    },
    ScalarCountMismatch {
        receipt: usize,
        payload: usize,
    },
    ItemCountMismatch {
        category: SourceCategory,
        receipt: usize,
        payload: usize,
    },
    TotalsInconsistent {
        expected: usize,
        sum: usize,
    },
}

impl fmt::Display for ReceiptVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCategoryInventory => {
                write!(
                    f,
                    "receipt must contain each disclosure category exactly once"
                )
            }
            Self::CountOverflow => write!(f, "receipt category counts overflow"),
            Self::ProfileMismatch { receipt, payload } => {
                write!(
                    f,
                    "profile mismatch: receipt has '{receipt}', payload has '{payload}'"
                )
            }
            Self::QualityMismatch { receipt, payload } => {
                write!(
                    f,
                    "quality mismatch: receipt has '{receipt:?}', payload has '{payload:?}'"
                )
            }
            Self::ByteCountMismatch { receipt, payload } => {
                write!(
                    f,
                    "disclosed byte count mismatch: receipt={receipt}, payload={payload}"
                )
            }
            Self::ScalarCountMismatch { receipt, payload } => {
                write!(
                    f,
                    "disclosed scalar count mismatch: receipt={receipt}, payload={payload}"
                )
            }
            Self::ItemCountMismatch {
                category,
                receipt,
                payload,
            } => {
                write!(
                    f,
                    "item count mismatch for '{category}': receipt={receipt}, payload={payload}"
                )
            }
            Self::TotalsInconsistent { expected, sum } => {
                write!(
                    f,
                    "receipt category sum {sum} does not match total {expected}"
                )
            }
        }
    }
}

impl std::error::Error for ReceiptVerificationError {}

impl DisclosureReceipt {
    /// Returns the category receipt for a given source category, if present.
    pub fn category(&self, cat: SourceCategory) -> Option<&CategoryReceipt> {
        self.categories.iter().find(|c| c.category == cat)
    }

    /// Verifies that receipt counts strictly match the actual serialized payload.
    pub fn verify_against_payload(
        &self,
        payload: &RenderedContextPayload,
    ) -> Result<(), ReceiptVerificationError> {
        // Missing categories must not bypass the payload comparisons below.
        // Check cardinality first so hostile inventories require bounded work.
        let expected_categories = [
            SourceCategory::UserRequest,
            SourceCategory::MessageHistory,
            SourceCategory::ToolEvents,
            SourceCategory::ProjectSignals,
            SourceCategory::SessionState,
        ];
        if self.categories.len() != expected_categories.len()
            || expected_categories.iter().any(|expected| {
                self.categories
                    .iter()
                    .filter(|entry| entry.category == *expected)
                    .count()
                    != 1
            })
        {
            return Err(ReceiptVerificationError::InvalidCategoryInventory);
        }
        if self.context_profile != payload.context_profile {
            return Err(ReceiptVerificationError::ProfileMismatch {
                receipt: self.context_profile,
                payload: payload.context_profile,
            });
        }
        if self.context_quality != payload.context_quality {
            return Err(ReceiptVerificationError::QualityMismatch {
                receipt: self.context_quality,
                payload: payload.context_quality,
            });
        }
        if self.disclosed_bytes != payload.disclosed_bytes() {
            return Err(ReceiptVerificationError::ByteCountMismatch {
                receipt: self.disclosed_bytes,
                payload: payload.disclosed_bytes(),
            });
        }
        if self.disclosed_scalars != payload.total_message_scalars() {
            return Err(ReceiptVerificationError::ScalarCountMismatch {
                receipt: self.disclosed_scalars,
                payload: payload.total_message_scalars(),
            });
        }

        // Verify total sums match category sums
        let sum_included = self.checked_total(|c| c.included_count)?;
        if sum_included != self.total_included {
            return Err(ReceiptVerificationError::TotalsInconsistent {
                expected: self.total_included,
                sum: sum_included,
            });
        }
        let sum_omitted = self.checked_total(|c| c.omitted_count)?;
        if sum_omitted != self.total_omitted {
            return Err(ReceiptVerificationError::TotalsInconsistent {
                expected: self.total_omitted,
                sum: sum_omitted,
            });
        }
        let sum_truncated = self.checked_total(|c| c.truncated_count)?;
        if sum_truncated != self.total_truncated {
            return Err(ReceiptVerificationError::TotalsInconsistent {
                expected: self.total_truncated,
                sum: sum_truncated,
            });
        }
        let sum_redactions = self.checked_total(|c| c.redaction_count)?;
        if sum_redactions != self.total_redactions {
            return Err(ReceiptVerificationError::TotalsInconsistent {
                expected: self.total_redactions,
                sum: sum_redactions,
            });
        }

        // Verify UserRequest
        if let Some(cat) = self.category(SourceCategory::UserRequest) {
            let payload_req_count = if payload.latest_user_request.is_empty() {
                0
            } else {
                1
            };
            if cat.included_count != payload_req_count {
                return Err(ReceiptVerificationError::ItemCountMismatch {
                    category: SourceCategory::UserRequest,
                    receipt: cat.included_count,
                    payload: payload_req_count,
                });
            }
        }

        // Verify MessageHistory
        if let Some(cat) = self.category(SourceCategory::MessageHistory) {
            let non_tool_count = payload
                .recent_messages
                .iter()
                .filter(|m| m.role != "tool")
                .count();
            if cat.included_count != non_tool_count {
                return Err(ReceiptVerificationError::ItemCountMismatch {
                    category: SourceCategory::MessageHistory,
                    receipt: cat.included_count,
                    payload: non_tool_count,
                });
            }
        }

        // Verify ToolEvents
        if let Some(cat) = self.category(SourceCategory::ToolEvents) {
            let tool_count = payload
                .recent_messages
                .iter()
                .filter(|m| m.role == "tool")
                .count();
            if cat.included_count != tool_count {
                return Err(ReceiptVerificationError::ItemCountMismatch {
                    category: SourceCategory::ToolEvents,
                    receipt: cat.included_count,
                    payload: tool_count,
                });
            }
        }

        // Verify ProjectSignals dirty_paths
        if let Some(cat) = self.category(SourceCategory::ProjectSignals) {
            let expected_signals_items = payload.project_signals.languages.len()
                + payload.project_signals.tools_on_path.len()
                + payload.project_signals.dirty_paths.len();
            if cat.included_count != expected_signals_items {
                return Err(ReceiptVerificationError::ItemCountMismatch {
                    category: SourceCategory::ProjectSignals,
                    receipt: cat.included_count,
                    payload: expected_signals_items,
                });
            }
        }

        // Verify SessionState
        if let Some(cat) = self.category(SourceCategory::SessionState) {
            let session_items = payload.session_state.loaded_references.len()
                + payload.session_state.explicit_exclusions.len();
            if cat.included_count != session_items {
                return Err(ReceiptVerificationError::ItemCountMismatch {
                    category: SourceCategory::SessionState,
                    receipt: cat.included_count,
                    payload: session_items,
                });
            }
        }

        Ok(())
    }

    fn checked_total(
        &self,
        count: impl Fn(&CategoryReceipt) -> usize,
    ) -> Result<usize, ReceiptVerificationError> {
        self.categories.iter().try_fold(0usize, |sum, category| {
            sum.checked_add(count(category))
                .ok_or(ReceiptVerificationError::CountOverflow)
        })
    }
}
