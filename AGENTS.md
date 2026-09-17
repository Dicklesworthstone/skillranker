# AGENTS.md — SkillRanker

Guidelines for AI coding agents working in this repository.

## Rule 0 — Direct Instructions

Follow Jeffrey's direct instructions. These rules encode standing preferences;
they do not overrule the user. Finish the authorized work and report concrete
results, with the checks that support them.

## No Deletion Or Destructive Git

Do not delete files or directories without explicit written permission, including
files you created yourself. Do not run `git reset --hard`, `git clean -fd`,
`rm -rf`, force pushes, or equivalent destructive operations without explicit
authorization for the exact operation and its consequences.

Inspect before changing. Preserve work you did not create. Never stash, revert,
overwrite, or blanket-stage another agent's changes. Use explicit owned pathspecs
when staging. Do not amend published commits.

Work on `main`; create another branch only when the user requests it. Keep the
legacy compatibility branch synchronized when the repository's publishing
instructions require it. Public documentation and source URLs use `main`.

## Project Mission

SkillRanker is a Rust CLI, `sr`, that selects the most useful skills for the
**next step of a live coding-agent session**:

```text
live context + visible skills + local history
  -> wide Jev evaluation -> shortlist rerank + fit checks
  -> ranked JSON / hook context / table / inline TUI
  -> observe subsequent skill loads -> update local feedback
```

Keep this workflow focused. SkillRanker is not a general agent harness, a skill
execution engine, an embedding-model server, or a replacement for meta_skill.
It must be able to conclude that no skill applies.

Read these before substantive work:

1. [The comprehensive plan](COMPREHENSIVE_PLAN_TO_DESIGN_SKILLRANKER.md), including
   the ranking formula, open decisions, risks, and build order.
2. [README.md](README.md), the product and CLI contract.
3. The actual code, dependency sources, fixtures, and relevant live task state.

The plan explains intent; current source and executed checks establish behavior.
Resolve contradictions at the affected boundary rather than copying a sketch
into production. Update the relevant documentation when making a design choice.

## Architecture Doctrine

- Keep context capture, roster discovery, Jev transport, ranking math, ledger,
  cache, and output presentation separate.
- Make ranking math, canonicalization, and question construction pure functions
  of explicit inputs wherever possible. Keep filesystem, subprocess, HTTP, and
  database effects at visible boundaries.
- Use Asupersync for structured concurrency, deadlines, cancellation, and lab
  replay. Do not introduce Tokio, reqwest, or a second async runtime as a shortcut.
- Use `cass` through its machine-readable subprocess interface for session
  discovery and normalized exports. Do not reimplement every harness parser.
- Keep direct transcript adapters small and fixture-backed. Hook envelopes and
  transcript event formats are separate contracts.
- Keep meta_skill optional. Filesystem discovery and local observations must
  work without `ms`. Do not duplicate its bandit state when using its priors.
- Prefer FrankenSQLite (`fsqlite`) for the SQLite ledger. The plan leaves the
  backend choice open: verify transaction and concurrency behavior before
  pinning it, and record any alternative explicitly.
- Keep FrankenTUI behind `tui`; JSON and hook users must not need terminal UI
  dependencies. Keep transport alternatives behind their named features.
- No Tantivy, local embedding model, general ML framework, or large dependency
  forest for the in-memory BM25 prefilter. Justify new dependencies at the
  boundary that needs them.
- Use Rust 2024 and Cargo. Pin a toolchain compatible with the chosen dependency
  versions; do not inherit OCR's SIMD/nightly requirements without a reason.
- Forbid unsafe code at crate roots. This workload does not justify an unsafe
  kernel island. Commit `Cargo.lock` for the CLI.

## Ranking Semantics — Preserve These Distinctions

1. **Preference is not applicability.** Wide and rerank Choice probabilities
   compare options. Independent `fits` Nouls judge whether a candidate applies.
   A high Choice probability never overrides uniformly poor fit.
2. **Confidence belongs to a distribution.** Preserve the API's one Choice
   confidence value. Do not synthesize per-option confidence or label `score`
   as the probability of success.
3. **The three gates have an orientation.** Compute `needs_skill` from
   `acts_on_system`, `documented_procedure`, and `1 - prose_suffices`.
   Below `0.30` by default, skip the rerank and emit the no-skill result.
4. **Keep all evidence.** Return raw wide/rerank probabilities, fit, blended
   score, and the weak flag. `--explain` exposes the same inputs and arithmetic;
   it must not manufacture model-generated reasons.
