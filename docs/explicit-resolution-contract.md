# Local Explicit Requirements Resolution Contract (P2)

This document formalizes the contract for resolving explicit skill requirements locally before prompt truncation, secret redaction, or probabilistic retrieval (`p2_explicit_resolution`, bead `sr-roadmap-l1i.3.6`).

## 1. Scope and Mission

SkillRanker allows users and harnesses to explicitly require or exclude specific skills. When explicit requirements are present, they carry absolute authority:
1. They bypass probabilistic gating, fit thresholds, and Quill BM25 lexical retrieval.
2. They do not invoke the external Jev ranking service (`no_network_needed`).
3. They are resolved strictly against the complete visible roster before any prefiltering or ranking.

If any explicit requirement cannot be resolved with certainty (e.g., it is missing, ambiguous, shadowed, unverified, forbidden, or conflicting), SkillRanker refuses to make advisory guesses and immediately exits with status `Unavailable` (Exit 5), leaving the user's prompt intact.

## 2. Invariants

### Invariant 1: Local Pre-Truncation Resolution
- Directives are parsed and resolved from the full bounded local user input **before** any redaction, windowing, or prompt truncation occurs.
- Prompt text is bounded by `HOOK_STDIN_BYTES` (1 MiB). Exceeding this limit returns `ExplicitResolutionError::OversizedInput`.

### Invariant 2: Quoted Examples and Code Blocks Ignored
- Quoted strings (`"..."`, `'...'`), fenced code blocks (` ```...``` `), and inline code (`` `...` ``) are stripped before directive parsing.
- Examples mentioned inside quotations or markdown code blocks are never parsed as positive or negative requirements.
- English contractions and possessives (e.g., `don't`, `can't`, `it's`, `user's`) are preserved so natural negative directives like `don't use skill X` function correctly.

### Invariant 3: Multi-Channel Directive Sources
Explicit requirements and exclusions are aggregated from:
1. **CLI flags**: `--require-skill <name|id>` and `--exclude-skill <name|id>`.
2. **Context envelope**: `context_skill_references` and `context_excluded_skills`.
3. **Prompt slash commands**: `/use-skill <target>`, `/require-skill <target>`, `/skill <target>`, `/exclude-skill <target>`, `/no-skill <target>`.
4. **Natural language directives**:
   - Positive: `use skill <target>`, `require skill <target>`, `run skill <target>`, `invoke skill <target>`, `execute skill <target>`.
   - Negative: `do not use skill <target>`, `don't use skill <target>`, `exclude skill <target>`.

### Invariant 4: Maximum 32 Explicit References Limit
- A hard limit of 32 distinct explicit skill references is enforced (`MAX_EXPLICIT_REQUESTS`).
- Submitting 33 or more distinct references immediately rejects with `ExplicitResolutionError::TooManyExplicitReferences`.

### Invariant 5: Conflicting Directive Detection
- If the same skill is both required and excluded (whether by exact name, exact ID, or cross-resolution where a required name maps to an excluded `SkillId`), resolution fails with `UnresolvedReason::ConflictingDirective`.
- SkillRanker does not guess which directive takes precedence.

### Invariant 6: All-or-Nothing Resolution (Exit 5)
- Every specified explicit requirement must resolve successfully to a verified, invocable candidate.
- If any reference is:
  - **Missing**: Not found in the visible roster (`UnresolvedReason::Missing`).
  - **Ambiguous**: Colliding names without unique priority (`UnresolvedReason::Ambiguous`).
  - **Shadowed**: Shadowed by a higher-priority root (`UnresolvedReason::Shadowed`).
  - **Unverified**: Lacking harness verification (`UnresolvedReason::Unverified`).
  - **Forbidden**: Restricted from both agent and user invocation (`UnresolvedReason::Forbidden`).
  - **Conflicting**: Subject to opposing directives (`UnresolvedReason::ConflictingDirective`).
- The entire batch fails with `ExplicitResolutionResult::Unavailable` containing all failure records.
- No partial lists are returned and no network calls to Jev are made.

### Invariant 7: Manual-Only Skills
- Skills restricted with `agent_invocable: false, user_invocable: true` resolve to `InvocationKind::ManualOnly` with `manual_only: true`.
- Resolving a manual-only skill does NOT authorize the agent to bypass the restriction by reading the raw skill file.

### Invariant 8: Security and Diagnostic Hygiene
- Diagnostics and error messages never include private canary tokens, credentials, or raw prompt text.

## 3. Implementation and Verification

- Implementation: [`src/roster/explicit.rs`](../src/roster/explicit.rs)
- Unit & Contract Tests: [`tests/roster_contract.rs::local_explicit_resolution`](../tests/roster_contract.rs)
- Contract Boundary: `p2_explicit_resolution` (Assertion ID: `no_network_needed`)
