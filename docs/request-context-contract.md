# Request-First Context Rendering Contract (P3)

This document formalizes the contract for rendering bounded, request-first context and session provenance for the Jev ranking engine (`p3_request_context`, bead `sr-roadmap-l1i.4.8`).

## 1. Scope and Mission

SkillRanker captures and normalizes session history into an internal, privacy-preserving payload shape sent to the Jev ranking backend. Context rendering enforces strict windowing budgets, deterministic head/tail truncation, whole-field redaction prior to truncation, removal of non-contextual model thinking/reasoning blocks, sanitization of binary and media data, and stripping of prior SkillRanker advisory output while preserving user quotes.

## 2. Invariants

### Invariant 1: Request-First Ordering and Deduplication
- The latest user request appears **once and only once** in `latest_user_request`.
- Older conversation turns and tool events appear in `recent_messages`.
- If an event in `events` shares identity with `current_request.event_id`, it is strictly omitted from `recent_messages` to prevent duplicate representation of the active prompt.
- For a native Claude transcript, the latest user request is the latest prompt the user submitted. Claude also writes `user` records the user never typed. The native parser marks these as system context: they stay in bounded history but never become the request. They are:
  - records flagged `isMeta` or `isCompactSummary`;
  - records whose `promptSource` is `system`, or whose `origin.kind` is not `human`;
  - unflagged interrupt markers and `<local-command-stdout>`/`<local-command-stderr>` output.

  These are unverified harness conventions. Records without such marks keep their user role.

### Invariant 2: Context Budget and Reservation Hierarchy
- The overall context budget is capped at **12,000 Unicode scalar values** (`RENDERED_CONTEXT_SCALARS`) and at most **12 logical messages** (`RECENT_NORMALIZED_MESSAGES`).
- Space within the scalar budget is reserved for `latest_user_request` **first**.
- If the latest request exceeds the scalar budget, it is truncated using deterministic head/tail truncation (`... [N chars omitted] ...`), and `recent_messages` is rendered empty.
- Remaining scalar budget (`max_total_scalars - req_scalars`) is allocated to `recent_messages` working backwards from newest to oldest recent turns.

### Invariant 3: Whole-Field Redaction Before Truncation
- Every field (latest request text, message content, tool arguments, tool results, dirty paths) is fully scanned and redacted with `[REDACTED]` **before** any character truncation or excerpting.
- This prevents partial token clipping from leaking credential fragments that would otherwise evade regex pattern matching.
- Following payload assembly, the complete serialized JSON document is re-inspected via `Redactor::inspect_payload`. If any secret is detected, serialization fails closed with `RenderContextError::SecretsDetected`.
- Diagnostics and error descriptions never retain secret patterns or matched private text.

### Invariant 4: Reasoning and Thinking Block Removal
- Model thinking/reasoning blocks (`<thinking>...</thinking>`, `<thought>...</thought>`, and ````thinking ... ```` blocks) are completely stripped from assistant and system messages.
- Ordinary assistant conclusions, recommendations, and conversational responses outside thinking blocks are strictly preserved.
- If a message consists entirely of thinking blocks, it is dropped from `recent_messages`.

### Invariant 5: Media and Binary Data Sanitization
- Embedded binary blobs and base64 data URIs (`data:image/...;base64,...`, `data:application/...;base64,...`) are replaced with standard omission markers (`[image omitted]`, `[media omitted]`).
- Pre-existing omission markers (`[image omitted]`, `[file omitted]`, etc.) are preserved.

### Invariant 6: Essential Attachment Missing Semantics
- When `current_request.essential_attachment_missing` is true, the request's meaning cannot be derived without the omitted non-text content.
- Rendering sets `context_quality = ContextQuality::Insufficient`.
- In ranking workflows, insufficient context results in decision `Unavailable` (`unavailable / unsupported-context`), rather than treating the prompt as an empty or conversational request.

### Invariant 7: Provenance-Aware Prior Advisory Stripping
- Prior advisory blocks emitted by SkillRanker (`"Suggested skill for the next step:"`, `"Use it only if it fits the user's request and current instructions."`, `"[SkillRanker]"`, etc.) are stripped from assistant, system, and tool events.
- **User content is never stripped:** If a user quotes or refers to an advisory marker (e.g. `Why did you suggest "Suggested skill for the next step: rust-cargo-basics"?`), the quote is preserved intact as ordinary user instructions.

### Invariant 8: Tool Summaries and `--no-tools` Isolation
- Tool events are rendered as structured summaries containing tool name, allowlisted arguments (`command`, `path`, `file_path`, `query`, `skill`, etc.), exit/error status, and head/tail excerpts (default 200 chars) with error lines preserved.
- The `--no-tools` flag removes tool arguments and results from remote context.
- Local load evidence and observations are extracted before remote filtering, ensuring local learning and suppression remain intact even when remote tool context is disabled.

## 3. Payload Schema

The resulting internal payload conforms to the documented schema:

```json
{
  "schema_version": 1,
  "harness": "claude_code",
  "context_quality": "complete",
  "project_signals": {
    "languages": ["rust", "lean"],
    "tools_on_path": ["cargo", "lake", "rch"],
    "dirty_paths": ["src/kernel/typeck.rs"],
    "dirty_paths_truncated": false
  },
  "session_state": {
    "loaded_references": [
      {"name": "rust-cargo-basics", "summary": "Cargo commands and common build errors"}
    ],
    "loaded_state": "observed",
    "explicit_exclusions": []
  },
  "recent_messages": [
    {"role": "tool", "tool": "shell", "status": "failed",
     "summary": "cargo test: 3 failures in typeck::universe"}
  ],
  "latest_user_request": "Investigate the failing universe tests."
}
```

## 4. Implementation and Verification

- Implementation: [`src/context/render.rs`](../src/context/render.rs)
- Unit & Contract Tests: [`tests/context_contract.rs::bounded_request_rendering`](../tests/context_contract.rs)
- Contract Boundary: `p3_request_context` (Assertion ID: `rendered_within_budget`)
