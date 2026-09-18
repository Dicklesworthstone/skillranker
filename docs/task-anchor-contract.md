# Task Anchor and Explicit Directive Contract (P3)

This document formalizes the contract for task anchor preservation, terse continuation antecedent recovery, and pre-windowing directive extraction (`p3_task_anchors`, bead `sr-roadmap-l1i.4.9`).

## 1. Scope and Mission

SkillRanker identifies and preserves the active user objective ("task anchor") and all explicit skill directives prior to windowing, scalar truncation, or remote payload rendering. When a user issues a terse continuation prompt (e.g., "continue", "go on", "proceed", "next"), the recommendation engine requires an active task anchor to establish semantic context.

This boundary guarantees that:
1. Full bounded local directives are extracted from the complete conversation history before windowing or scalar budget truncation occurs.
2. Directives issued in earlier turns are preserved even if those turns fall outside the rendered 12-message cutoff window.
3. Terse continuations recover the most recent substantive antecedent user instruction and its source event provenance.
4. Terse continuations lacking a recoverable antecedent instruction fail closed with `unavailable / missing-task-context` rather than being treated as empty, conversational, or negative recommendations.
5. Externally supplied task summaries are accepted only when accompanied by explicit, verifiable provenance; summaries are never invented using an unspecified LLM.
6. Oversized requests (>1 MiB) are rejected as `oversized-input` rather than partially inspected.
7. Mutually contradictory directives across current or historical turns are detected and reported as `conflicting-directives`.

## 2. Invariants

### Invariant 1: Pre-Windowing Directive Extraction & Preservation Beyond Cutoff
- Explicit skill directives (`/use-skill`, `/require-skill`, `/exclude-skill`, natural language directives like `use skill:foo`, `do not use skill:bar`) are parsed from the unredacted, unwindowed context.
- Historical user turns are scanned in chronological order.
- Directives established in turns that fall outside the 12-message rendered context cutoff (e.g., in turn 2 of a 20-turn session) remain fully active and preserved in `TaskAnchor.directives`.
- Model windowing or message truncation cannot strip or lose an active explicit directive.

### Invariant 2: Terse Continuation Classification & Antecedent Recovery
- Standard terse continuation phrases ("continue", "go on", "proceed", "next", "ok", "run it", "resume", etc.) are detected via normalized classification (`is_terse_continuation`).
- When the active prompt is a terse continuation:
  - The resolver searches backwards through session events for the most recent substantive user message (`Role::User`, `EventKind::Message`).
  - Intermediate non-message events (tool calls, tool results, assistant responses, compaction markers) are skipped.
  - The search stops if a `TaskBoundary` event is encountered, as a task boundary delineates independent user workflows.
  - If a substantive user message is found, it is retained as the task anchor with `AnchorProvenance::HistoricalEvent { event_id }`.

### Invariant 3: Essential Missing Context Semantics (`missing-task-context`)
- If a prompt is classified as a terse continuation but no substantive antecedent exists in session history (e.g., the very first session prompt is "continue", or all prior user messages are terse or bounded by a task boundary), anchor resolution fails with `AnchorResolution::MissingTaskContext`.
- In ranking workflows, `MissingTaskContext` maps to decision `Unavailable` (`unavailable / missing-task-context`).
- It is never treated as a conversational message, an empty request, or an unqualified negative determination that "no skills match".

### Invariant 4: Supplied Task Summary Provenance Requirement
- When session compaction or external orchestration supplies a task summary, it is accepted **only** if accompanied by non-empty, verifiable provenance metadata (`AnchorProvenance::SuppliedSummary { provenance }`).
- An unverified summary (empty provenance string or whitespace-only) is strictly rejected.
- SkillRanker never calls an LLM or secondary model to invent or fabricate a task summary.

### Invariant 5: Oversized Uninspectable Request Guard
- If `current_request.text` exceeds `HOOK_STDIN_BYTES` (1 MiB / 1,048,576 bytes), resolution rejects immediately with `AnchorResolution::OversizedInput`.
- Resolution does not inspect or parse a truncated prefix of an oversized request, because partial inspection could miss explicit exclusions, negative directives, or sensitive credentials.

### Invariant 6: Cross-Turn Contradictory Directive Detection
- If a skill is marked required (positive directive) and excluded (negative directive) across any combination of current and historical turns, resolution detects the conflict and returns `AnchorResolution::ConflictingDirectives`.
- SkillRanker does not guess which directive takes precedence or silently overwrite exclusions.

### Invariant 7: Pre-Truncation Secret Redaction
- Before being placed in `TaskAnchor.text`, the substantive instruction (whether derived from current request, historical event, or supplied summary) is redacted using `Redactor::redact_field`.
- The redactor replaces sensitive patterns with `[REDACTED]`.
- Raw secret values never appear in task anchor fields or diagnostics.

## 3. Data Model and Types

```rust
pub enum AnchorProvenance {
    CurrentRequest,
    HistoricalEvent { event_id: EventId },
    SuppliedSummary { provenance: String },
}

pub struct TaskAnchor {
    pub text: String,
    pub source_event_id: Option<EventId>,
    pub provenance: AnchorProvenance,
    pub is_terse: bool,
    pub directives: Vec<AnchorDirective>,
}

pub enum AnchorResolution {
    Established(TaskAnchor),
    MissingTaskContext { reason: &'static str },
    OversizedInput { bytes: usize, max: usize },
    ConflictingDirectives { detail: String },
}
```

## 4. Implementation and Verification

- Implementation: [`src/context/anchor.rs`](../src/context/anchor.rs)
- Unit & Property Tests: [`tests/context_contract.rs::task_anchors`](../tests/context_contract.rs)
- Contract Boundary: `p3_task_anchors` (Assertion ID: `anchor_intact`, E2E Case: `task-anchor-preserved`)