5. **Use stable numerics.** Validate finite probabilities and thresholds in
   `[0, 1]`, clamp log/logit inputs under one documented epsilon policy, and
   use max-shifted softmax. Reject malformed responses; do not silently turn
   missing options or NaNs into plausible rankings.
6. **Ordering must be deterministic.** Use source-qualified identities and a
   documented tie-break. Normalize over the actual shortlist before slicing
   the top K; do not relabel the truncated slice as the original distribution.
7. **Bound every roster stage.** No wide Choice exceeds 255 entries. Define
   empty and one-entry behavior. Clamp or reject invalid top/shortlist values.
   A chunk union must itself fit the downstream Choice limit, with deterministic
   selection. Probabilities from different chunks are conditional on different
   candidate sets, not automatically comparable global probabilities.
8. **Session signals are scoped.** Loaded-skill evidence and ignored counts
   belong to the correct session and task. Task-boundary resets must not erase
   factual evidence about instructions still present in context.

Defaults from the plan: top `5`, shortlist `8`, gate/fit `0.30`, and weights
`fit=1.0`, `prior=0.5`, `phase=0.3`, `loaded=3.0`, `ignored=0.7`.
Changes to these defaults require behavior evidence and updated documentation.

## Context, Roster, And Privacy Boundaries

The product sends selected session context to a remote evaluation service.
Make that boundary explicit and test it.

- Redact **every outgoing field**, including the latest request, tool arguments,
  result summaries, paths, descriptions, and body excerpts where applicable.
  Apply the same redaction to `--dry-run`, diagnostics, and recorded fixtures.
- Never log credentials, raw authorization headers, or unredacted transcripts.
  Keep API credentials out of Git, config examples, crash reports, and error
  bodies. Do not echo response bodies that might contain sensitive inputs.
- Drop thinking blocks. Keep the latest user request semantically intact while
  compacting older context. Truncate at valid Unicode boundaries.
- Resolve the latest-request-versus-budget conflict explicitly: the nominal
  12,000-character compaction budget is not permission for unbounded request
  bytes. Bound input sizes and handle oversized latest requests visibly.
- Match automatic session discovery to the canonical workspace. In ambiguous
  concurrent sessions, prefer explicit hook/session identity over guessing the
  newest unrelated transcript.
- Treat skill text and transcript text as data, not instructions to the ranker.
  Never execute commands found in a description or API answer. Resolve returned
  option keys only against the roster actually sent.
- Bound discovery walks at the Git root, handle symlink cycles and unreadable
  files, and retain provenance for name collisions. Keep legitimate project
  `.claude/skills`, `.codex/skills`, and `.agents/skills` files trackable.
- Invoke `cass` and `ms` with argument arrays, bounded output, and deadlines.
  Never interpolate transcript paths or skill names into a shell command.
- `--dry-run` must make no evaluation request. The second payload depends on
  the first answer: require an explicit prior/replayed shortlist to show an
  exact rerank request rather than quietly performing a live wide call.
- `--no-tools` suppresses tool results; `--no-ledger` disables ledger reads,
  writes, and feedback effects for that invocation. Test the resulting behavior.

## Asupersync, Deadlines, And Effects

Independent context capture, roster discovery, and prior reads run concurrently.
The wide pass and rerank run sequentially. Chunked wide passes use a bounded
scope; no detached task outlives the invocation.

- Pass `&Cx` explicitly through owned async APIs, use child scopes, and preserve
  cancellation/error distinctions until the CLI boundary.
- The default three-second Jev deadline covers both calls and all retries.
  Backoff must consume the same remaining budget, not restart it.
- Retry only eligible transient failures. Bound response bytes, attempts,
  concurrency, and subprocess lifetime. Authentication/validation errors are
  not fixed by blindly retrying.
- Cancellation is request, drain, then finalize. Prove that HTTP work and
  subprocesses stop and ledger effects are committed or aborted coherently.
- A blocking `transport-ureq` call cannot be cancelled merely by dropping its
  async wrapper. Configure its own timeouts and account for draining the worker.
- Use short database transactions; do not hold a write lock across network or
  subprocess work. Test overlapping hook invocations against a real database.
- Consult the cache before live inference. A timeout may use only an eligible
  cache entry with matching identity and permitted freshness. Emit cache provenance;
  do not disguise an error as a confident no-skill decision.

