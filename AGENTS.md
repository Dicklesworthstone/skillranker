# AGENTS.md — SkillRanker

Guidelines for AI coding agents working in this repository.

## Rule 0 — Direct Instructions

Follow Jeffrey's direct instructions. These rules encode standing preferences;
they do not overrule the user. Finish authorized work and report concrete results,
with the checks that support them.

## No Deletion Or Destructive Git

Do not delete files or directories without explicit written permission, including
files you created yourself. Do not run `git reset --hard`, `git clean -fd`,
`rm -rf`, force pushes, or equivalent destructive operations without explicit
authorization for the exact operation and its consequences.

Inspect before changing. Preserve work you did not create. Never stash, revert,
overwrite, or blanket-stage another agent's changes. Stage explicit owned paths.
Do not amend published commits.

Work on `main`; create another branch only when the user requests it. Keep the
legacy compatibility branch synchronized when publishing instructions require it.
Public source URLs and documentation use `main`.

## Project Mission And Reading Order

SkillRanker is a standalone Rust CLI, `sr`, that recommends the most useful skills
for the **next step of a specific live agent session**:

**TypeSafe.ai's Jev is the system's essential ranking engine. A TypeSafe API key
is required for the product's ranking workflow.** Keep this dependency prominent
in product descriptions, installation, and onboarding. Context capture, Quill retrieval,
caching, local scoring, and feedback support Jev; they are not a replacement
inference system. Do not imply that `sr` has a key-free ranking backend.

```text
exact session + visible roster + user constraints
  -> bounded, redacted state -> local explicit resolution or Jev ranking
  -> eligibility + abstention -> JSON / table / Claude hook / inline TUI
  -> local observations + independent judgments -> evaluated policy changes
```

Keep this workflow focused. SkillRanker does not execute skills, grant tool
permissions, override user requirements, or stop an agent from working when the
recommendation service fails.

Read these before substantive work:

1. [The comprehensive plan](COMPREHENSIVE_PLAN_TO_DESIGN_SKILLRANKER.md), including
   core invariants, trust policy, dependency choices, and acceptance gates.
2. [README.md](README.md), the product and command contract.
3. Actual source, dependency versions, fixtures, and relevant live task state.

The reviewed plan supersedes its earlier sketches: there is no `ms` bridge,
ignored-suggestion penalty, transparent `ureq` fallback, or HTTP-only deadline.
Resolve contradictions at the affected boundary and update the relevant docs.
Current source and executed checks establish behavior; prose alone is not proof.

## Maintainer Credentials — Local Environment And Vault

**Live Jev evaluations require `TYPESAFE_API_KEY`.** The maintainer credential is
available in these locations; retrieve it without printing it:

| Location | Lookup |
|---|---|
| This checkout on `threadripperje` | `/data/projects/skillranker/.env` (also `/dp/skillranker/.env`), variable `TYPESAFE_API_KEY` |
| HashiCorp Vault on `threadripperje` | KV v2 mount `secret`, path `skillranker`, field `TYPESAFE_API_KEY` |
| HashiCorp Vault on `ts1` (`thinkstation1`) | The same mount, path, and field in that machine's local Vault |

The local `.env` is Git-ignored, untracked, and owner-readable/writable only
(`0600`). Export its values before launching `sr` or the agent process whose hooks
need the key; do not assume `.env` is loaded automatically:

```bash
cd /data/projects/skillranker
set +x
set -a
. ./.env
set +a
```

For recovery or use on either maintainer machine, run the following **on that
machine** (connect with `ssh ts1` first for `ts1`). Existing Vault authentication
is required. Each machine has a copy of its local Vault's public CA certificate
at `~/.config/vault/skillranker-ca.pem`:

```bash
set +x
TYPESAFE_API_KEY="$(
  VAULT_ADDR=https://127.0.0.1:8200 \
  VAULT_CACERT="$HOME/.config/vault/skillranker-ca.pem" \
  VAULT_SKIP_VERIFY=false \
  vault kv get -mount=secret -field=TYPESAFE_API_KEY skillranker
)" && export TYPESAFE_API_KEY
```

