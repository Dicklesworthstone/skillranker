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