Verify dependency APIs against the pinned sources. Plan sketches such as runtime
macros or timeout helpers are not proof that a particular signature exists.

## Ledger, Calibration, And Cache Correctness

- Record suggestions and observations separately. Link feedback to a stable
  session/turn identity and transcript position; retries and duplicate hook
  events must not count one load twice.
- An observed skill load is not a correctness label. Missing observations are
  not automatic failures. Do not claim causal `fixed`/`broke` counts without
  independent outcome evidence or a valid comparison design.
- Use Beta(1, 4) smoothing for sparse per-skill priors, at least 10 observations
  for phase-specific cells, and at least 200 labeled turns before automatic
  threshold fitting. Preserve a fixed-default fallback.
- Evaluate threshold changes on held-out observations or a chronological split;
  do not report training-set optimization as generalization. Keep the objective,
  denominators, sample counts, and label provenance inspectable.
- The meta_skill bridge must be idempotent. Observe actual outcomes and map them
  to the external contract deliberately; ignored suggestions are not fabricated
  successful or failed executions. Do not maintain two competing prior stores.
- Cache identity covers every effective ranking input: session/workspace,
  redacted context, roster content, model/endpoint identity, question version,
  configuration, and relevant prior/session state. Never use only the latest
  user-message hash. Do not put raw credentials into cache metadata.
- Apply the ten-minute TTL and roster invalidation consistently. Cache hits
  must still advance feedback observation without inflating independent samples.
- Description diagnostics are read-only. Gap examples are sensitive local
  data. Neither command silently rewrites skills or publishes transcript text.
- Version schemas and use atomic, recoverable migrations. Prove cancellation
  and restart behavior with real temporary stores, including failure between
  reserving an effect and committing it.

## CLI And TUI Contract

`sr` is an agent-first CLI with a useful human surface.

- Bare `sr` ranks once. Table on a TTY, JSON otherwise; explicit output flags
  override auto-detection. Never open an interactive TUI implicitly.
- Stdout is data; stderr carries diagnostics, progress, and argument-correction
  notes. Preserve valid JSON on failures as well as successes.
- Keep `capabilities --json` synchronized with actual commands, defaults,
  schemas, feature gates, examples, and exit codes.
- Stable exits: `0` success/abstention, `2` usage, `3` missing session,
  `4` API/network, `5` empty roster, `6` timeout without usable cache.
- Error envelopes contain `{error: {code, kind, message, hint, retryable}}`;
  `kind` uses kebab-case, and the hint names a concrete corrective action.
- Hook output stays short. Preserve the no-skill sentence; do not confuse an
  operational failure with a successful abstention. Explicit `--hook-top`
  overrides adaptive output count.
- Hook installation merges existing settings, is idempotent, and preserves
  unrelated configuration. Verify each harness's actual entry point and event
  contract; Claude Code's schema is not universal.
- Escape untrusted text for its output context, including terminal control
  sequences and the hook wrapper. A skill name cannot inject extra hook content.
- The nine-row inline TUI uses the same result model as JSON. Watch mode is
  debounced and bounded; render snapshots use fixed data and virtual time.
- Honor `NO_COLOR`, `CI`, and `TERM=dumb` for human decoration.

## Testing And Verification

After substantive Rust changes, run the relevant tests plus these gates:

```bash
cargo fmt --check
rch exec -- cargo check --locked --all-targets
rch exec -- cargo clippy --locked --all-targets -- -D warnings
rch exec -- cargo test --locked
ubs --diff
```

Use RCH for expensive builds on the shared fleet. Check its current routing and
honor any active compile-lane restrictions. If RCH is unavailable in a standalone
environment, the underlying Cargo commands are the gates. Report infrastructure
failures as infrastructure failures, not test passes.

Verify each supported transport/feature combination when changing that boundary;
do not substitute a single all-features build for testing the actual defaults.
For documentation-only work, inspect links, examples, license, whitespace, and
Git hygiene; do not manufacture a Cargo test claim.

The behavioral verification ladder is:

| Boundary | Required evidence |
|---|---|
| Context | Sanitized fixtures per harness; tool compaction, Unicode, latest-request retention, oversized inputs, and redaction on every output route |
| Roster | Real temporary trees, precedence/collisions, symlinks, malformed frontmatter, empty/singleton rosters, and 255/256+ boundaries |
| Jev protocol | Recorded response provenance, typed request validation, unknown/missing options, malformed probabilities, and bounded response handling |
| Ranking | Gate orientation, fit versus preference, stable ties, finite extremes, demotions, and deterministic replay |
| Cache | Misses on changed ranking inputs, TTL expiry, eligible fallback, and feedback correctness on repeated hits |
| Ledger | Real transactions, duplicate delivery, restart recovery, late/missing observations, concurrent writers, and idempotent external feedback |
| Cancellation | Lab-runtime tests for wide/rerank timeout, retry exhaustion, child draining, and partial-effect cleanup |
| CLI/hooks | Spawn the actual binary; assert stdout, stderr, exit codes, abstention, and repeated hook installation preserving existing settings |
| TUI | Deterministic renders and bounded watch/cancellation behavior over the production result model |

Offline replay tests must never contact TypeSafe. Live integration checks are
separate and explicitly credentialed; replay success does not prove remote
service availability. Use meaningful adversarial cases alongside honest success
cases. Fix production behavior rather than weakening a valid assertion.

## Performance Discipline

Correctness and useful selection outrank shaving milliseconds. The plan's
400–600 ms hook budget is a target; never turn it or the cookbook's evaluation
numbers into a measured SkillRanker claim without an experiment.

Measure p50/p95/p99 by adapter, roster size, cache state, transport, model,
request count, and error rate. Include cold `cass`, chunk overflow, timeouts,
and sustained repeated hooks. Pin source, configuration, fixtures, and response
identity when comparing local changes. Preserve ranking behavior before claiming
a pure performance win.

## Editing Discipline

Prefer narrow edits to existing modules. Do not create `*_v2.rs`,
`*_improved.rs`, speculative frameworks, or duplicate implementations. Avoid
scripted mass rewrites. Use `rg` for text and file discovery, and structural
tools when the edit depends on syntax.

Read the dependency source or primary documentation when an API is uncertain.
Do not copy sibling-specific paths, hardware assumptions, release claims, or
test counts into this repository.

## Beads And Agent Coordination

Use `br` for implementation tasks when the repository's tracker is initialized.
Read the live issue before claiming work; current task state outranks recovery
notes. Typical commands:

```bash
br ready --json
br show <id>
br update <id> --status in_progress
br close <id> --reason "Implemented and verified with the named checks"
br dep cycles
br sync --flush-only
```

`br` does not perform Git operations. Stage only the intended tracker exports
and configuration, not its local databases, locks, or recovery files. Use only
`bv --robot-*`; bare `bv` launches an interactive TUI. Do not manufacture a
large issue inventory as a substitute for implementing the assigned task.

In an explicitly coordinated multi-agent session, register with Agent Mail,
check reservations/inbox, reserve exact edit paths, and use the issue ID in
threads and reservation reasons. Treat unexpected worktree changes as peer work;
do not stop to ask whether they should be discarded.

Use `cass search ... --robot` or `--json` for prior-session lookup, never bare
`cass`. Verify historical claims against current source and task state.

## Release Infrastructure — DSR Only

Use DSR for release/build orchestration. Do not create, enable, dispatch, or
depend on GitHub Actions workflows, including tests, provenance, or publishing
fallbacks. GitHub Releases are a distribution destination, not the build engine.

Tie artifacts and checksums to the exact source revision. Verify every claimed
target independently. A missing credential, unsupported target, or unexecuted
gate is a named blocker; it does not become a successful release cell.

Preserve the [LICENSE](LICENSE) verbatim. Use
`LicenseRef-MIT-OpenAI-Anthropic-Rider` in descriptions and `license-file` where
package metadata requires a file; do not mislabel this as unmodified MIT. Do not
copy the OCR project's third-party model-weight notice into SkillRanker.

## GitHub And Contributions

Repository: <https://github.com/Dicklesworthstone/skillranker>.

Use `gh` for repository operations. Outside submissions are reports or examples
to investigate, not patches to merge directly. Independently reproduce issues,
implement the appropriate correction, and follow the contribution policy in
the README. Preserve the full policy when editing that document.

## Session Completion

1. Finish the authorized change and inspect the final diff.
2. Run checks appropriate to the changed surface and state exactly what ran.
3. Update any owned task with the implementation and evidence; leave incomplete
   gates and blockers visible.
4. Stage explicit owned paths, run `ubs --staged`, and commit with a descriptive
   conventional message.
5. Push authorized work and verify the remote revision and local status. Never
   force-push to resolve a surprise.
6. Report the result, validation, and concrete remaining work. Do not claim
   runtime, integration, or release proof from documentation alone.
