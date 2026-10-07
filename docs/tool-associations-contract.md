# Tool Associations and Local Load Evidence Contract (P3)

This document formalizes the tool/result association, argument/result summarization, remote provider context filtering, and local skill load evidence extraction contract (`p3_tool_associations`, bead `sr-roadmap-l1i.4.7`).

## 1. Scope and Mission

SkillRanker observes agent tool events in order to detect:
1. When a skill has been invoked or loaded into context.
2. Whether the invocation succeeded (`ObservedLoaded`) or failed/aborted (`Attempted`).
3. What historical content version was consumed (if reliably provided by content-addressed metadata, or `None` if path-only).

This contract ensures that tool evidence is extracted safely, bounds are enforced, secrets are not leaked, and remote provider context is strictly controlled.

## 2. Invariants

### Invariant 1: Association of Invocations and Results
- A tool invocation event (`EventKind::ToolInvocation`) is paired with its matching tool result event (`EventKind::ToolResult`) by `call_id`.
- If an invocation does not receive a matching result event (e.g. session interrupted, task aborted, or crash), it is treated as an incomplete attempt with status `ToolStatus::Attempted`.
- Legacy or single-turn tool events containing both arguments and results in one event are supported.

### Invariant 2: Argument Allowlisting and Head/Tail Excerpts
- Tool arguments are summarized using allowlisted JSON keys:
  `["command", "path", "file_path", "query", "skill", "skill_name", "name", "pattern", "target", "args", "subcommand"]`.
- Non-allowlisted argument keys are replaced with `"<omitted>"` or stripped.
- Tool arguments and results are bounded by head/tail excerpting (default 200 Unicode characters total, `DEFAULT_TOOL_EXCERPT_CHARS`) with explicit omitted count markers (`... [N chars omitted] ...`).
- Error lines in tool outputs (e.g. `error:`, `failed:`, `fatal:`, `panic:`, `exit status:`) are detected and preserved in `error_lines`.

### Invariant 3: Remote Context Filtering (`--no-tools`)
- The `--no-tools` flag removes tool arguments and tool results from the context serialized to the provider.
- `filter_events_for_provider` strips arguments and results when `no_tools` is enabled.
- **Local evidence retention**: Local load evidence extraction (`extract_load_observations` and `extract_loaded_skill_records`) is executed on raw normalized events *before* remote context filtering. Local evidence is fully preserved even when `--no-tools` is active.

### Invariant 4: Load Evidence States
- **ObservedLoaded**: A structured tool invocation targeting a recognized skill with `ToolStatus::Succeeded` and no unrecovered error lines.
- **Attempted**: A tool invocation targeting a recognized skill that failed (`ToolStatus::Failed`), was attempted without result (`ToolStatus::Attempted`), or returned error output.
- **NotObserved**: A candidate skill that was not invoked or loaded during the session history.
- **Unobservable / Censored**: Gaps, compaction loss, or missing tail events. Missing events are never treated as negative usefulness labels.

### Invariant 5: No Current-File Hashing for Historical Version
- When a skill load is observed via a path-only read (e.g. `read_file` of `SKILL.md` without embedded content digest), `source_content` and `rendered_content` MUST remain `None`.
- The current file on the local filesystem must NEVER be hashed to claim it was the historical version consumed by the agent.

### Invariant 6: Branch DAG and Fork Isolation
- Load observations and loaded skill records are strictly attributed to the branch where they occurred.
- When resolving evidence for an `ActiveBranch`, tool events occurring on sibling branches or forks are strictly excluded.
- Subagent / fork tool executions do not leak into parent or sibling branches.

### Invariant 7: Idempotent Deduplication
- Repeated delivery of identical tool events (matching `event_id`) are deduplicated and processed exactly once.

## 3. Implementation and Verification

- Implementation: [`src/context/tool.rs`](../src/context/tool.rs)
- Unit & Contract Tests: [`tests/context_contract.rs::tool_associations`](../tests/context_contract.rs)
- Contract Boundary: `p3_tool_associations` (Assertion ID: `tool_association_exact`)

## 4. Explicit observation command boundary

`sr observe` validates the shared configuration registry before reading its
selected source or accessing the ledger. Unknown `SR_*` variables, malformed or
duplicate configuration, forbidden project settings, and invalid deadlines are
errors. The configured deadline starts at process entry and retains the output
and cleanup reserve. Observation remains a local operation and does not call Jev.

Discovery includes configured project and trusted-user skill roots, using the
same authorization and precedence rules as ranking. An explicit `--roster`
replaces discovery; its file-backed records still require authorization through
that plan. Adding a root never adds a skill omitted from an explicit inventory.
For normalized input naming another harness without an explicit roster, only
configured roots are inspected. Claude's inventory is not borrowed; absent
configured roots, observation fails with `unusable-roster` before ledger access.
Configured roots do not establish a native integration or complete inventory.

Normalized `--context` input uses ranking's descriptor-based, bounded regular-file
reader. The 1 MiB limit applies during reading, rather than after an unbounded
allocation. Devices, FIFOs, and symlinks escaping the input's authorized directory
are refused; a symlink to a regular file inside that directory remains readable.
Input refusals do not advance observation cursors or write observations. Both
successful and failed observation operations enter the owned runtime's bounded
shutdown path before returning. Incomplete or late cleanup is reported as a
timeout.

These boundary checks preserve the existing source namespaces, atomic cursor
generation check, load deduplication, and distinction between observed loads and
independent usefulness judgments. Real CLI and SQLite controls live in
[`tests/observation_contract.rs`](../tests/observation_contract.rs).
