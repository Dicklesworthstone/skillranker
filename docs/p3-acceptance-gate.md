# Phase P3 Acceptance Gate: Exact-Session Context and Privacy Foundation

The Phase P3 exact-session context and privacy foundation contracts are accepted under
boundary `p3_acceptance_gate` (`sr-roadmap-l1i.4.14`).

Closing this gate establishes the verified exact-session context ingestion, branch resolution,
authoritative prompt overlays, capability-checked cass archive bridging, tool load associations,
bounded request rendering, task anchor preservation, project signal collection, disclosure
profiles, byte-parity disclosure receipts, and fail-closed conformance error handling required
before Phase P4 core CLI and ranking pipeline assembly.

## Scope and Boundary Inventory

Phase P3 encompasses 14 distinct contract boundaries mapped in `tests/contract_matrix.toml`
and bound by `tests/contract_authority.toml`:

| Boundary ID | Owner Bead | Title | Unit/Property Test References | E2E Cases & Assertions |
|---|---|---|---|---|
| `p3_source_selection` | `.4.1` | Source selection & source-mode errors | `tests/source_selection.rs::exact_source_selection`<br>`tests/source_selection.rs::failed_chosen_source_never_reads_a_successful_neighbor`<br>`tests/source_selection.rs::linked_worktrees_are_distinct_despite_a_shared_git_directory` | `no-cross-session-fallthrough`<br>[`exact_session_bound`] |
| `p3_normalized_envelopes` | `.4.2` | Bounded normalized envelopes | `tests/context_contract.rs::normalized_envelopes` | `bounded-normalized-parsing`<br>[`normalized_bounded`] |
| `p3_native_jsonl` | `.4.3` | Native JSONL snapshots & cursor generations | `tests/context_contract.rs::native_jsonl_incremental` | `partial-tail-deferred`<br>`compaction-detected`<br>[`incremental_clean`] |
| `p3_branch_resolution` | `.4.4` | Active branches, compaction & boundaries | `tests/context_contract.rs::branch_and_worktree` | `sibling-branch-isolation`<br>[`no_branch_confusion`] |
| `p3_claude_overlay` | `.4.5` | Authoritative Claude prompt overlay | `tests/context_contract.rs::claude_prompt_overlay` | `hook-prompt-overlaid-once`<br>[`authoritative_prompt`] |
| `p3_cass_adapter` | `.4.6` | Optional cass archive adapter | `tests/context_contract.rs::cass_adapter` | `cass-bounded-pipe`<br>`offline-cass-mock`<br>[`cass_safe`] |
| `p3_tool_associations` | `.4.7` | Tool/result associations & load evidence | `tests/context_contract.rs::tool_associations` | `loaded-evidence-extracted`<br>[`tool_association_exact`] |
| `p3_request_context` | `.4.8` | Bounded request-first context & provenance | `tests/context_contract.rs::bounded_request_rendering` | `visible-head-tail-truncation`<br>[`rendered_within_budget`] |
| `p3_task_anchors` | `.4.9` | Task anchors & explicit directives | `tests/context_contract.rs::task_anchors` | `task-anchor-preserved`<br>[`anchor_intact`] |
| `p3_project_signals` | `.4.10` | Bounded project signals | `tests/context_contract.rs::project_signals` | `dirty-paths-repo-relative`<br>[`bounded_project_signals`] |
| `p3_disclosure_profiles` | `.4.11` | Standard/minimal disclosure profiles | `tests/privacy_contract.rs::disclosure_profiles` | `minimal-profile-drops-tools`<br>[`profile_respected`] |
| `p3_disclosure_receipts` | `.4.12` | Bounded field-level disclosure receipts | `tests/privacy_contract.rs::disclosure_receipts` | `disclosure-receipt-matches-payload`<br>[`receipt_accurate`] |
| `p3_conformance_matrix` | `.4.13` | Native/normalized/cass adapter conformance | `tests/context_failures.rs::context_failure_cases` | `corrupted-jsonl`<br>`oversized-record`<br>`missing-prompt`<br>[`safe_context_rejection`] |
| `p3_acceptance_gate` | `.4.14` | Phase P3 acceptance gate | `tests/p3_gate.rs::all_p3_invariants_verified` | `full-context-suite`<br>[`p3_gate_passed`] |

## Invariant Verification

