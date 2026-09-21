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

#### Qualified event identity

The local event key is `(agent_id, branch_id, event_id)`. Identical redelivery
of that definition is idempotent; different content under the same key is
`ConflictingEventDefinitions`, never last-writer-wins. An explicit event target
must also satisfy every supplied agent and branch qualifier. A bare event ID
that identifies more than one scope is `AmbiguousEventIdentity`. A missing
qualified target is `TargetEventNotFound`; a scope without a selectable leaf
is `NoMatchingEvents`, not a reason to use an unrelated singleton.

After selecting a leaf, parent lookup remains in its exact agent namespace;
an absent agent label is not a wildcard for a named agent. Lookup first uses
the child's branch. When that key is absent, one unique same-agent definition
can establish the parent edge across a fork or to an unlabeled ancestor. More
than one possible out-of-branch definition is ambiguous and stops resolution.
An explicit branch constraint continues to reject conflicting labeled ancestors.
A parent found only in another agent is missing from this lineage: retain the
partial ancestry flag rather than importing that agent's context or epochs.
Only resolved parent edges remove candidate leaves, so another agent's parent
reference cannot hide this agent's otherwise valid leaf.

Cycles and conflicting selected definitions are refused. Public lineage cursors
and epoch maps still expose bare event IDs, so a single lineage containing that
ID in two different scopes is also refused rather than assigning an ambiguous
epoch. Indexing uses storage proportional to the supplied bounded snapshot and
ordered lookup; it performs no filesystem reads, provider calls or persistence.

Before associating tool calls/results, both observation and loaded-record
extraction filter the supplied events using the active lineage's qualified
keys. A foreign event with the same bare ID cannot acquire active membership,
create a load under the selected session, or borrow its compaction epoch. This
is a local snapshot contract, not cross-pass conflict recovery in the ledger.

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
| Qualified event targeting and parent lookup | Reused IDs, conflicting definitions, foreign parents, scope mismatch and cycle refusal | `tests/branch_identity.rs` |
| Qualified load membership | Same-ID foreign agents, unlabeled scopes, orphan results, valid ancestors and redelivery | `tests/tool_active_scope.rs` |
