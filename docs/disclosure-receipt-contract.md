# Disclosure Receipts Contract (P3)

This document formalizes the contract for bounded field-level disclosure receipts and request provenance (`p3_disclosure_receipts`, bead `sr-roadmap-l1i.4.12`).

## 1. Scope and Mission

SkillRanker generates a cryptographically sound, bounded disclosure receipt alongside every rendered context payload. The receipt provides fine-grained, verifiable auditing of what was transmitted, omitted, truncated, or redacted across all data categories during context windowing and sanitization.

The disclosure receipt serves as:
1. The structured receipt returned by `--dry-run` and `explain` modes.
2. The auditable metadata embedded in request provenance for tracking disclosure volume.
3. The empirical foundation for evaluating whether a smaller context profile (e.g., `Minimal`) maintains quality parity with `Standard`.

This boundary guarantees that:
1. Every piece of disclosed data is categorized into allowlisted source categories.
2. For each category and across the entire payload, exact counts are maintained for:
   - `included_count`: Number of items or records retained in the payload.
   - `omitted_count`: Number of items or records dropped due to budget, profile, or `--no-tools`.
   - `truncated_count`: Number of fields subjected to head/tail scalar truncation.
   - `redaction_count`: Number of sensitive credential or secret pattern matches redacted.
3. The receipt **never** contains matched secret fragments, raw credentials, local filesystem paths, or confidential transcript prose.
4. Receipt metrics strictly match the actual serialized context payload (`assertion_id: receipt_accurate`).

## 2. Invariants

### Invariant 1: Allowlisted Source Categories
Disclosure reporting strictly categorizes context elements into five allowlisted categories:
- `user_request`: The active user prompt / indispensable task anchor.
- `message_history`: Prior user, assistant, and system conversational turns.
- `tool_events`: Tool invocations, arguments, and execution results.
- `project_signals`: Languages, tools on PATH, and repository-relative dirty paths.
- `session_state`: Loaded skill references and explicit skill exclusions.

No ad-hoc, untyped, or arbitrary categories may be introduced without schema versioning.

### Invariant 2: Zero Secret Fragment Disclosure
- Receipts contain numeric metrics, category enumerations, schema versions, and profile discriminants only.
- Matched credential text (e.g., API keys, OAuth tokens, private keys, passwords) is replaced with `[REDACTED]` in the payload and counted in `redaction_count`.
- Under no circumstances does the receipt store, reference, log, or serialize the matched secret text or any substring thereof.

### Invariant 3: Exact Payload Parity (`receipt_accurate`)
Receipt metrics must strictly reflect the payload generated during the same single-pass render:
- `receipt.context_profile == payload.context_profile`
- `receipt.context_quality == payload.context_quality`
- `receipt.disclosed_bytes == payload.disclosed_bytes()`
- `receipt.disclosed_scalars == payload.total_message_scalars()`
- Sum of `categories[*].included_count == receipt.total_included`
- Sum of `categories[*].omitted_count == receipt.total_omitted`
- Sum of `categories[*].truncated_count == receipt.total_truncated`
- Sum of `categories[*].redaction_count == receipt.total_redactions`
- Category-level included counts strictly match the element counts in `payload` (e.g., non-tool messages, tool messages, project signal items, session state items).

### Invariant 4: Profile and Flag Conformance
- When `context_profile == ContextProfile::Minimal`:
  - `tool_events.included_count == 0`
  - `message_history.included_count == 0`
  - `project_signals` dirty paths count is 0 (omitted paths reflected in `signals.omitted_count`).
- When `no_tools == true`:
  - `tool_events.included_count == 0`
  - All tool events in history are counted under `tool_events.omitted_count`.

### Invariant 5: Safe Explain and Dry-Run Provenance
- The receipt is safe to emit in plain text, structured JSON, or terminal diagnostics.
- It provides complete transparency regarding data disclosure without widening attack surfaces or exposing developer environment state.

## 3. Data Model and API

```rust
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryReceipt {
    pub category: SourceCategory,
    pub included_count: usize,
    pub omitted_count: usize,
    pub truncated_count: usize,
    pub redaction_count: usize,
}

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

impl DisclosureReceipt {
    pub fn category(&self, cat: SourceCategory) -> Option<&CategoryReceipt>;
    pub fn verify_against_payload(&self, payload: &RenderedContextPayload) -> Result<(), ReceiptVerificationError>;
}

pub fn render_context_and_receipt(
    context: &NormalizedContext,
    options: &RenderContextOptions<'_>,
) -> Result<(RenderedContextPayload, DisclosureReceipt), RenderContextError>;
```