The unified Phase P3 test suite (`tests/p3_gate.rs`) exercises all 13 core invariants in a single cohesive gate:

1. **Source Selection & Exact Session Binding**:
   - Conflicting source flags (e.g. both `--transcript` and `--cass`) fail closed with `SourceError::ConflictingSourceModes`.
   - A missing or failing chosen source never falls through to an unchosen neighbor (`SourceError::SourceNotFound`).
   - Linked worktrees maintain distinct canonical identities despite sharing a `.git` root.

2. **Normalized Context Envelopes**:
   - Strictly validates `NormalizedContext` JSON schemas with `#[serde(deny_unknown_fields)]`.
   - Rejects unannounced payload fields and preserves typed `Role` (User, Assistant, System).

3. **Incremental Native JSONL Snapshots & Generations**:
   - `snapshot_jsonl` defers incomplete trailing records without failing (`incomplete_tail: true`).
   - Appending lines continues from the established cursor generation without rebuilding.
   - File compaction or replacement is detected and causes generation advance (`rebuilt: true`).

4. **Active Branch Resolution & Sibling Isolation**:
   - Resolves message ancestry via parent links rather than sorting timestamps.
   - Sibling branches from subagents or alternate forks are strictly excluded.
   - Workflows remain eligible for repeat invocation; reference skills are suppressed only when proven present in the current epoch with matching content hash.

5. **Authoritative Claude Prompt Overlay**:
   - Hook stdin prompt overlays the transcript tail exactly once.
   - Re-applying the overlay detects the existing turn by identity and prevents duplication.
   - Missing prompt text fails closed with `OverlayError::MissingPrompt`.

6. **Capability-Checked Cass Archive Adapter**:
   - Validates cass binary capabilities against exact requirements (crate 0.8.0, API 1, contract "1", commands `export` and `sessions`).
   - Rejects path traversal attempts and enforces trusted executable boundaries.

7. **Tool / Result Associations & Load Evidence**:
   - Pairs tool use events with tool result events by `call_id`.
   - Extracts loaded skill claims and observations for eligibility filtering.

8. **Request-First Context Rendering & Bounds**:
   - Bounded rendering strictly conforms to character/scalar-value budgets (12,000 Unicode scalar values default).
   - Applies visible head-tail truncation markers when requests exceed budget.
   - Sanitizes binary media with explicit omission markers (`IMAGE_OMISSION_MARKER`).
   - Secret redaction is executed prior to any truncation.

9. **Task Anchors & Explicit Directives**:
   - Preserves original task anchors across lossy rendering and compaction.
   - Redacts credentials and sensitive patterns within task anchors.

10. **Bounded Project Signals**:
    - Git status parsing enforces repository-relative paths and rejects parent-directory escapes (`..`).
    - Strips non-UTF8 paths and enforces `DIRTY_PATH_LIMIT`.
    - Git helper invocation ignores `core.fsmonitor` to prevent arbitrary command execution.

11. **Standard and Minimal Disclosure Profiles**:
    - `DisclosureProfile::Minimal` strips all tool arguments and results from provider payloads.
    - `DisclosureProfile::Standard` preserves bounded tool summaries.

12. **Field-Level Disclosure Receipts**:
    - `DisclosureReceipt` entries match outgoing byte counts and SHA-256 digests exactly.
    - Diagnostic string formatting and debug output are audited to guarantee zero secret leakage.

13. **Conformance Failure Rejection**:
    - Rejects corrupt JSONL records, oversized lines exceeding buffer limits, and malformed overlays with typed errors.

## Execution and Evidence Receipts

- **Unit and Integration Verification**:
  ```bash
  rch exec -- cargo test --test p3_gate
  rch exec -- cargo test --test context_contract
  rch exec -- cargo test --test source_selection
  rch exec -- cargo test --test privacy_contract
  rch exec -- cargo test --test context_failures
  ```
- **Contract Matrix Conformance**:
  ```bash
  python3 -I -B scripts/validate_contract_matrix.py
  ```
- **End-to-End Context Mechanics Suite**:
  ```bash
  scripts/e2e/run.sh --suite context --binary /usr/bin/python3 --artifacts /tmp/context-e2e-artifacts
  ```
  All 18 scenarios in `scripts/e2e/suites/context.json` execute under sandbox isolation and pass with `status: "passed"` and matching assertion IDs.
