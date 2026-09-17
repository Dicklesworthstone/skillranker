# Active Branch Resolution, Compaction Epochs, and Worktree Identity Contract

Contract specification and verification rules for branch-aware event DAG lineage,
context window compaction epochs, loaded-state reference vs workflow filtering,
and canonical worktree identity.

Satisfies boundary `p3_branch_resolution` (`sr-roadmap-l1i.4.4`).

## Invariants and Semantics

### 1. Directed Acyclic Graph (DAG) Branch Lineage
- Native transcript events form a branch-aware tree or DAG via `event_id` and `parent_id` pointers, not a flat chronological stream.
- Active branch resolution traces backwards from the active leaf or target event to root (`root -> ... -> leaf`).
- **Timestamp ordering alone cannot identify a fork**: events may arrive out-of-order, be interleaved with concurrent subagents, or have clock skew. Active lineage is strictly established by following `parent_id` links.

### 2. Sibling Branch and Subagent Isolation (`no_branch_confusion`)
- When divergent branches share an ancestor (forks, subagents, parallel turns), events on sibling branches are strictly excluded from the active branch.
- Invocations, tool results, and skill loads occurring on a sibling branch do not leak into the active branch's context or eligibility calculations.
- Satisfies assertion `no_branch_confusion` and E2E case `sibling-branch-isolation`.

### 3. Context Compaction, Resumed Epochs, and Task Boundaries
- `EventKind::Compaction` marks an epoch boundary. Every compaction advances the context epoch (`epoch-0`, `epoch-1`, ...).
- `EventKind::TaskBoundary` records task boundaries within a branch.
- After compaction, prior content availability becomes *unknown* unless an adapter explicitly proves that the content survived.

### 4. Loaded-State Filtering: Reusable Reference vs Workflow
- **Workflows (`SkillUsageKind::Workflow`) and unknown usage kinds (`SkillUsageKind::Unknown`) remain eligible for re-invocation**: prior loading does not prove that a new execution is unneeded.
- **Reusable references (`SkillUsageKind::Reference`) are suppressed only if**:
  1. The skill was loaded on the *active branch* (sibling loads do not count).
  2. The skill was loaded in the *current context epoch* (compaction wipes proven availability).
  3. Content hashes (`source_content` and `rendered_content`, if present) match the candidate version.
  4. The invocation did not have dynamic arguments (`!has_dynamic_arguments`).
  5. The invocation was not turn-scoped (`!turn_scoped`).

### 5. Withholding Advice on Unresolved Branches
- If the event history contains ambiguous sibling leaves and no explicit target event or branch is provided to disambiguate them, the branch cannot be safely resolved.
- SkillRanker withholds session-specific advice and loaded-state suppression (`BranchAdvice::Withheld`, `BranchResolution::Unresolved(AmbiguousSiblingForks)`) rather than merging sibling histories or making an arbitrary guess.

### 6. Canonical Worktree Identity
- Worktree identity (`WorkspaceId`) is derived exclusively from the canonical directory path (`canonicalize()`), ensuring that linked worktrees sharing a `.git` common directory receive distinct local identities.
- Git branch name is parsed from `HEAD` (`ref: refs/heads/<branch>`).
- Detached HEAD commits are detected and identified (`is_detached_head = true`, `git_branch = None`).
- Non-Git workspaces resolve canonical directory identity cleanly without requiring Git repository files.

## Verification Matrix

| Assertion / Case | Requirement | Verification Location |
|---|---|---|
| `tests/context_contract.rs::branch_and_worktree` | Full contract verification: forks, timestamps, compaction, reference vs workflow, worktree | `tests/context_contract.rs` |
| `sibling-branch-isolation` | Sibling events are excluded from active lineage | `tests/context_contract.rs` |
| `no_branch_confusion` | Sibling loads do not suppress active candidates | `tests/context_contract.rs` |
| Out-of-order timestamps | Parent links prevail over skewed timestamps | `tests/context_contract.rs` |
| Compaction epochs | Epochs increment on compaction; compacted references become eligible | `tests/context_contract.rs` |
| Worktree isolation | Linked worktrees receive distinct `WorkspaceId`s; detached HEAD detected | `tests/context_contract.rs` |