Keep shell tracing off while handling credentials. Never print the key, paste it
into a tool argument, commit it, or include it in logs, fixtures, or documentation.
Keep `.env.example` credential-free. When rotating the key, update the local
`.env` and both Vault copies, preserving unrelated fields. A loaded credential
does not authorize sending session content: network opt-in still applies.

These are private maintainer stores. Outside users must sign up at the
[TypeSafe console](https://console.typesafe.ai) and obtain **their own API key**;
the README must direct them there.

## Architecture Doctrine

- Start with one Rust package, binary `sr`, and a library exposing pure context
  normalization, question construction, validation, eligibility, and scoring.
  Avoid premature workspace fragmentation.
- Keep filesystem, subprocess, network, database, and output effects at visible
  boundaries. Ranking and canonicalization should be pure functions of explicit
  inputs wherever possible.
- Use Asupersync for owned concurrency, deadlines, cancellation, HTTP/TLS, and
  deterministic lab replay. Do not introduce Tokio, reqwest, or another runtime.
- **No runtime meta_skill dependency.** Do not invoke `ms`, link its application
  crate, read its private database/configuration, or send it outcomes. Copy only
  narrow reusable components and tests, with pinned-source provenance and the
  actual notices recorded in `THIRD_PARTY_NOTICES.md` when imports occur.
- Cass is an optional, capability-checked subprocess adapter. Do not auto-index,
  start its daemon, assume exports normalize every event, or assume exported tool
  filtering/redaction satisfies our policy.
- Use `rusqlite` with bundled SQLite for the initial storage backend, as the
  reviewed plan specifies. Evaluate FrankenSQLite separately under the same
  transaction, migration, crash, and concurrency tests before any substitution.
- Keep FrankenTUI behind `tui`. JSON/hook builds must not require terminal UI
  dependencies. The TUI consumes the same result model and scoring policy.
- Use FrankenSearch's `frankensearch-quill` as the sole lexical search engine,
  with its default features disabled and a bounded in-memory adapter. No Tantivy
  use is allowed in runtime, fallbacks, tests, benchmarks, or copied/reference
  code; never enable Quill's optional oracle or legacy lexical features. Check
  resolved normal/build/dev feature graphs. No embedding model, external embedding
  service, or broad ML framework for retrieval. Preserve standalone builds.
- Rust 2024, Cargo, committed `Cargo.lock`, and a toolchain pinned to verified
  dependency requirements. Do not inherit OCR's SIMD/nightly requirements by analogy.
- Forbid unsafe code at crate roots. This workload does not justify SIMD islands
  or unsafe parsers.

## Identity, Visibility, And User Authority

1. **Bind state to workspace, session, and agent branch.** A newest-session guess
   is not the hook's identity. Explicit source failure must not fall through to
   another conversation. Require selection for ambiguity; `--latest` is explicit.
2. **The hook prompt is authoritative.** Claude's submitted prompt may not yet
   be in its transcript. Overlay it once using event identity. Equal prompt text
   alone does not make two user turns duplicates.
3. **Resolve explicit requirements locally.** Structured requests and
   `--require-skill` bypass probabilistic retrieval and gates. Resolve directives
   from full bounded local input before redaction/truncation. Exclusions prevail
   over advisory output. Missing/ambiguous exact requests are reported, not
   replaced with similar candidates. Quoted examples are not positive directives.
4. **Visibility is harness-specific.** `--roster` replaces discovery. Otherwise
   use the harness inventory or a verified adapter's precedence. Do not assume
   Claude sees Codex roots or that every filesystem skill is loadable.
5. **Separate identities.** Keep stable opaque skill ID, actual invocation name,
   sanitized display name, source, content hash, and local load target distinct.
   An invented source-qualified ID is not automatically a valid harness command.
6. **Resolve collisions before output.** Deduplicate canonical files, preserve
   aliases, apply real shadowing rules, and exclude ambiguously invocable names.
7. **Snapshot and revalidate.** Hash/excerpt the same bounded bytes; before
   emission recheck content version and loadability. Changed candidates cannot
   remain actionable under an old response.
8. **Provider output carries no authority.** Resolve option IDs only through the
   local request map. Never execute or trust paths, commands, endpoints, or new
   names supplied by skill text, transcripts, or a model answer.

## Context, Privacy, And Input Bounds

Network transmission is an explicit trusted choice. An API key in the environment
is not permission for an arbitrary project to export session content.

- Network is disabled by default; trusted user configuration or `--allow-network`
  enables it. `--offline` means zero network calls and conflicts with
  `--allow-network`. Offline uses direct/normalized input, not cass; local-only
  commands cannot invoke unverified children or Git fsmonitor helpers.
- Project config may tune allowlisted ranking values/exclusions. It cannot set
  credentials, endpoints/proxies, expanded transcript roots, raw retention,
  redaction overrides, or network authorization.
- Use HTTPS, verified public certificate roots, origin-scoped credentials, and
  disabled redirects. Never use accept-all TLS. Loopback HTTP tests must not
  carry production credentials.
  `TYPESAFE_ENDPOINT` is a base origin with empty/root path; append
  `/v1/systemone` once. Reject userinfo, query/fragment, and non-root paths.
- Redact every outgoing bounded field, including skill descriptions/excerpts,
  current request, tool arguments, and results. Redact before truncation, then
  scan the assembled payload. Never retain matched secret text in diagnostics.
- Drop thinking/reasoning, media/binary data, and earlier `sr` advisories. Render
  the latest request once, within the budget, using visible head/tail truncation
  when necessary. Do not promise unbounded whole-request preservation.
- `--no-tools` removes arguments and results from provider context. Observe local
  load evidence separately before filtering. Keep invocation/result associations.
- Keep absolute workspace paths, branch names, offsets, credentials, and ledger
  identifiers local by default. Use bounded repository-relative `dirty_paths`;
  Git status does not report modification times.
- Read regular, authorized transcript files only. Reject devices/FIFOs and
  unexpected symlink targets. Never follow paths merely mentioned in a message.
- JSONL readers snapshot length, process complete records, defer an incomplete
  tail, and detect replacement/truncation/compaction. Preserve attribution quality
  when history is missing. Do not rewrite the source transcript.
- Bound YAML size, nesting/aliases, discovery walks, and file counts. Missing
  frontmatter may use a title/paragraph fallback; malformed frontmatter is an
  excluded record with a sanitized error, not an empty skill.
  Reject duplicate keys/record definitions within each schema collection/namespace
  in normalized input, rosters, configuration,
  replay/evaluation artifacts, frontmatter, and provider responses. Bound new
  input formats too; policy files cannot execute interpolation or recursive includes.
- Invoke trusted subprocesses with argv arrays, bounded pipes, explicit
  environments, and deadlines. Omit provider credentials from child environments.
  Never interpolate transcript data into shell code.
- `--dry-run` has no network or persistence effects. Stage 2 needs explicit
  shortlist IDs or a validated recorded wide response; do not secretly call Jev
  to make the preview possible.

Initial limits from the plan:

| Resource | Default |
|---|---:|
| Hook stdin | 1 MiB |
| Transcript tail / records | 2 MiB / 2,000 |
| One transcript record | 256 KiB |
| cass stdout | 8 MiB |
| Logical messages / rendered context | 12 / 12,000 Unicode scalar values |
| Skill file / frontmatter | 256 KiB / 16 KiB |
| Discovery files / parsed bytes | 10,000 / 32 MiB |
| Wide excerpt | 160 characters |
| Rerank description / body excerpt | 1,000 / 700 characters |
| Serialized request / decoded response | 96 KiB / 2 MiB |

Validate before allocation and expose truncation/partial coverage. Partial input
cannot justify an unqualified no-skill message.

## Jev Protocol And Ranking Semantics

- A Choice has at most **255 total options: 254 real skills plus `__none__`**.
  The sentinel has a distinct type and cannot collide with a skill ID.
- Default overflow is Quill's deterministic lexical BM25 prefilter. Explicit requests
  resolve before it. Pin tokenization, normalization, field weights, and tie-breaks.
  Build a deduplicated disjunction of escaped literal terms, with size limits
  measured after escaping/separators; no analyzed terms means retrieval-empty.
  An experimental chunk mode needs separate bounds and quality proof; probabilities
  from different chunk candidate sets are not globally comparable.
- Wide gate: mean of `specialized_method`, `material_help`, and
  `1 - context_suffices`. This is a heuristic, not an independent calibrated
  probability. Include planning/explanation skills; acting on files is not required.
- Gate/fit defaults are `0.30`. Sizes satisfy `1 ≤ K ≤ M ≤ 32`, with `K=5`,
  `M=8`, clamped to available candidates. Test zero, one, 254, and 255+ cases.
- The detailed Choice also includes none and may reject every candidate. Each fit
  question includes the skill's meaning; opaque question keys convey no content.
- Parse structured JSON. Reject duplicate keys, missing/foreign option IDs,
  answer-type mismatches, non-finite/out-of-range values, and invalid usage counts.
  Distribution sums must be positive and within the documented `1e-4` tolerance
  of one; normalize only rounding drift and retain raw values.
- Validate `choice` against an argmax, allowing ties. Deterministic local ties use
  stable skill IDs. Cap decoded/decompressed response bytes, not just wire length.
- Filter exclusions, reusable references proven present with matching rendered
  content in the current epoch, and candidates below minimum fit. Workflows and
  unknown usage kinds remain eligible for repeat invocation. A changed shortlist
  invalidates the result. **Each** remaining candidate must individually beat the
  none option's raw probability before blending; ties are excluded. A global
  best-candidate check cannot authorize a worse candidate that fit blending boosts.
- Priors cannot rescue failed eligibility, a low fit, or a none winner.
  `ranked`, `explicit`, `abstain`, and `unavailable` are distinct typed decisions.
- Preserve `wide_probability`, `rerank_probability`, `fits`, `rank_score`, and
  distribution-level `choice_confidence` as distinct quantities. Do not fabricate
  per-skill confidence or free-text model explanations. Unexecuted fields are null.
- Scoring uses epsilon `1e-6`, clipped logarithms/log-odds, and max-shifted softmax.
  Normalize over all eligible shortlist candidates before top-K truncation; report
  omitted mass. A singleton score of one is not certainty of usefulness.
- Default weights: `w_fit=1`, `w_prior=0`, `w_phase=0`. Bounds are respectively
  `[0,4]`, `[0,0.5]`, `[0,1]`. Experimental phase matching uses distribution mass
  over declared phases, not a hard argmax bonus.
- Never penalize a skill just because a prior suggestion was not observed loaded.
  Compaction/uncertain observation invalidates loaded-state suppression.

## Runtime, Deadline, And Failure Behavior

One invocation owns all tasks and child processes. Start the monotonic deadline
at process entry, before stdin/config/discovery. The default is **3,000 ms total**,
with the final **200 ms reserved for output and cleanup**.

Independent capture and discovery may overlap after identity is established.
Wide and rerank calls remain sequential. Pass remaining time into every read,
subprocess, lock, retry, HTTP operation, and persistence effect.

- Use explicit `&Cx`, child scopes, and cancellation/error distinctions until the
  CLI boundary. Cancellation is request, drain, finalize; no detached workers.
- A blocking closure does not become cancellable because it is inside a task or
  timeout. Prove stalled-leaf behavior and check completion timestamps. Do not
  publish a late result while cleanup happens.
- No transparent `ureq` fallback. If the Asupersync transport cannot satisfy DNS,
  trust roots, TLS, bounded POST/response reads, and cancellation, fix the boundary
  or report the exact blocker.
- Default budget: two logical requests and at most four HTTP attempts total.
  Honor valid `Retry-After`, bounded jitter, and the remaining deadline. Do not
  retry authentication, request validation, or malformed answers blindly.
- An attempt without a response can still incur cost. Preserve unknown usage,
  rather than counting it as zero. Cache reads are zero new provider usage.
- Subprocess cancellation must terminate and reap the owned process tree, drain
  pipes without deadlock, and handle signals and broken pipes.
- Never hold a database transaction across network work. SQLite busy waits are
  bounded by 25 ms and the remaining deadline.
- Watch mode permits one active ranking per session, coalesces changes, and uses
  a minimum five-second interval. Generation checks prevent stale publication.

## Cache, Observations, And Calibration

Use separate request and decision fingerprints. Canonical redacted request bytes,
candidate identities/content/excerpts, questions, model/endpoint, adapter, and
privacy versions identify a provider request. Session/branch, current eligibility,
policy/configuration, prior snapshot, and visibility identify a decision.

- Use keyed BLAKE3 fingerprints with a local protected random key. Hashes remain
  sensitive linkable metadata; never place credentials in cache metadata. Both
  fingerprints live in a workspace/session/agent-branch namespace with context
  epoch, adapter, visibility, and key-generation identity. Never share responses
  across sessions merely because redacted text matches.
- Store validated provider responses separately from presentation. Stage 2 is
  keyed by its actual shortlist. Reapply current eligibility before output. A
  partial stage hit is not a complete offline result. With an unversioned alias,
  do not mix an old wide answer and a fresh rerank; refresh the pair or report
  unavailable. TTL starts at response receipt, and clock rollback cannot extend it.
- Only exact, unexpired, revalidated entries may drive hooks. Ten-minute TTL is
  a maximum, not permission to reuse a previous task's ranking on timeout.
- Observe new events before final decision lookup. Exclude operational invocation
  counters from model state; include meaningful tools, constraints, and compaction.
- Duplicate delivery uses event identity, not prompt text alone. Ambiguity is
  exposed and excluded from exposure-based learning. Single-flight locks are
  bounded, namespace/request-scoped, fenced, recoverable, and never long SQLite
  write transactions. An expired owner cannot publish after its successor.
  Share only exact validated responses; consumers keep distinct decisions and
  exposure records. Cross-process result sharing needs the response cache and
  is disabled by `--no-cache`; never hide bodies in coordination state. Only the
  owner incurs each uniquely identified provider attempt.
- Distinguish generated, prepared, emitted, and acknowledged delivery. Database
  commit and stdout cannot form one atomic transaction. Crash ambiguity remains
  unknown; do not claim exactly-once exposure.
- A successful resolved load, attempted load, unobservable load, and censored
  observation are different states. Missing events are not negative usefulness labels.
  A path-only successful read cannot prove the historical content version.
- Ranking-window and observation cursors are separate. Never advance the
  observation watermark across unread bytes. Commit observations, loaded-state
  evidence, and cursor advance atomically using the expected cursor generation.
  Normalized imports cannot update native session state by repeating its IDs.
- Attribute one observed load to the latest eligible preceding emission in the
  same agent/turn. Superseded recommendations are censored. Handle the last turn
  through explicit observation/finalization rather than assuming another prompt.
- Stats report adoption and operational metrics with denominators and unknown
  counts. Precision, fit Brier scores, and task success require independent labels
  or controlled outcomes, not self-reinforcing adoption statistics.
  Historical corrections need full membership/version and eligibility evidence,
  including candidates outside the shortlist. Missing snapshots mean unknown,
  not absence. Deduplicate bounded snapshots and honor quota/retention references.
- Priors start disabled. Judged-usefulness priors are centered/shrunk and keyed by
  skill revision and policy context. Sparse cells fall back; an arbitrary count
  alone never establishes adequate labels or phase coverage.
- Calibration consumes a versioned labeled evaluation artifact, separates
  session/task families, includes an untouched holdout, and penalizes missed useful
  suggestions as well as wrong/needless ones. Preview before explicit `--apply`.
  Fit priors on training, tune thresholds on validation, and evaluate once on the
  final holdout. Prior snapshots predate scored cases; future labels cannot leak.
- Do not invent missing rerank values for low-gate production turns. Re-evaluate a
  consented dataset or use explicitly budgeted shadow evaluation.

Persistence controls are distinct: `--no-cache` disables cache; `--no-ledger`
disables all ledger/cursor reads and writes; `--no-persist` disables both plus
persistent coordinator/key access. `--dry-run` is an exact stateless preview,
creates no keys/logs/locks, and must not claim to include hidden historical state.

Use owner-only platform data/cache directories, short WAL transactions, foreign
keys, and uniqueness constraints. Raw requests/bodies are not stored by default.
Default event retention is 30 days; cleanup/migrations run outside hooks. Busy,
full, corrupt, or newer-schema storage degrades optional learning visibly without
destructive repair. Explicit feedback/admin mutations fail if their required write fails.
Use explicit `sr ledger init` and migration preview/`--apply`, with SQLite-aware
backups that include committed WAL state. Expired rows are excluded logically;
physical removal is explicit, not secure erasure. Bound ledger/sidecars/backups
to 256 MiB and cache/coordinator state to 64 MiB, stopping optional recording at quota.

## CLI, Hooks, And TUI

- Bare `sr` ranks once, table on a TTY and JSON otherwise. Explicit formats win.
  Only `sr tui` enters the interactive UI. Stdin modes are explicit.
- Use strict clap parsing and documented aliases. Reject misspellings, unknown
  or duplicate configuration keys, incompatible modes, and invalid bounds after
  bounded configuration reads and before discovery/network/mutation. Forbidden
  project settings are errors; doctor may explain them without validating the policy.
- Stdout is data; stderr is diagnostics. Keep `capabilities --json` synchronized
  with schemas, adapters/events, compiled features, limits, examples, and exits.
  Distinguish planned commands from implemented capabilities and tested harness
  versions from unverified ones. This repository currently contains design docs.
- Ordinary CLI exits: `0` success, `2` usage/config, `3` session, `4` network/provider/budget,
  `5` roster/retrieval, `6` timeout, `7` input/adapter, `8` privacy, `9` required storage,
  `10` provider contract, `11` offline/cache-only miss. JSON errors include schema version, unavailable decision,
  and `{code, kind, message, hint, retryable}` with stable kebab-case kinds.
- `sr hook claude` accepts its verified `UserPromptSubmit` contract. Advisory output
  uses `hookSpecificOutput.additionalContext`, bounded to 1,024 characters and at
  most one suggested invocation name. Explicit multiple requests are separate.
- Hooks default to shadow; trusted `hook.mode = "advisory"` enables injection.
  Ordinary abstentions are silent; abstention messages require an explicit
  experiment. Scoped snoozes are trusted user preferences, never negative labels,
  and cannot veto explicit skill requirements.
  API/input/privacy/coverage/parser failures mean empty stdout, sanitized stderr,
  and exit zero. Never use blocking decision fields or exit 2 for recommendation
  failure. Classify the dedicated hook boundary before non-exiting argument parsing.
- Hook install/uninstall previews an exact settings diff; `--apply` changes only
  the managed entry with backup, concurrent-edit detection, and atomic replacement.
  Preserve unrelated settings, use an absolute trusted binary path and timeout,
  and test repeated install/uninstall plus malformed configuration.
  Backups are owner-only in private state; previews omit unrelated secrets.
  Atomic rename is not compare-and-swap against external editors. Lock cooperating
  installers, detect conflicts, and document unsupported concurrent external edits.
  A later internal deadline change cannot exceed the installed outer timeout.
- Do not claim native Codex/omp/Grok integration from a guessed equivalent hook.
  Each requires verified event/input/prompt/visibility/output/deadline/delivery
  contracts. Normalized context is the general integration boundary.
- Sanitize terminal controls and escape output formats. Provider/skill text cannot
  inject hook fields, commands, or terminal sequences.
- TUI selection emits a local reference; it never invokes a loader or shell.
  Restore terminal state and separate controlling-terminal rendering from piped
  output. Handle display-cell widths, resize, small terminals, and cancellation.
- Honor `NO_COLOR`, `CI`, and `TERM=dumb` for human decoration.

## Product Improvements And Evidence

The plan's I01–I15 priorities refine P0–P9; they do not authorize extra routine
inference calls or bypass existing quality gates. Keep the core CLI independent
of P9 retrieval/excerpt/description experiments.

- Stage explanations and `--why-not` expose observed exclusion reasons without
  changing candidate admission, request bytes, or scores. Unevaluated is not zero.
- Demo/replay use explicitly non-actionable envelopes. Replay reads bounded case
  data without resolving embedded source paths, executing skills, calling Jev,
  or updating native session state. Capture is a separate explicit privacy choice;
  it cannot reconstruct missing bodies from metadata-only history.
  Freeze evaluation time, local eligibility, numeric priors/phase inputs, and
  computation versions. Replay never consults today's clock/configuration/priors.
  Declare incomplete stages; exporting no cache secret does not prevent artifact
  integrity checks. Publish exports atomically without replacing raced-in targets.
- Minimal context and disclosure receipts remain subject to essential-context
  checks. Project settings cannot widen a trusted disclosure profile.
- An optional enforced shared attempt allowance debits before send and survives
  restart/pruning. Unavailable enforcement state withholds provider requests;
  ordinary optional-ledger degradation remains separate. Budget limits intersect
  invocation/batch caps and apply to retries/probes too. No background probes.
  Activate a durable guard intent before accounting setup, match ready generations,
  and preserve charges through recovery. Limits count admissions, not wire/billing
  timestamps; permits are single-use and deadline/window bound. Late obsolete
  responses cannot reset a newer breaker generation or another profile's auth pause.
- Read explicit snoozes as trusted configuration even when ledger or persistent
  runtime state is disabled. Ranking does not clean up or mutate configuration.
- Corrective feedback is partial and unblinded; an alternative missing from the
  historical roster is prospective feedback, not an original ranking mistake.
  Paired labels commit atomically with revision checks and distinct skill IDs;
  unknown membership is not proof that the alternative was absent.
- Policy rollback restores only managed policy fields after conflict checks;
  it never restores old credentials, network consent, or whole configuration files.
- Preserve label coverage and unknown usage in value reports. No unlabeled
  adoption metric becomes usefulness, financial savings, or task success.
- Passage selection and multiple query views use Quill exclusively, shared
  resource limits, and versioned request fingerprints. Promote only after
  held-out equal-budget comparisons, keeping the original policy available.
- Sequential monitoring tests the declared harm-or-unresolved composite endpoint.
  Missing-only alarms are not harm findings. Post-finalization label corrections
  invalidate the epoch; preserve alarms/spent alpha, audit corrected history,
  and use fresh prospective units/allocation before resuming inference.
  Independent families alone do not justify arbitrary binomial confidence claims.

## Verification And Performance

After substantive Rust changes, run relevant tests plus these gates:

```bash
cargo fmt --check
rch exec -- cargo check --locked --all-targets
rch exec -- cargo clippy --locked --all-targets -- -D warnings
rch exec -- cargo test --locked
ubs --diff
```

Use RCH for expensive builds on the shared fleet and honor active compile-lane
restrictions. In standalone environments without RCH, the underlying Cargo
commands are the gates. Infrastructure failure is not a test pass. Verify actual
default and relevant feature combinations, not only an all-features build.

For documentation-only changes, check links, examples, license, whitespace, and
Git hygiene; do not manufacture a Cargo result.

| Boundary | Required evidence |
|---|---|
| Session/context | Concurrent sessions/subagents, identical prompts, prompt timing, compaction, partial tails, and identity changes |
| Privacy | Secrets across truncation boundaries and every field, unsafe path types, bounded Unicode, trusted configuration, offline behavior |
| Roster | Actual temporary trees, overrides/collisions, malformed YAML, symlink escape/cycles, 0/1/254/255+ candidates, content changes during calls |
| Jev/scoring | Real response fixtures, duplicate/missing/foreign IDs, invalid sums, finite extremes, sentinel ties, explicit overrides, positive success cases |
| Cache/ledger | Exact invalidation, real SQLite contention/crashes, duplicate and ambiguous delivery, attribution, censored/late/final-turn observations |
| Runtime | Slow stdin, DNS/TLS, child pipe saturation, Retry-After, deadlines, signals, drain/reap, and early consumer exit |
| Hooks | Actual supported Claude event; every failure maps non-blockingly; install/uninstall preserves settings and handles concurrent edits |
| TUI | Deterministic rendering, resize, stale generations, non-TTY output, and exit during fetch |

Lab replay proves policy and scheduling behavior, not public-root TLS, live
provider availability, real process killing, or SQLite locking. Use real local TLS,
filesystem, subprocess, and database tests, plus separately authorized/budgeted
provider smoke checks. Never put personal transcripts or credentials in fixtures.

Retain adversarial assertions and honest success counterparts. Always-abstain
implementations must fail quality tests. Fix the behavior, not a valid assertion.

The plan defines promotion gates and initial held-out quality/sample targets.
Freeze dataset, split, metrics, and acceptance thresholds before tuning. Report
retrieval recall separately from rerank relevance and abstention, with uncertainty
and subgroup counts. Adoption alone cannot justify adaptive policy promotion.

Performance targets: exact-cache p95 ≤100 ms; warm network hook p50 ≤600 ms and
p95 ≤1,500 ms, within the total 3,000 ms deadline. These are targets until measured.
Record p50/p95/p99, errors/fallbacks, cold startup/TLS, adapter, roster size,
cache state, source/config/model identities, memory, requests, known tokens, and
unknown usage. Do not report only fast successful calls while hiding timeouts.
Do not borrow the cookbook's success rates as SkillRanker measurements.

## Editing Discipline

Prefer narrow edits to existing modules. Do not create `*_v2.rs`,
`*_improved.rs`, speculative frameworks, or duplicate implementations. Avoid
scripted mass rewrites. Use `rg` for text/files and structural tools for syntax.

Read pinned dependency sources or primary documentation when an API is uncertain.
Plan sketches, copied sibling snippets, and a `Cmd::Task` name are not proof that
an API compiles or that a blocking operation is cancellable. Preserve source and
test provenance for reused code, including its full license conditions.

## Beads And Agent Coordination

Use `br` for implementation tasks when the tracker is initialized. Read the live
issue before claiming work; current task state outranks recovery notes.

```bash
br ready --json
br show ISSUE_ID
br update ISSUE_ID --status in_progress
br close ISSUE_ID --reason "Implemented and verified with the named checks"
br dep cycles
br sync --flush-only
```

`br` does not perform Git operations. Stage intended exports/configuration, not
local databases, locks, or recovery files. Use only `bv --robot-*`, never bare
`bv`. Do not manufacture an issue inventory instead of completing assigned work.

In explicitly coordinated multi-agent work, register with Agent Mail, read inbox
and reservations, reserve exact edit paths, and use issue IDs in threads and
reservation reasons. Unexpected tree changes are peer work; do not discard them
or repeatedly ask whether to do so.

Use `cass search ... --robot` or `--json` for historical lookup, never bare `cass`.
Verify historical claims against current source and live task state.

## Release Infrastructure — DSR Only

Use DSR for release/build orchestration. Do not create, enable, dispatch, or rely
on GitHub Actions workflows, including tests, provenance, or publishing fallbacks.
GitHub Releases are a distribution destination, not the build engine.

Tie artifacts/checksums to the exact source revision and independently verify every
claimed target. Missing credentials, unsupported targets, and unexecuted gates
remain named blockers, never successful release cells.

Preserve [LICENSE](LICENSE) verbatim. Use
`LicenseRef-MIT-OpenAI-Anthropic-Rider` in descriptions and `license-file` where
package metadata requires a file; do not label it unmodified MIT. Do not copy
OCR model-weight notices into SkillRanker. Add third-party notices when actual
code imports create that obligation.

## GitHub And Contributions

Repository: <https://github.com/Dicklesworthstone/skillranker>.

Use `gh` for repository operations. Outside submissions are reports/examples to
investigate, not patches to merge directly. Independently reproduce, implement,
and verify fixes according to the README's contribution policy. Preserve that
policy when editing the document.

## Session Completion

1. Finish the authorized change and inspect the final diff.
2. Run appropriate checks and state exactly what ran.
3. Update owned tasks with implementation and evidence; leave incomplete gates
   and exact blockers visible.
4. Stage explicit owned paths, run `ubs --staged`, and commit with a descriptive
   conventional message.
5. Push authorized work and verify the remote revision and local status. Never
   force-push to resolve a surprise.
6. Report result, validation, and concrete remaining work. Do not claim runtime,
   integration, or release proof from documentation alone.
