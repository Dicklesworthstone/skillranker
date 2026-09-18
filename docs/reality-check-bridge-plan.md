# SkillRanker reality check and bridge plan

Latest assessment: 2026-09-18. Earlier 2026-09-17 reviews and receipts are retained
below as history. Inventory and ownership statements describe their stated
snapshots, not a frozen release or a product-completion percentage.

## Current assessment — 2026-09-18, evening source review

**The local inspection CLI works, and substantial ranking components now exist,
but there is still no executable next-step recommendation workflow.** The most
valuable implementation step is assembling the existing components into `sr rank`,
after repairing the concrete prerequisite defects below. More feature design is
not the bottleneck.

This review reread the complete AGENTS.md, README.md and comprehensive plan. It
checked command dispatch, component implementations, tests, phase claims, live
Beads and GitHub Releases. The frozen CLI verification base is `8865520`;
concurrent changes, including the later P3 catalog acceptance and cache namespace
repair, are distinguished from that base. This is a comprehensive workflow and
gap assessment, not an exhaustive security review of every source line.

### Current capability map

| Promised workflow | Source reality | Remaining acceptance owners |
|---|---|---|
| Local configuration and readiness | `doctor --config` and general `doctor` are implemented, including key-presence versus authentication and independent network consent. | `.5.17` delivered; actual ranking consumes its prerequisites through `.5.11` |
| Roster inspection and drift | `roster` listing, pagination, snapshot and diff dispatch exist. Unverified harness visibility remains explicitly unverified. | `.3.17`; active `sr-fzia` repair/proof |
| Exact context and privacy | Normalized input, branch resolution, overlays, redaction, rendering and disclosure components exist. Native block parsing and authorized snapshot opening remain defective. | Reopened `.4.3` and `.4.14` |
| Jev requests and scoring | Asupersync transport, codec, retries, wide/rerank builders, eligibility and finite scoring are implemented library components. | Reopened `.2.10`/`.2.12`; integration `.5.11` |
| Actual recommendation | `src/cli.rs::command` registers only doctor and roster. Bare `sr`, rank, demo, capabilities and hook have no implemented dispatch. | `.5.11`–`.5.16`, `.5.18`–`.5.22` |
| Exact response reuse | Fingerprints, in-memory response cache and SQLite lease primitives exist. Shared response-body delivery does not. | Reopened `.5.10`, `.5.20`; active `sr-yac4` |
| Ledger, observations and feedback | Private SQLite/storage foundations exist; the observation-ledger schema, attribution and user commands remain open. | `.6.1`–`.6.13`, `.6.26`/`.6.29` |
| Capture, replay and evaluation | Contracts and synthetic policy oracles exist; no reachable capture/replay/eval workflow or measured relevance cohort. | `.6.14`–`.6.25`, `.6.27`–`.6.29` |
| Claude hooks and controls | No dedicated quiet hook, managed installer, shared allowance, breaker or snooze command is reachable. Do not install this binary as the proposed hook. | `.7.1`–`.7.13` |
| Demonstrated usefulness and performance | No accepted relevance, controlled-harm or representative-hook cohort was established by this review. Latency numbers remain targets. | `.8.1`–`.8.10` |
| Learning and optional interfaces | Calibration, monitoring, priors, TUI and later retrieval/adapter experiments remain future implementation. `tui = []` adds no viewer. | P8/P9, with individual gates |
| Distribution and platforms | GitHub Releases returned an empty list. Linux builds can run through RCH; storage remains Linux-gated. No macOS behavioral or release-artifact qualification established. | `.5.23`, `.8.7` |

The earlier section below is now historical: its claims that wide/rerank builders,
general doctor, roster dispatch, fingerprints and response-cache components were
absent no longer describe current source. Conversely, implementing those pieces
has not made `rank` reachable. The public README retains its requested product
voice; this document records delivery status separately.

### Three concrete defects behind premature acceptance

1. **An ordinary smoke test can spend API credits without opt-in.**
   `tests/jev_smoke.rs::budgeted_live_contract_smoke` is not ignored, defaults
   `SKILLRANKER_LIVE_CONSENT` to true and calls a helper that reads either the
   environment or the checkout's `.env`. With no key, the same test instead
   verifies refusal and passes. Therefore a green test does not establish a live
   exchange, while an ambient key can unexpectedly cause one. Reopened `.2.10`,
   its acceptance gate `.2.12` and P1. Require consent before credential lookup,
   explicit opt-in live execution, separate deterministic refusal tests and a
   revision-bound live receipt. No live request or credential read was made by
   this assessment. The recorded September 17 request/response hashes match their
   provenance file, but that file explicitly disclaims Asupersync qualification;
   it cannot certify this later transport build. The newer spike document's
   response/latency assertion lacks a source-bound execution receipt.

2. **Cross-process leases do not deliver cross-process responses.**
   `SingleFlightCoordinator::coordinate_request` takes `MemoryResponseCache`;
   SQLite contains lease metadata only. The competing-process test accepts
   `FollowerResolution::LeaderFailed` and checks its child's exit code, without
   checking a delivered response body. Wrapper lookups hard-code the wide stage,
   completion becomes visible before `cache.put`, and completed lease rows are
   returned before expiry is considered. A lost/expired body cannot trigger a
   fresh owner through that path. SQLite opening also bypasses the private-store
   qualification boundary. Reopened `.5.10` and attached persistent-cache proof
   requirements to `.5.20`; the separate namespace repair retains its owner.
   Acceptance needs real processes receiving the same validated wide and rerank
   responses, one owner, fresh reacquisition, publication-race tests, protected
   bounded storage and truthful unknown-usage accounting.

3. **Native context acceptance exceeds the parser's behavior.**
   `context::jsonl::native_text` accepts string content only. An array of text or
   nested tool blocks becomes empty text while parsing still succeeds; native
   tool arguments/results/status are not recovered by that path. Separately,
   `read_snapshot` checks metadata and then opens the path without descriptor-bound
   authority or matching the checked identity. A replacement can occur between
   those operations. Reopened `.4.3`, `.4.14` and P3. Preserve the recent identity,
   overlay and corrupt-record-budget fixes; add real supported block-shape cases
   and authorized-open race tests. The new P3 catalog at `5c36509` is an improvement
   over fixture-interpreter evidence: it runs actual Rust integration targets.
   Its reported 77 passes do not establish these missing behaviors or installed
   harness compatibility. Do not discard valid narrower evidence when reopening
   the broader gate.

These are source-established defects and proof gaps, not newly executed race
reproducers. Their repairs are outstanding. Initial Agent Mail reads succeeded,
but both attempted coordination messages timed out; durable Beads comments
248–254, 256 and 257 carry the findings and handoffs instead.

### Bridge, coverage and refinement

The initial snapshot contained 201 issues: 78 closed, 110 open, 6 in progress,
6 deferred and 1 blocked. Those are tracker counts, not delivery percentages.
The existing seventy-goal and I01–I15 crosswalks below still cover the scoped
vision. No duplicate feature tasks were added. Reopening original boundaries
keeps their unfinished requirements attached to the feature that promises them.
An explicit `.4.14` → `.4.3` blocking edge was added: a closed intermediate
conformance node otherwise left the phase gate ready despite the reopened parser.

Implement in this order:

1. Repair explicit live-test consent and qualify the selected transport build;
   finish the active roster/cache repairs and native input corrections.
2. Assemble one normalized-input `sr rank` path through `.5.11`, using the existing
   EffectGate, configuration refresh, builders, eligibility and scoring. Prove
   useful, explicit, abstain and unavailable outcomes through the actual binary,
   with request counts and current-state revalidation. Keep optional history
   absent on the first successful run.
3. Complete dry-run, four demos, bounded output, explanations and truthful
   capabilities. Prove exact cache reuse across processes, expiry and refusal
   behavior before core acceptance `.5.21`.
4. Add minimal durable observation/replay/evaluation, then a recorded shadow hook
   and actual supported-harness proof. Provider success alone cannot certify
   this integration.
5. Collect the predeclared relevance, paired-harm and representative operational
   cohorts; publish only DSR targets actually qualified. Preserve Linux delivery
   independently of the separate macOS implementation/proof requirement.
6. Retain calibration, monitoring, TUI and all later experiments behind their
   existing gates. They must not delay the first usable core CLI.

The deeper planning passes tested three assumptions: whether existing components
can form the first useful journey, whether positive acceptance evidence actually
crosses the claimed process/harness boundary, and whether external evidence can
be replaced by implementation. The resulting changes are connection-first
ordering, reopening the three defective boundaries, and retaining real provider,
harness, cohort and platform evidence as deliverables. Refinement then checked
coverage, ownership, dependencies, positive/failure proof and final claim limits.
No additional speculative feature or process system was justified.

Completing the existing roadmap **with its original acceptance evidence** would
cover the declared vision. Closing its implementation rows alone would not.
No measured quality, latency, live provider readiness or release claim follows
from the number of passing library tests.

### Fresh verification

Documentation consistency passed; the matrix validator accepted 79 declarations;
evaluation-policy validation accepted 12 synthetic cases; the real context case
catalog validated. These are consistency checks, not execution of every case.
The dependency graph had zero active cycles after the reopenings.

Required-remote verification on `ovh-b` passed **24 tests, zero failures, zero
ignored**: bootstrap CLI (3), configuration CLI (8), doctor readiness (8) and
roster CLI (5). Command:

```bash
RCH_REQUIRE_REMOTE=1 RCH_WORKER=ovh-b CARGO_BUILD_JOBS=1 \
rch --json exec --base 8865520 --clean-overlay --overlay-path src/main.rs -- \
env CARGO_HOME=/data/projects/skillranker/.rch-tmp/rch-cargo-cache-ovh-b \
cargo +nightly-2026-08-31 test --offline --locked -j 1 \
  --test bootstrap_cli --test cli_config --test doctor_readiness --test roster_cli
```

The unchanged main-file overlay anchored base
`886552032cb7addb6d58a044feba6b13daf1514a`; RCH receipt fingerprint:
`0f007f5062fad4cee59bb9cae9ad320e1dfce84e47703844e5f8bc3fef70e173`.
Build/test log: `/data/tmp/sr-reality-cli-20260918.log`.

Thirteen direct invocations of that compiled binary in a new isolated home and
synthetic skill tree confirmed: help/version, doctor, doctor-config and roster
exit zero; bare invocation, rank, demo, capabilities, hook, replay, eval and tui
exit 2. Unsupported hook invocation writes ordinary error JSON, not a quiet hook
envelope. No data/cache state directory appeared. The binary SHA-256 is
`8e3e8289975cd165b49d6b3ead53ab8fab9cc9f73b1257be5e239099ee781e55`;
full command/output evidence is
`/data/tmp/sr-reality-cli-invocations-20260918.json` (retained remote original:
`ovh-b:/tmp/sr-reality-cli-f_97mbu9/report.json`).

This proves we can build and test the current Linux inspection subset through
this machine's remote fleet. It does not establish a complete rank journey,
macOS support, a full-suite current-tree pass, live Jev readiness, performance
targets or a release. No live-provider test was selected. Peer builds and source
edits continue and are not covered by this run's frozen revision.

## Earlier assessment — 2026-09-18, before the evening review

**SkillRanker has substantial tested foundations, but it does not yet deliver its
central user promise: recommending skills for a session.** The executable now
supports help, version and local `doctor --config`; the previous assessment's
help/version-only description is obsolete. There is still no `rank` dispatch.
Do not install this binary as the planned quiet hook: unsupported hook arguments
currently return a usage error, not the future non-blocking hook protocol.

This assessment read all 671 lines of AGENTS.md and all 1,971 lines of README.md,
then reconciled the comprehensive plan, existing engineering contracts, source
entry points, tests, live tracker and distribution state. Initial source anchor:
`dee4aac` on `main`. Concurrent roster work continued during review; uncommitted
retrieval/explicit-resolution changes are work in progress, not accepted product
capabilities. This is a workflow and acceptance audit, not a claim of exhaustive
line-by-line security verification of every implementation.

### What users can actually do

| Workflow | Current reality | Decisive source / remaining owners |
|---|---|---|
| Inspect configuration | **Working narrow CLI:** `doctor --config`, JSON/table, trusted precedence, sanitized errors. General readiness is still missing. | `src/main.rs` → `src/cli.rs::run` → `execute`; `tests/cli_config.rs`; `.5.1` and `.5.17` remain broader work |
| Select and normalize session input | **Implemented library components:** exact source selection, bounded normalized envelopes/native JSONL, branch resolution, Claude prompt overlay, tool/load associations and capability-checked cass. **Not a rank journey.** | `src/context/`; `.4.8`/`.4.9` context rendering and anchors, `.4.11`/`.4.12` disclosure, `.4.13`/`.4.14` conformance/acceptance |
| Discover/select skills | **Implemented library components:** authorized reads, metadata, harness-visible roots, identity/aliases/restrictions, bounded literal Quill query compiler. Deterministic roster retrieval and explicit resolution are actively being implemented. | `src/authorized_read.rs`, `src/roster/`; `.3.6`, `.3.7`, `.3.10`–`.3.17`; no roster CLI dispatch yet |
| Protect disclosures | Bounded redaction and authority/effect contracts exist. Complete request-field redaction/disclosure wiring remains open. | `src/privacy/`; `.3.2`, `.4.11`, `.4.12`, `.5.9`, `.5.15` |
| Call Jev | **Real library transport:** selected Asupersync HTTPS, typed request/reply validation, attempt accounting and deadline/budget-bounded retries. Local TLS tests are real; live TypeSafe interoperability is separately unproven. | `src/jev/{endpoint,codec,admission,client,retry}.rs`; `.2.10`–`.2.12` |
| Recommend a next skill | **Not implemented as a product path.** No connected wide/rerank builders, gate policy, eligible-candidate scoring and final publication through the CLI. Output validation alone does not select anything. | `.5.2`–`.5.5`, `.5.11`–`.5.15`, `.5.19`/`.5.21` |
| Reuse responses safely | **Linux storage foundation only:** real rusqlite, actual engine identity checks, private files, metadata/incarnation/generation and atomic exports. No stage-response cache, keyed namespace or single-flight integration yet. | `src/storage/`; `.5.7`–`.5.10`, `.5.20`; schema currently stores foundation metadata |
| Capture, replay, observe, judge, report | Schemas/policy fixtures exist; no reachable ledger/replay/feedback/evaluator commands. | P5 `.6.1`–`.6.29` |
| Install a Claude hook / control spending | Adapter contracts exist; dedicated hook protocol, installer, durable allowance, shared cooldown and snooze integration do not. Invocation retry accounting is not a cross-process allowance. | P6 `.7.1`–`.7.13` |
| Demonstrate benefit or low latency | **Unproven.** Synthetic arithmetic and library TLS tests do not establish recommendation quality, harm risk, production fallback or end-to-end latency. | P7 `.8.1`–`.8.10`; measured cohorts and operational trials required |
| Learn, monitor, inspect interactively | Policy design exists; calibration/priors/monitoring/TUI and later experiments are not delivered. `tui = []` remains an empty reserved Cargo feature. | P8/P9, individually gated |
| Install a released Linux/macOS product | GitHub release query returned no releases. Storage/export module is Linux-only. No verified dual-platform artifact is established. | `.8.7`, new `.5.23`; DSR remains the sole release orchestrator |

Quill is genuinely integrated through the shipping narrow crate, with native
expected-result fixtures and a dependency denylist; this is not a Tantivy wrapper.
Asupersync is the active runtime/transport. There is no requirement to install
`ms`; no numerical-backend implementation is claimed from the availability of
FrankenNumPy/SciPy/Pandas. Their narrow evaluation qualification remains `.6.18`.
`src/transport.rs` is still outside the crate module graph; the active Jev client
uses `src/jev/endpoint.rs`. An uncompiled alternate file is not a capability.

### Concrete gaps that the prior closure picture missed

1. **Stalled stdin is not bounded by the advertised helper.**
   `read_stdin_before_cleanup` checks the clock before calling generic blocking
   `Read::read`. It cannot interrupt that read and returns success on EOF without
   checking completion time. It also rejects exact-cap data before probing EOF.
   The current slow-input test sleeps before entering an immediate `Cursor`, so
   it cannot detect an in-flight stall. New **`sr-roadmap-l1i.2.13`** owns the
   repair and real pipe/late-EOF/signal/cleanup tests; it blocks `.2.11` and `.7.1`.
   This is source-established, not a newly executed reproducer or completed fix.
   Historical `.2.1` proof remains intact but is not whole-boundary acceptance.
2. **macOS persistence/export is absent, not merely untested packaging.**
   `src/lib.rs` hides the complete storage module outside Linux, including the
   shared atomic exporter; `tests/storage_contract.rs` is Linux-only. New
   **`sr-roadmap-l1i.5.23`** owns platform implementation and native behavioral
   qualification after `.5.6`/`.3.14`; dual-platform release `.8.7` depends on it.
   Linux development remains independently useful. Do not qualify macOS from
   successful cross-compilation, skipped tests or a nominal portable fallback
   inside an unavailable module.
3. **Engineering status had become stale.** The prior assessment said no actual
   HTTP client, no rusqlite dependency and only help/version. Those are now false.
   The comprehensive plan also claimed no local tracker and required a
   documentation-only README banner. Both plan contradictions are corrected;
   the requested finished-product README voice is preserved.
4. **Product E2E remains future work.** The checked-in runner suite directory
   contains runner mechanics suites, not executable rank/cache/hook/learning
   suites. Planned matrix rows are obligations, not evidence that those suites
   exist or passed. The two corrective boundaries are explicitly planned rows
   in both coverage authority and matrix, with positive and failure cases.

### Does the backlog close the vision?

The initial live snapshot contained **196 issues: 48 closed, 135 open,
7 in progress and 6 deferred**. Counts are not a delivery percentage. The
existing 17 workflow groups, 70 goal rows and I01–I15 crosswalk below still cover
the intended feature set. This pass revalidated current source and ownership
against those mappings; it does not reissue the older 195-record per-body audit
as a fresh exhaustive review of every current comment.

**Implementing and genuinely accepting the roadmap, including these two repair
obligations, covers the scoped vision. Merely closing its coding tasks does not.**
Live provider interoperability, installed-harness conformance, independent
consented labels, controlled outcome trials, supported-platform tests and DSR
artifacts are deliverables with external evidence requirements. They cannot be
manufactured by more code or synthetic fixtures. Deferred native adapters retain
individual qualification/disposition gates; normalized input is not native
support. No additional missing product feature was established in this pass.

The new tasks fill a closed-boundary repair gap and a platform implementation
gap; they do not replace the existing core, hook, evaluation or experiment tasks.
They contain self-contained causes, constraints, proof requirements and success
counterparts. All tracker mutations were performed with `br`.

### Bridge to useful delivery, in logical order

1. **Finish already-owned foundation leaves and acceptance.** Preserve active
   explicit resolution/retrieval work. Complete disclosure/context rendering,
   roster revalidation, stdin repair and transport failure proof. `.2.10` is an
   explicitly consented live check, not permission to use ambient credentials.
2. **Deliver one real normalized-input CLI journey through `.5.21`.** Implement
   builders → wide gate → rerank → per-candidate eligibility/none comparison →
   finite scoring → final current-state validation → bounded output. Prove
   useful, explicit, genuine abstention and unavailable outcomes. Show exact
   request counts and paid-attempt accounting, not only JSON shape. Offline
   demo/dry-run must use the same production normalization and decision paths.
3. **Prove exact caching and failure boundaries before calling the core done.**
   Test current-authority revocation, source changes during requests, cache
   isolation, owner loss, no-persist/offline semantics and stale publication.
   Keep standalone ranking independent of the optional observation ledger.
4. **Add faithful evidence and recorded shadow hooks.** P5 and P6 preserve
   independent initialization/consent, exact-session attribution, durable
   allowance and non-blocking output. Actual supported harness evidence remains
   mandatory; parsing an event fixture is insufficient.
5. **Measure before advisory promotion and release.** Freeze independent
   relevance/harm/operational designs, include failed/unstarted cases, and meet
   the declared gates. macOS storage qualification can proceed in parallel and
   blocks only claims that require it. Publish only verified DSR target cells.
6. **Retain all later ambition behind its own gate.** Learning, statistical
   monitoring, TUI, additional adapters, passage/query-view experiments and
   description diagnostics remain in P8/P9. None is silently dropped, and none
   should substitute for completing the first useful selector.

The initial `bv --robot-triage` ranked core acceptance `.5.21` as a central
bottleneck and recommended ready transport work. Its summary reported zero
blocked while individual recommendations listed blockers; use live `br`
dependencies for claimability, not that aggregate field. No fabricated ETA is
inferred from graph centrality or default task estimates.

### Ambition and refinement in this pass

Three ambition passes improved the existing bridge rather than adding speculative
features: (1) measure progress by a connected user journey instead of library or
bead counts; (2) require real fault witnesses and completion-time evidence at the
stdin boundary; (3) separate portable implementation and native proof from a
release packaging checkbox, while preserving all statistical and P9 ambitions.

Five refinement passes followed: (1) reconcile current entry points and replace
stale capability claims; (2) inspect closed runtime/storage boundaries and create
only the two concrete corrective tasks; (3) overlay dependencies without making
macOS a prerequisite for Linux core development; (4) add explicit planned matrix
rows, success/failure cases and receipt limits, preserving existing rows and
historical evidence; (5) rerun declaration and graph checks and review the final
diff for unsupported claims. No further feature-level gap was found in that final
review. This records actual dispositions, not five new product certifications.

### Verification and evidence limits

Audit artifacts are retained under
`/data/tmp/sr-reality-20260918-m6_9ak1p`:

- Fresh required-remote Cargo run on `hz3`, job `30024414133224260`:
  `cargo test --locked --test bootstrap_cli --test cli_config --test runtime_contract -j 2`;
  **19 passed, zero failed or ignored** (3 bootstrap, 8 config, 8 runtime).
  The CLI tests execute the real compiled binary. This is selected-suite
  evidence from the transferred shared-tree snapshot, not a frozen whole-tree
  release certificate or proof of the missing stalled-read cases.
- Python discovery: **72 passed**. After coverage edits, the independent matrix
  suite in a detached frozen worktree passed **21/21**, including actual runner
  receipt checks. The preceding moving-tree run failed one receipt-identity
  assertion while peer changes occurred; its log is retained and is not a pass.
  Freezing source resolved that failure without changing code or assertions.
- Public-document consistency and **79 coverage declarations** passed. These
  validate links/contracts and planned obligations, not runtime feature support.
- `br dep cycles --json`: **zero active cycles**. Export inspection found
  **419 blocking edges, 191 parent-child edges, zero dangling endpoints**, and
  no non-P9 task transitively blocked by a deferred P9 adapter. Final observed
  inventory: **198 issues, 48 closed, 136 open, 8 in progress, 6 deferred**;
  `.2.13` had already moved to in progress during review.
- `bv --robot-triage`, `bv --robot-insights` and `git diff --check` completed.
- `ubs --staged` exited 3 because this documentation/TOML/tracker change set
  contains no supported scanner language. No scanner ran; this is not a UBS pass.

The earlier complete Rust test/check/
Clippy evidence in `/data/tmp/skillranker-retry-hwu6py3m/final-evidence.json`
belongs to its recorded source snapshot; it is not a fresh whole-tree receipt.
The two discovered repairs remain open. No live Jev request, private transcript
read, credential read, installed-hook change, quality trial or release occurred.

Agent Mail initially supplied current peer ownership; later reservation/message
calls failed at HTTP transport. Failed delivery is not claimed as coordination.
Exact owned audit paths and findings were recorded in a root Beads comment.
After recovery, leases 10817–10820 were granted without conflicts and message
41843 delivered the findings and scope to both active roster owners.
Active roster edits were preserved. The historical sections below retain dated
findings, including superseded failures, and are not current capability claims.

## Historical follow-up assessment (2026-09-17)

This follow-up reread all 671 lines of AGENTS.md and all 1,954 lines of README.md,
the comprehensive plan and engineering contracts, then checked live source,
tracker coverage, dependency graphs and executable verification. The historical
sections below retain their original evidence scope; their old task counts and
status observations are not the current inventory.

**This machine is equipped to develop and verify SkillRanker. The product still
cannot rank a session.** `src/main.rs` captures an entry clock and accepts only
help/version. Bare invocation, rank, demo, doctor, capabilities, hook and TUI
have no implemented dispatch. In particular, the bootstrap's exit 2 for hook
arguments must not be installed as the promised quiet hook.

The inspected host is Linux x86-64 `threadripperje`, with 128 logical CPUs,
approximately 420 GiB available memory and 3.31 TiB free on the project filesystem.
The pinned `nightly-2026-08-31` toolchain, Cargo, rustfmt, Clippy, GCC and linker
are installed. RCH is available; its fleet has usable capacity despite several
pressure-blocked workers. Repository policy requires remote compilation here.
Local toolchain availability is not a claim that this follow-up compiled locally.
The ignored maintainer `.env` exists with mode 0600; its contents were not read,
and credential presence does not establish authentication or network consent.

### Source reality and remaining delivery

| Boundary | Current implementation | Missing product behavior / existing owners |
|---|---|---|
| Public binary | Real help/version process and argument rejection | Shared command shell `.3.18`, strict CLI `.5.1`, pipeline `.5.11`, executable proof `.5.19`, core acceptance `.5.21` |
| Identity/configuration/output | Real typed identities, trust precedence, resource checks, adapter gates and bounded JSON validation | Actual configuration reads, filesystem authority, command wiring and publication |
| Session capture | Exact source-selection library with tests | Normalized reader `.4.2`, native snapshots/branches/prompt overlay `.4.3`–`.4.5`, cass `.4.6`, rendering/profiles/conformance `.4.7`–`.4.14` |
| Roster and privacy | Skill record types, bounded redactor, shipping Quill API fixtures and in-progress literal-query compiler | Authorized reads, metadata/discovery/precedence, explicit resolution, actual snapshot retrieval and revalidation `.3.1`, `.3.3`–`.3.7`, `.3.10`–`.3.17` |
| Jev | Origin/credential rules, typed wire codecs and invocation attempt receipts | Actual HTTP client/DNS/TLS `.2.7`, retry loop `.2.9`, consented live interoperability `.2.10`, transport acceptance `.2.11`–`.2.12` |
| Ranking | Output schema can validate supplied ranked documents | Wide/rerank question builders, eligibility and scoring `.5.2`–`.5.5`; no connected two-pass selector |
| Runtime | Entry clock, Asupersync ownership and in-progress blocking/subprocess helpers | Actual stalled-read/signal/cleanup proof; see the concrete stdin defect below |
| Cache and persistence | Contracts and limits | No cache or ledger implementation, no rusqlite dependency; `.5.6`–`.5.10`, `.5.20`, P5 |
| Hooks/user controls | Input/advice contracts only | Dedicated quiet hook, installer, durable allowance, breaker and snoozes: P6 |
| Evaluation/learning | Synthetic policy fixtures and real validator/runner tests | Product evaluator, independent cohorts, measured relevance/harm/latency, calibration and monitoring: P5/P7/P8 |
| TUI/distribution | Empty reserved `tui` feature; no GitHub Releases returned | Actual TUI `.10.7`/`.10.24`, independently verified Linux/macOS DSR artifacts `.8.7` |

`src/transport.rs` is a tracked, unexported alternate origin implementation.
Neither crate root includes it. Its presence is not HTTP transport or compiled
test evidence; the active origin implementation is `src/jev/endpoint.rs`.
The transport owner should reconcile this unused duplicate before wiring the
HTTP boundary rather than accidentally adopting its older contract.

### Concrete runtime finding

At the frozen snapshot, `read_stdin_before_cleanup` calls generic synchronous
`Read::read` after a deadline check. That call itself can stall past the deadline;
`Ok(0)` also returns success without checking its completion time. The existing
"slow stdin" test sleeps before entering the helper and then uses an immediate
Cursor, so it does not establish cancellation of an actual stalled read.
The helper also rejects a document exactly equal to `max_bytes` before checking
whether the next read would be EOF. This is a source-established defect, not a
claim that a reproducer or fix has passed. It was sent to runtime owner SilentIvy
in Agent Mail message 41615. Existing `.2.1`/`.2.2` and integration `.2.11` own
the required correction and real pipe/deadline/signal tests. Two attempts to add
a Beads comment timed out on the workspace write lock; the owner was notified of
that tracking limitation in message 41619. No new feature is
needed. Do not treat the closed runtime foundation as proof of bounded stdin.

### Immediate source and publication blockers

The frozen full-test run reproduced two query AST-validator failures in
`src/roster/retrieval.rs`: honest Quill field expansion was rejected as changed
meaning. Existing owner SilentFinch is repairing `.3.9`; its newer proof must
finish before this result can be superseded. Compilation alone missed this bug.

A peer also reported GitHub push protection rejecting ancestor `adf240a` because
`tests/redaction_contract.rs:61` matched a Slack-token detector (message 41623).
This report is a publication blocker, not evidence that a real credential was
exposed. Resolve the fixture/history through an authorized path; do not bypass
protection or rewrite shared history as part of this assessment.

### Bridge priority and coverage verdict

The 17 workflow groups and 70 concrete obligations below still cover the vision.
The observed inventory has 195 records: 26 closed, 8 in progress, 155 open and
6 deferred. These are tracker counts, not a product-completion percentage.
All I01–I15 improvements retain existing implementation and verification owners.
The current review found no additional missing product owner; it found unfinished
integration and a defect inside an already-owned runtime boundary.

The next useful milestone is the existing P4 core acceptance, not more planning:
finish safe input/roster and transport prerequisites; connect one normalized-input
journey through redaction, explicit resolution or Jev, eligibility and output;
prove ranked, explicit, abstain and unavailable via the actual executable;
then add the already specified cache, diagnostics and complete core gate.
Keep optional ledger history out of the first stateless recommendation, while
preserving cache and the other P4 acceptance obligations. Proceed to real shadow
hooks only after their own deadline, failure, visibility and installation tests.

Three improvement passes sharpened that sequence: prioritize a complete user
journey, require real effect-boundary tests instead of type/fixture substitution,
and keep empirical quality separate from software completion. Five refinement
passes checked vision coverage, prerequisite ordering, adversarial/success proof,
onboarding and documentation authority, then residual ownership. They retained
the existing roadmap; the new stdin finding goes to its existing owner. No task
was duplicated or closed merely because its supporting types or tests exist.

Completing every existing task **to its actual acceptance criteria** would cover
the scoped vision. Merely writing the code or marking tasks closed would not:
live Jev compatibility, real supported-harness behavior, independent judgments,
controlled harm outcomes, operational measurements and target-specific release
proof must actually be obtained. The current synthetic corpus supplies none of
those empirical results. BV triage identifies core acceptance `.5.21` as a central
bottleneck, but it is blocked by its six named prerequisites; both BV insights and
`br dep cycles` reported no active cycles.

### Verification in this follow-up

The frozen build snapshot is based on `adf240a79ed461a0f5376c68c2ba2b974e833c25`
plus explicitly captured current source/test/manifest overlays, including active
peer work. Its 106-file SHA-256 manifest and command logs are retained under
`/data/tmp/skillranker-reality-b2aqhma_`. It is not a clean release revision.

- Local Python tests: 72 checker tests and 42 process/evidence-runner tests passed.
- Public documentation consistency and 77 matrix declarations passed. Neither
  validator executes the README's commands or establishes product acceptance.
- Frozen formatting and all four declared dependency feature graphs passed;
  no prohibited engine/runtime was found. An empty TUI feature is still empty.
- Remote `cargo check --locked --all-targets -j 2` passed on `vmi1264463`
  (job `j-30024414133223896`, exit 0, 605 seconds including transfer). RCH
  reported overlay fingerprint
  `b5b4be96023bcd52a37085cc51e6eaefe8396d1eddca6e0253817c9b65a0e9af`.
- Remote `cargo test --locked -j 2` compiled the test profile, then exited 101
  on `vmi1264463` (job `j-30024414133223906`, 731 seconds). The library target
  ran three tests: one passed and two failed. Both
  `parser_recovery_and_truncation_are_failures_with_text_free_diagnostics` and
  `syntactically_valid_but_changed_meaning_is_refused` expected `Ok(())` but
  received `Err(MeaningChanged)`. Cargo stopped there; remaining test targets
  were not executed by this run. This is a source failure, not an RCH failure.
  The owner independently identified Quill's per-term content/title OR expansion
  and reported a correction under fresh remote verification (message 41621).
  That newer correction is outside this frozen snapshot and is not certified here.
- No live Jev calls, private transcript reads, installed-hook mutations, quality
  experiments or releases were performed.

## Earlier assessment

The checkout did not deliver a session-specific skill recommendation. The inspected `src/main.rs` accepted only help/version, rejected all other inputs with exit 2, and called no library pipeline. README now intentionally uses the user's requested finished-product voice; current availability belongs in source, capabilities, engineering evidence and Beads.

The project has real foundation code and tests, an extensive design, and a local task tracker. None of these establishes end-to-end ranking, live Jev transport, hook safety, measured usefulness, or release readiness. The working tree contains concurrent uncommitted additions; findings describe a moving checkout, not a release revision.
The initial audit over-attributed the contract-matrix repairs to peers: the repaired rows are the original author's own corrections committed in `f13fbc2`; the reference validator patch and regression suite were this audit's fixes, reviewed and executed here.
## Observed evidence

- `src/main.rs`: only `--help`/`-h` and `--version`/`-V` dispatch; unknown arguments are not echoed.
- `Cargo.toml`: Rust 2024 package and pinned Asupersync/Quill sources; `tui = []`; no CLI parser or SQLite dependency yet. Dependency presence is not integration proof.
- `docs/verification-foundation-audit.md`: historical source-bound remote tests cover 32 foundation tests. It explicitly excludes concurrent output work and makes no live provider claim. Current-run evidence is reported separately below.
- `gh release list --repo Dicklesworthstone/skillranker --limit 100`: successful command, no releases returned. This does not rule out unpublished or externally distributed builds.
- `rch status`: remote workers available, with several pressure-blocked workers. This establishes infrastructure availability, not a build pass.
- `.beads` exists locally. The comprehensive plan's statement that no project-local tracker exists is stale and must be corrected without changing shared parent trackers.

## Vision checklist and bridge sequence

Statuses below apply to usable product workflows. Partial library implementations are mapped separately; NOT DELIVERED does not mean every supporting function is absent. The full 70-obligation checklist and existing bead ownership refine these seventeen summary goals without creating a competing roadmap.

| Goal | Current status | Required user-visible result | Delivery boundary |
|---|---|---|---|
| V01 Reproducible standalone foundation | PARTIAL | Clean source build, honest feature inventory, pinned acceptable dependency graph | P0 |
| V02 Exact session and bounded context | NOT DELIVERED | Normalized/native selected session, prompt overlay and source attribution without cross-session fallback | P3 |
| V03 Privacy and trusted configuration | NOT DELIVERED | Explicit network opt-in, redaction before truncation, bounded reads, separate persistence controls | P0/P2/P3/P4 |
| V04 Visible roster and explicit authority | NOT DELIVERED | Only loadable targets, correct precedence/restrictions, explicit resolution before ranking | P2 |
| V05 Quill overflow retrieval | NOT DELIVERED | Deterministic bounded candidate selection at 255+ skills; no alternative engine | P2 |
| V06 Jev protocol and local scoring | NOT DELIVERED | Typed two-stage answers, none competition and eligible normalized scores | P1/P4 |
| V07 Bounded runtime and transport | NOT DELIVERED | Owned cancellation, verified TLS, whole-invocation deadline and attempt accounting | P1/P4 |
| V08 Core CLI and onboarding | NOT DELIVERED | Demo, doctor, roster, dry-run and authorized rank from an isolated user setup | P4 |
| V09 Explanation and disclosure | NOT DELIVERED | Stage reasons and receipts without changing requests or selection | P2/P3/P4 |
| V10 Cache and replay | NOT DELIVERED | Exact namespace-bound reuse; opt-in no-clobber capture and inert offline replay | P4 cache / P5 replay |
| V11 Ledger and feedback | NOT DELIVERED | Durable bounded observations, historical attribution and atomic corrections | P5 |
| V12 Claude hook and installation | NOT DELIVERED | Real verified prompt event, quiet failures, shadow default and reversible managed settings | P6 |
| V13 Allowance, breaker and snooze | NOT DELIVERED | Durable before-send admissions, fenced outage recovery and scoped user controls | P6 |
| V14 Quality, performance and harm evidence | NOT ESTABLISHED | Frozen independent cohorts and honest all-invocation metrics meeting gates | P7 |
| V15 Calibration and evaluation numerics | NOT DELIVERED | Training/validation/holdout isolation, declared backends and explicit policy rollback | P5 evaluator / P8 calibration |
| V16 Optional TUI and experiments | NOT DELIVERED | Actual terminal interaction and separately gated equal-budget experiments | P9 |
| V17 Shipped product | NOT ESTABLISHED | DSR-built source-bound artifacts, checksums and independently verified target install | Release |

## Per-gap resolutions

### G1: Foundation and dependency qualification — PARTIAL → verified prerequisite

**Current:** the first source snapshot contained eleven Rust files and a duplicate compatibility model. Subsequent owner consolidation removed that duplicate; the canonical contract is `src/adapter.rs`. Foundation validation and arithmetic are real, but the executable remains a bootstrap. No `todo!`/`unimplemented!` implementation placeholders were found: the missing product is absent integration/code, not disguised stubs.

**Target:** one reproducibly buildable library/binary with qualified dependencies and executable evidence of the runtime's required leaf operations.

**Changes:** preserve the single package; qualify Asupersync DNS/TLS/bounded HTTP POST and cancellation, Quill feature graph and bounded indexing, and linked SQLite version before their product paths are enabled. Keep feature claims synchronized with actual dispatch.

**Success criteria:** locked default and relevant feature builds; source-bound receipts; forbidden dependency absence across normal/build/dev targets; local TLS and stalled-leaf evidence, not just successful dependency compilation.

**Dependencies:** prerequisite for the boundaries using each dependency; independent parser work need not wait for unrelated runtime experiments.

### G2: Session, privacy and roster pipeline — partial foundations → integrated local input

**Current:** identity/limits and emerging context/configuration/adapter/output modules exist but main does not consume them.

**Target:** exact authorized bounded inputs produce a validated local context and a loadable snapshot, with no model authority over identities or access.

**Changes:** wire source selection and identity first; overlay hook prompt by event identity; normalize only complete bounded records; resolve directives before redaction; enforce trusted configuration/network policy; implement visibility and collision rules; snapshot the same bytes used for hash/excerpt; provide explicit local resolution and deterministic Quill overflow.

**Success criteria:** isolated temporary filesystem trees; concurrent sessions/identical prompts; incomplete tails and compaction; duplicate keys and malformed metadata; 0/1/254/255+ candidates; redaction across truncation boundaries; content/precedence changes outside the shortlist; explicit manual-only references never instruct a permission bypass.

**Dependencies:** G1 contracts; feeds G3. Preserve actual existing task ownership and avoid a second adapter or scoring convention.

### G3: First complete recommendation — NOT DELIVERED → core CLI

**Current:** `main` has no ranking dispatch. No runnable recommendation was observed.

**Target:** the documented normalized-context journey runs end to end through Jev, local validation and publication.

**Changes:** implement strict argument/config validation, honest capabilities, demo/doctor/roster/dry-run, typed Jev questions/responses, bounded transport, explicit attempt admission, scoring and pre-publication revalidation. Connect one pipeline rather than independent command demos. Implement table/JSON outputs and typed failure exits. Unexecuted quantities remain null.

**Success criteria:** actual `sr` executable exercises synthetic demo, successful explicit resolution, denied-network dry-run, authorized synthetic-context two-pass ranking, valid abstention and malformed-provider unavailable; none ties exclude candidates; no stale publication; deadline includes capture and cleanup. Live Jev evidence requires separate consent and budget; offline fixtures never substitute for it.

**Dependencies:** G1 runtime and G2 input/roster boundaries. This is the core-value milestone, not the last task after optional experiments.

### G4: Persistence, replay and judgments — NOT DELIVERED → exact durable evidence

**Current:** no persistence/replay workflow is reachable from CLI.

**Target:** exact validated cache reuse and independently interpretable historical evidence without expanding disclosure or conflating generated/emitted/acknowledged output.

**Changes:** request/decision fingerprints; protected namespace keys; response cache and alias-pair policy; explicit ledger initialization and maintenance; atomic observation/cursor commits; historical membership snapshots; explicit no-clobber capture and pure replay; atomic paired corrective feedback; matched-cohort value reports.

**Success criteria:** real SQLite contention/crash/WAL tests; linked SQLite >=3.51.3; namespace and content invalidation; no-cache/no-ledger/no-persist distinctions; no transactions across HTTP; replay performs no ambient/source-path reads or network; missing stages stay not-replayable; failed alternate labels cannot partially commit; quotas preserve maintenance reserve.

**Dependencies:** cache and its verified SQLite foundation are P4 core requirements, not postponed to P5; the first writable store needs the linked-engine check. P5 ledger/capture/replay depends on the accepted P4 result/provenance boundary. Do not make a first stateless rank depend on optional history.

### G5: Safe real harness integration — NOT DELIVERED → verified Claude hook

**Current:** `sr hook claude` is unsupported, not a quiet usable hook.

**Target:** supported `UserPromptSubmit` integration has shadow-default behavior, deliberate advisory enablement and bounded non-blocking failure semantics.

**Changes:** classify hook boundary before non-exiting parsing; validate real event/session/visibility contract; render complete bounded envelope; track delivery uncertainty; implement managed install/uninstall with backup and conflict checks; trusted snoozes; optional durable allowance/breaker with per-attempt rechecks.

**Success criteria:** actual supported Claude event smoke; isolated-home install/uninstall round trips preserve unrelated settings; every pre-publication error returns empty stdout and exit zero; partial writes remain unknown; <=1024-character output; before-send durable debits survive crashes and rotation; expired lease owners cannot publish; no implicit network consent or ledger initialization.

**Dependencies:** G3 plus G4 where persistence is required. Do not advertise native omp/Codex support from normalized-input compatibility alone.

### G6: Prove usefulness and operational performance — NOT ESTABLISHED → measured gates

**Current:** documented targets and synthetic policy artifacts are not measured product outcomes.

**Target:** a frozen complete evaluation cohort establishes recommendation quality and operational behavior for the declared population.

**Changes:** independent labeling and consented data collection; family-separated splits; explicit live attempt/runtime caps; stage coverage measurements independent of production gate; all-invocation timing and failure accounting; controlled paired harm evidence; source/model/configuration-bound reports.

**Success criteria:** >=300 adjudicated primary cases (>=150 positive, >=100 no-match, >=50 near-miss across groups); >=50 positive overflow cases; coverage@254 >=98%, coverage@M >=95%; top-one precision >=90% with 95% lower bound >=80%; positive-case suggestion >=80%; needless suggestions <=5% with upper bound <=10%; justified one-sided harm-or-unresolved upper bound <=2%; fallback <=5% across >=500 representative hook invocations. Exact-cache p95 <=100ms and warm-network p50 <=600ms/p95 <=1500ms remain engineering targets until measured. Unstarted/missing cases cannot disappear.

**Dependencies:** G3/G5 executable paths; dataset/rubric preparation can start earlier. Code completion alone cannot manufacture independent consent, labels or provider service reliability.

### G7: Complete advanced promised workflows without blocking the core

**Current:** evaluation/calibration/TUI/description workflows are not reachable; `tui` is empty.

**Target:** all P8/P9 and I01-I15 commitments are individually implemented and separately qualified, with the original default policy retained.

**Changes:** train/tune/holdout calibration; managed-field rollback; declared numerical/data backends; valid weighted sampling and sequential-monitor epochs; optional terminal viewer/watch; Quill passage/query-view experiments and evaluation-only description overlays; description/gap diagnostics.

**Success criteria:** no future-label leakage or adoption-as-usefulness; unchanged alpha spending after corrections; explicit backend identity; terminal resize/stale-generation/cancellation/selection behavior; equal-budget held-out comparisons before promotion; no skill execution/editing, background probes or alternative inference provider.

**Dependencies:** P5 owns the evaluator and numerical/sampling tooling; P8 owns adaptation/monitoring. P9 depends on P7 and only those P8 outputs actually used. TUI consumes the P4 result model. No P9 experiment or generalized native adapter blocks first useful CLI delivery.

### G8: Honest documentation and distribution

**Current:** README intentionally describes the finished product; the comprehensive plan retains stale tracker prose. GitHub release listing returned none. Current edits are not a source-bound published release.

**Target:** users can distinguish implemented, tested, experimental and planned behavior and install a verified artifact.

**Changes:** update stale progress statements in engineering status and Beads while preserving the requested README voice; maintain capabilities/schema/exit parity; use DSR only; bind artifact hashes and test receipts to exact source revision; preserve license rider and contribution policy.

**Success criteria:** clean isolated install on every claimed target; documented commands work for their stated release stage; checksum verification; DSR provenance; no release claim from a host-only build; no GitHub Actions fallback.

**Dependencies:** staged claims follow corresponding delivered milestones; broad production claims additionally require G6.

## Bead reconciliation and refinement queue

1. Map every V/G item to live existing issue descriptions and acceptance criteria, not title matches alone.
2. Reuse existing implementation/proof tasks; create only genuinely missing gap or integration/evidence tasks.
3. Embed the full missing requirement and acceptance cases in each amended/new bead through `br` only.
4. Check whether existing P4/P6/P7 closures demand actual executable evidence, independent labels and source-bound artifacts.
5. Refine in place across completeness, dependency parallelism, adversarial/success proof, operational usability and final convergence. Record actual changes/no-change findings rather than claiming ceremonial rounds.
6. Validate cycles and robot triage after mutation. Future agents implement through `br ready`; this reality check does not silently implement the whole product.

## Verification ledger

Executed: direct compilation of `src/main.rs` and the command matrix below; 42 Python policy tests; 20 runner tests; 10 contract-matrix tests; Rust formatting. Earlier Rust test invocation timed out after 600 seconds during dependency fetching and is not a pass. Final acceptance reconciliation, graph review and the repair cycle are recorded below; historical receipts remain distinct.

## Ambition round 1 — replace broad milestones with executable dependency boundaries

Review question: improve the existing plan rather than add features. Correction applied above: P1 is runtime/transport/validator; P2 roster/privacy/retrieval; P3 context; P4 includes exact cache and first SQLite qualification; P5 ledger/replay/evaluation; P6 hook/controls; P7 measured rollout; P8 adaptation; P9 separate experiments. The initial draft incorrectly blurred these phases and is corrected in place.

The dependency chain is `P0 -> {P1,P2,P3} -> P4 -> P5 -> P6 -> P7`, then P8/P9 under their declared inputs. Existing `.3.18` owns the early shared CLI shell for P2 roster commands, avoiding a circular dependence on broad P4 dispatch. Preserve the original defaults and all later features without making them prerequisites for core value.

## Ambition round 2 — require evidence references to resolve, not merely look plausible

The checked-in matrix validator passed while nine of ten non-planned rows named nonexistent test symbols (one named a nonexistent file). For example `foundation_compiles_and_exports_expected_crate_contract` does not exist in `tests/bootstrap_cli.rs`; `tests/contract_matrix_contract.rs` is absent. Generic `runner-smoke` interpreter cases cannot certify Rust identity/output/configuration contracts. Several existing owner IDs exist but refer to different work (dry-run, demo, installer and shadow rows).

The refinement is a stronger closure contract, not a new ranking feature: existing `.1.9` must bind each executed claim to a real test/suite/case/assertion and exact accepted receipt, while `.1.12` independently rejects fabricated/missing/misowned references, absent inventory, incomplete coverage and mechanics-to-product substitution. Future planned references may remain prospective but cannot claim execution. Source identity must include compile-time README/plan inputs when Rust tests embed them.

## Ambition round 3 — preserve authority and statistical evidence through integration

The initial snapshot had two compatibility models: nine dimensions in `src/adapter.rs`, seven in `src/context/compatibility.rs`. Owner consolidation subsequently removed the duplicate; this is a resolved overlap, not a current missing-code defect. Preserve the canonical nine-dimensional model. Deserializing a claimed passed smoke receipt cannot establish trusted installed-version evidence; parse-only hook acceptance is distinct from permission to publish advice.

The output boundary must resolve the 2 MiB `OutputDocument` cap versus 256 MiB evaluation report contract, distinguish public failure kinds from internal reasons/comparison states, and keep synthetic demo context separate from recorded answer provenance. These are existing `.1.3` contract obligations, not permission to invent another command.

Statistical ambition means collecting the right evidence, not adding opaque mathematics: separate relevance, controlled harm and operational cohorts; preserve failed/unstarted/missing cases and actual full-roster labels; justify population/design/interval assumptions before results. Existing P7 collection/evaluation tasks already cover these, including controlled pre-promotion advisory trials. Zero flagged harm in 150 independent justified binomial units illustrates the 2% bound; it is not a universal magic sample size. Do not duplicate these tasks or mistake synthetic arithmetic for empirical benefit.


## Full vision and acceptance crosswalk

| Invariant | Observable acceptance condition | Exact source |
|---|---|---|
| INV1 Session authority | Workspace, source adapter/producer, session, agent, branch and epoch separate state; unknown attribution uses invocation-local identity and cannot update durable native state. Equal text never establishes event identity. | Plan:27–36, 269–305, 583–623; `docs/identity-contract.md`, `SessionIdentity`, `DurableEventKey` |
| INV2 User authority | Explicit references resolve before redaction, truncation, lexical retrieval and probability gates. Exclusions defeat advisory suggestions; conflicting positive/negative directives fail rather than being guessed. Manual-only references never authorize autonomous file-read bypass. | Plan:287–289, 406–410, 539–547 |
| INV3 Loadability | Provider IDs resolve only through the local option map. Publication validates current membership, precedence, relevant indexed/wide content, all shortlist versions/restrictions and every explicit target. Partial evidence cannot establish a global no-match. | Plan:373–426; README:734–749 |
| INV4 Honest decisions | `ranked`, `explicit`, `abstain`, `unavailable` remain distinct; absent stage values are null. Timeout, lexical miss, denied networking and missing coverage are not relevance judgments. | Plan:522–582, 893–923, 1020–1040; `docs/output-contract.md:23–108` |
| INV5 Honest quantities | Choice probabilities, fit estimates, relative score, concentration confidence, load adoption, judged usefulness and controlled task outcomes never substitute for one another. | Plan:535–582, 656–700, 1157–1192 |
| INV6 Whole-operation bounds | Deadline starts at process entry; every read, allocation, parser, subprocess, request/retry, lock, database action and output is bounded. No late publication or detached work. | Plan:198–245, 307–317, 755–791 |
| INV7 Privacy and trust | Network off by default; key presence is not consent. All outgoing fields are allowlisted/redacted before truncation and rescanned; imported data cannot grant roots, network, credentials or delivery authority. No raw bodies persisted by default. | Plan:281–337, 793–807 |
| INV8 Advisory only | Ranking, feedback and diagnostics never load, execute, edit, install or create skills, grant permissions or block agent continuation. | Plan:7–19, 27–36, 741–753, 925–976 |
| INV9 Evidence tiers | Synthetic fixtures, successful parsers, dependency compilation, mechanics evidence, real local effects, live provider, real harness and quality/release evidence are distinct gates. Positive counterparts prevent always-refuse implementations passing. | Plan:194–196, 1125–1155, 1273–1304; both test READMEs |
| INV10 Protected versus disposable state | Cache/ledger failure may degrade ranking, but an explicitly enabled allowance must refuse new requests without durable enforcement. Configuration controls remain readable under no-persist; replay is stricter and reads no ambient policy. | Plan:62–96, 613–623, 702–739 |

## Seventy testable goal obligations

### P0 — frozen foundations; no prerequisite phase

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G01 | Maintain one standalone Rust 2024 package/library and `sr` binary, dated toolchain and lockfile; public capabilities distinguish implemented, planned and tested support. Preserve license rider; prohibit unsafe crate code, runtime `ms`, alternate runtime and Tantivy. | Plan:15–19, 1042–1101, 1310; AGENTS:108–141; `docs/dependencies.md:10–33,88–92` |
| G02 | Freeze bounded, versioned local identities, normalized envelope and roster records: complete durable namespace or fresh invocation nonce; supplied loads cannot become observed; stable skill identity is independent of content hash; local path bytes survive and diagnostics hide private values. Duplicate keys/definitions fail without losing valid repeated references. | Plan:279–305,390–410; entire `docs/identity-contract.md` (`validate_definitions`, `SessionIdentity::event_key`, `SkillId::from_source`) |
| G03 | Freeze configuration/trust and resource schemas before side effects. Validate unknown/duplicate fields, scalar types, finite values and incompatible modes; ordinary precedence is defaults→user→allowlisted project→recognized environment→CLI, with security fields excluding project authority. | Plan:307–317,793–807,1008–1018; README:596–646 |
| G04 | Freeze decisions, failure kinds/exits, output quality/provenance, nullability, non-actionable demo/replay/report envelopes and snapshot-bound trace pagination. External raw-byte parsing must reject duplicates rather than accept a pre-collapsed map. Output validation is not proof of loadability or delivery. | Plan:809–923,1020–1040; `docs/output-contract.md`, `OutputDocument::{from_json,from_value,to_json,failure}`, `TraceCursor::resume` |
| G05 | Freeze adapter/source revision manifest and separate parse permission from native-advice permission. Every advertised installed-version support cell needs identity/visibility compatibility, all conformance dimensions and real-harness smoke evidence, not an official-schema citation or synthetic fixture. | `docs/adapter-contract.md:11–38`, `AdapterRecord::advice`, `AcceptInput`, `EmitNativeAdvice`; Plan:152–154,1310 |
| G06 | Freeze initial evaluation semantics and representative success/failure cases before tuning: acceptable sets, explicit-only exclusion, 0/1/2 loss, missing-versus-failed-versus-unstarted distinctions, family splits, gate thresholds and numerical counterexamples. Synthetic policy validation is not an evaluator or holdout. | `tests/eval/README.md`; Plan:668–700,1125–1192; document bead `sr-roadmap-l1i.1.8` |
| G07 | Maintain source-bound, bounded evidence mechanics: fixed expectations; complete event reconciliation; no retry erasure of failures; partial case selection cannot pass; isolation unavailable means blocked, not unsandboxed fallback; final source identity must match. Bind independent expected identity, not an artifact's own claim. | `tests/evidence/README.md`, especially `evidence.validate`; integration matrix `.1.9`, independent certification `.1.12` |
| G08 | Reconcile README/AGENTS/plan/examples with delivered stages and actual qualification revisions; retain imported-source path/hash/change/test provenance and full notices when copying begins. P0 acceptance explicitly names unresolved provider/harness/leaf-operation unknowns. | Plan:19,428–443,1310,1327–1366; `docs/dependencies.md:63–76`; foundation document IDs `.1.1`, `.1.2`, `.1.4`, `.1.7`, `.1.8`, audit `.1.13` |

### P1 — runtime/transport and response contract; depends P0

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G09 | On the exact selected Asupersync features, prove real DNS, trusted-root TLS, authenticated bounded JSON POST, timeout/cancellation and process exit. Use one shared Asupersync `Cx` source with Quill. No accept-all certificates or transparent `ureq` fallback. | Plan:477–489,755–775,1311; `docs/dependencies.md:14–47` |
| G10 | Canonical trusted HTTPS base origin appends `/v1/systemone` exactly once; reject userinfo/non-root paths/query/fragment. Disable redirects and hidden proxies; scope credentials/cache/allowance by origin. Loopback HTTP is explicitly development-only and credential-free. | Plan:479–489,793–805 |
| G11 | Validate every requested answer/type and exactly its option IDs; reject duplicates, foreign/missing IDs, non-finite/out-of-range values, invalid usage and bad chosen-option argmax. Distribution sum is positive and within `1e-4`; normalize only rounding drift and retain raw estimates. Enforce decoded/decompressed response bound. | Plan:522–533 |
| G12 | One root-owned invocation propagates remaining monotonic time into every leaf; no new work consumes output reserve. Exercise slow stdin, blocking leaves, saturated child pipes, signals, cancellation and broken pipes; terminate/reap owned process tree. Retry only classified transient failures within attempt/deadline limits; preserve unknown paid usage. | Plan:761–791,1127–1155; local proof and separately budgeted live contract smoke are different evidence |

### P2 — roster, redaction, retrieval; depends P0

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G13 | Resolve roster authority in order: explicit manifest replaces enumeration; harness inventory; verified adapter roots/precedence; explicit generic roots with unverified visibility. Validate manifest permissions and root containment independently. Missing optional root differs from configured unreadable root; unsupported source coverage is disclosed. | Plan:373–388 |
| G14 | Distinguish stable ID/invocation name/display name/source/content/version/load target; canonical-file aliases deduplicate, display-name collisions do not. Apply real shadow winners and invocation restrictions before retrieval; exclude ambiguous names, never invent a callable qualified name. | Plan:390–408; `docs/identity-contract.md` |
| G15 | Explicit resolution inspects full bounded user directives and full visible roster before advisory filtering. Quoted/code/tool examples are not positive requests. Resolve every reference, allow manual-only references, report every missing/ambiguous/forbidden/conflicting target separately, and make zero advisory calls on unresolved input; never truncate success to K. | Plan:287,406–410; 32-reference limit |
| G16 | Parse bounded YAML/frontmatter with adapter-pinned boundary/boolean/name semantics, BOM/CRLF/multiline/fence cases. Missing metadata may fall back; malformed or ambiguous metadata excludes with sanitized partial-coverage diagnostics. Never expand command substitutions, placeholders or helper scripts. | Plan:412–418; duplicate-key rule Plan:283 |
| G17 | Open only permitted regular files through race-safe identity/traversal checks, detect symlink escape/cycles, hash and excerpt the same capped bytes. Before publication revalidate all causal roster dependencies, including changed wide candidates outside M, new overflow winners and new shadows. Changed/incomplete validation returns unavailable rather than promoting a runner-up. | Plan:420–426 |
| G18 | Redact full bounded fields before excerpting and scan assembled outgoing payload; include roster descriptions/body/tool arguments and secret-split boundaries. Do not retain matched text or raw parser failures. Preserve Unicode/truncation provenance; patterns do not promise confidentiality detection. | Plan:319–329,428–443,1127–1131 |
| G19 | For ≤254 eligible skills skip lexical filtering; above it use only pinned Quill BM25, stable-ID single-shard ingest, commit-before-search, title/content field mapping, escaped literal-term OR and shared fuel/memory/deadline bounds. Admit actual matches only; zero matches/fuel/parser/index failures are unavailable, not padded or alternate-engine results. Verify cutoff ties and all supported dependency graphs. | Plan:445–463 |
| G20 | Produce stable inspection/exclusion reasons for I01 and opt-in I09 snapshot/diff: additions/removals/content/restrictions/shadowing/invocation changes, compatible workspace/adapter/source namespace, incomplete scan ≠ deletion, inert saved paths, owner-only atomic no-clobber export. | Plan:46–54,68–70,136–138,426; CLI stage table Plan:987–989 |

### P3 — exact context and adapters; depends P0

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G21 | Mutually exclusive explicit source selection; no fallback to another conversation on failure. Non-TTY does not imply transcript/hook input. Bare discovery requires unambiguous selection or TTY choice; `--latest` is explicit, hook never guesses it. | Plan:247–267 |
| G22 | Claude `UserPromptSubmit` validates event/session/transcript association before reading and overlays authoritative incoming prompt once using identity/proven adapter rule. First missing transcript is prompt-only; existing malformed transcript is an error. Parent/subagent/branch identities do not merge. | Plan:269–289; `docs/adapter-contract.md:40–60` |
| G23 | Normalize branch-aware parent/tool/turn/compaction history; timestamp sorting alone is insufficient. JSONL uses length snapshot, complete boundaries and generation-aware cursor recovery; incomplete tail deferred, malformed completed record surfaced. Maintain separate bounded observation watermark with no advance across unread bytes. | Plan:291–305 |
| G24 | Retain recoverable active-task anchor for terse continuations; essential directive/antecedent/attachment loss is unavailable. Drop reasoning/media and only provenance-identified prior sr output. Request appears once; deterministic budgeted head/tail summaries preserve tool association/status and local load evidence. | Plan:287–329 |
| G25 | I06 standard/minimal profiles and field receipts are tested against actual outgoing bytes. Minimal excludes optional history/tool bodies/dirty paths but retains essential request/anchor/constraints/candidate material; no-tools further restricts. Project settings cannot widen trusted minimal; essential omitted evidence causes unavailable. | Plan:124–126,339–371 |
| G26 | Project signals use trusted executables and allowlisted filenames/tools without running project binaries; safe version-qualified Git disables fsmonitor/untracked cache/optional locks/renames/submodules, scrubs routing/config environment and bounds pipes. Unsupported/slow signal is omitted, not retried unsafely; non-UTF-8 paths remain locally intact. | Plan:331–337 |
| G27 | Optional cass is capability/version/source bound, rejects remote records by default, reports pagination/incomplete discovery, normalizes native exports itself and does not assume no-tools/loaded evidence. No auto-index/daemon; offline/dry-run/local-only explicit cass refuses with exit 7. I13 conformance includes sanitized real protocol samples and versions. | Plan:263–267,152–154,801; `docs/adapter-contract.md` |

### P4 — first usable CLI pipeline; depends P1, P2, P3

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G28 | One pure pipeline builds bounded wide state/questions with real none option, three oriented gates, phase and optional diagnostic stuck. Gate mean uses inverted context-suffices; below 0.30 skips rerank. Passing gate retains M real candidates even when wide none wins; singleton works; validate configured `1≤K≤M≤32` before effective clamping. | Plan:491–513 |
| G29 | Detailed Choice compares M candidates plus none, includes bounded meaning in every fit question and treats all untrusted material as data. Trim older context/excerpts in fixed order while retaining admitted candidates/sentinel; impossible request size yields unavailable, never relevance abstention. | Plan:479–489,514–520 |
| G30 | Eligibility precedes blend: restrictions/exclusions/reusable-reference evidence, then fit≥threshold, then **each** candidate's raw rerank probability strictly greater than none. Sentinel ties abstain. Never let a high fit/prior/phase rescue a removed candidate; exercise both documented misleading-runner-up counterexamples. | Plan:535–553 |
| G31 | Compute finite clipped-log/log-odds utility and max-shifted softmax over all eligible shortlist records before top-K; preserve raw probabilities/fits and omitted mass. Defaults `(w_fit,w_prior,w_phase)=(1,0,0)`, bounded weights; singleton score 1 ≠ useful certainty. Reusable references require complete matching rendered content/current epoch; workflows/unknown usage remain invocable. | Plan:555–581 |
| G32 | Cache only validated stage responses under exact keyed request and session-scoped decision identities. Re-enumerate/retrieve and ingest current evidence before lookup/publication; TTL begins at receipt, rollback invalidates, keys rotate. No stale hook fallback, mismatched shortlist reuse or cached-wide/fresh-rerank pair under unversioned model alias. | Plan:583–603 |
| G33 | Fenced exact-request single-flight shares responses, not decisions/events/exposures. Followers retain own deadline/generation/eligibility and add zero new usage; missing owner accounting is unknown. No-cache disables cross-process response sharing without hidden response bodies; no-ledger/no-persist/dry-run obey independent restrictive semantics. SQLite-backed P4 stores must already satisfy linked-engine minimum. | Plan:605–623,706 |
| G34 | Actual CLI dispatch supplies rank/roster/doctor/capabilities/demo plus JSON/table, strict source/flag/config validation and meaningful errors. Stdout is data, stderr sanitized; non-TTY defaults JSON. Dry-run is exact stateless wide payload with zero network/state creation; stage 2 requires explicit shortlist/recorded wide evidence. Explicit/local termination produces no pretend request. | Plan:978–1040; README:332–455; AGENTS:399–447 |
| G35 | I01 explanations trace all eight stages, first decisive exclusion and later actually evaluated stages; unknown/unevaluated gets no fabricated operands. Include threshold/tie/version/action hints, bounded snapshot pagination. Explain/why-not leaves decision, request bytes and request count identical. | Plan:46–54,919–923; `docs/output-contract.md:138–161` |
| G36 | I05 demo exercises normalization→validation→decision→rendering with labeled useful/none/explicit/unavailable fixtures, no ambient config/state and non-actionable output. Doctor separates local readiness, key presence, consent, time/scoped transport evidence, ledger and hook mode; no implicit live check/install/migration. I11 doctor-config shows winning sources/blocked overrides and fingerprint only when valid. | Plan:110–120,144–146; pure replay envelope defined here but capture/import waits P5 |

### P5 — optional durable evidence and evaluator; depends P4

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G37 | Bounded owner-only local SQLite ledger with verified ≥3.51.3 actual engine/source ID, migrations/checksums, foreign keys, short WAL transactions and ≤25ms busy budget. No network-filesystem ledger or transaction across HTTP; cache/coordination and optional ledger have distinct roles. Preserve event/stage/attempt/roster/observation/judgment/calibration identities. | Plan:702–729 |
| G38 | Explicit init is idempotent; hooks never initialize/migrate. Preview/apply migration uses WAL-aware recoverable backup; unsupported schema is not downgraded/repaired destructively. Every mutation checks store incarnation/schema/data generation; clear rejects stale writers without resetting allowance or disabling future recording. Quotas reserve real maintenance headroom; retention is logical, pruning physical and explicit, never claimed secure erasure. | Plan:725–739 |
| G39 | Observe structured successful target loads separately from attempts, unknown versions, unobservable and censored outcomes. Attribute latest preceding eligible same-agent/turn emission using event order; close final turn via explicit source-bound observe. Atomic observations+loaded-state+cursor CAS prevent crash skips/duplicate rewards; no-persist/no-ledger conflict with promised observe writes. | Plan:628–644 |
| G40 | Separate generated/prepared/emitted/acknowledged; SQLite and stdout are not atomic. Zero-byte shadow output is not exposure; duplicate verified deliveries do not create extra training examples, ambiguous identities do not collapse turns. CLI/TUI/shadow/advisory channels retain separate denominators; optional recording failure remains visible and does not break ranking. | Plan:646–664 |
| G41 | I02 capture/import freezes actual redacted inputs, option maps, validated responses, versions, as_of, snoozes, loaded/visibility evidence, numeric prior/phase values, computation profile and per-stage completeness. Capture is opt-in owner-only capped atomic no-clobber; no credentials/key/error bodies. Failed capture preserves incurred usage and returns storage/timeout failure. | Plan:56–72 |
| G42 | Replay reads only explicit case/local-policy files: no ambient configuration, current clock/priors, source-path access, sockets, children, writes or live cache import. Compatible exact replay reproduces decisions/numerics; missing stage/uncaptured prior is not-replayable; changed prompts/model/retrieval/excerpts/M require new consented answers. Unavailable history can replay failure metadata; profiles/tolerances govern cross-platform parity. | Plan:62–72; `docs/output-contract.md:110–136` |
| G43 | I15 feedback uses one current revisioned assessor/provenance label per historical event/skill version. Distinct original/alternative resolve full historical roster+eligibility, commit negative+positive+group atomically with expected revisions. Failed lookup cannot leave lone negative; absent alternative is prospective, missing snapshot unknown; partial/unblinded correction never becomes full acceptable-set or independent holdout evidence. | Plan:160–164,662–664,727 |
| G44 | I14 value reports disclose evaluated/emitted/abstained/muted/failed/load/judged cohorts with every numerator/denominator/unknown count. Known request/token usage and unknown attempts remain separate; cache/followers add zero new cost. Cost per useful suggestion uses only matched judged cohort and applicable versioned price; zero labels/unknown usage never yields exact dollars or labor savings. | Plan:156–158,656–666,789–791 |
| G45 | Evaluator defaults offline; live runs require online+trusted consent+explicit batch HTTP-attempt cap, 600000ms default overall runtime and per-case deadline. Preserve unfinished and failed cases; report complete/partial and independent gate state. Freeze full-roster independent acceptable labels, family splits/provenance, gate-independent retrieval/shortlist coverage and Quill/cookbook/Choice/fit/blend/context baselines. | Plan:783–785,1038,1157–1192 |
| G46 | I12 perturbations cover order/opaque IDs/whitespace/duplicate-looking descriptions/decoys/distraction/hostile instructions. Local equivalent semantics obey exact invariants; changed Jev inputs consume new responses/budget and yield measured decision/coverage changes, not required identical probabilities. Variants remain in original family/split. | Plan:148–150,1127–1155 |
| G47 | Freeze family representative, frame, strata and positive allocations before labels; uniform-without-replacement draw records RNG/seed/randomization provenance and justified `pi=n_h/N_h`. Manual seed is diagnostic, replayed manifest is the same draw, full census needs no randomness. Unknown metadata remains in frame; no outcome-selected redraw or diagnostic queue in holdout denominator. | Plan:1194–1213 |
| G48 | Weighted bounded-loss reports use `sum W_h mean_h`/Horvitz–Thompson on declared frame, missing labels bounded 0/1, exact census and declared Hoeffding/union bound; unsupported probability/design blocks inferential claims. Ratios require separate method, not weighted Wilson pseudo-counts. Exhaustively enumerate tiny samples, verify joins/nulls and numerical endpoints, compare uniform versus stratified at equal budgets, and recompute explanation cards. | Plan:1103–1119,1215–1233,1260–1271 |

### P6 — shadow hook and operational controls; depends P4 and P5

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G49 | Actual supported Claude UserPromptSubmit boundary classifies hook mode before non-exiting argument parsing. Every pre-publication API/input/config/privacy/coverage/timeout failure yields empty stdout, sanitized stderr and exit 0, never block fields; shadow emits no advice. Render entire ≤1024-scalar envelope before publication; partial/broken writes are unknown, never retried/replaced or marked emitted. | Plan:925–952 |
| G50 | Managed install/uninstall preview exact sanitized diff; explicit apply uses trusted absolute command, escaped args, timeout, private backup, cooperating lock and digest conflict detection. Preserve unrelated settings and permissions, idempotency, managed restrictions; modified entries conflict. Outer timeout initially 4s, later internal deadline cannot exceed installed budget without reinstall/clamp/refusal. Disclose unsupported external concurrent edits. | Plan:954–967 |
| G51 | I05 isolated-home onboarding proves demo→doctor/roster→exact source→dry-run→authorized rank→recorded shadow→advisory choice. Missing key, denied network, empty roster and missing ledger each give correct next step. Installer never silently authorizes hook networking or initializes ledger; one-shot CLI consent does not authorize later hooks. | Plan:110–120; README:259–330 |
| G52 | I03 optional shared allowance accepts 1–10000 admissions per fixed UTC hour in user/endpoint scope. Setup preview identifies boundaries/charges; lock-protected durable activation intent→accounting generation→ready transition preserves charges and resumes after crash. Every attempt re-reads guard and durably debits under same bounded setup lock before HTTP. Missing/mismatched/full/busy/corrupt accounting refuses; no-persist cannot use active guard. | Plan:74–90 |
| G53 | Allowance permits are unique single-use request/endpoint/generation/window/deadline bound; no future preallocation/refund of possibly sent or expired permits. Every accounting writer verifies WAL+synchronous FULL and sync fits deadline. Preserve charges through restart/config/key rotation/cache/ledger cleanup. Counts are admission-window, not wire/billing-hour, guarantees; wide-only allowance exhaustion returns unavailable with usage. | Plan:80–90 |
| G54 | I03 breaker classifies actual transient attempt failures, opens after 3 failures for 30s, doubles failed half-open cooldown to 5min cap and honors longer Retry-After by refusing early admission. One fenced next-real-request probe, no background health call. Stale generations cannot close successors; auth pauses isolate credential profiles or remain invocation-local. Non-provider errors/abstentions never poison circuit. | Plan:92–96 |
| G55 | I04 snooze preview/apply verifies historical event workspace/session/agent/branch, supports one skill/all/clear mode, ≤128 entries, 1min–24h duration, trusted config backup/conflict rules. Apply before retrieval, all-muted makes zero provider calls, explicit references override, explanations retain muted records. Unknown expiry stays muted; no-ledger/no-persist still read controls, ranking never writes cleanup or usefulness labels. | Plan:98–108; README:1432–1471 |
| G56 | I13 native advice has per-installed-version pass evidence for prompt timing, branch, visibility, restrictions, compaction, load evidence, output, deadline and delivery, plus real-harness smoke. Measure real concurrent admission, cancellation/restarts/clock changes/output/setup failure paths; unknown incompatible semantics disable advice. | Plan:152–154,1125–1155,1316; `docs/adapter-contract.md:22–38` |

### P7 — measured advisory rollout; depends P6

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G57 | Promote only complete compatible frozen cohorts meeting declared retrieval, shortlist, precision, positive-case, no-match and operational thresholds. Family independence alone does not justify binomial inference; freeze population/design/interval method before results. Missing coverage/zero emissions/insufficient evidence is not established. | Plan:1273–1304; numerical table below |
| G58 | Controlled harm cohort uses isolated equivalent baseline/advice snapshots, identical permissions/budgets/settings, randomized arm order and blinded adjudication. Count a family once if any planned replicate is newly harmful or required judgment unresolved; report separate harms/missing and net difference. No side-effecting replay against production accounts and no degenerate zero-harm bootstrap. | Plan:1190–1192,1294–1302 |
| G59 | Measure named-hardware/roster/model/region all-invocation latency, cache/network strata, startup/TLS/index-build/search/rehash, fallback, memory, attempts/tokens/unknown usage. Require fallback≤5% over ≥500 representative invocations; successful-call latency cannot hide timeouts. | Plan:230–245,1183–1189,1304 |
| G60 | Enable advisory mode deliberately after gates, preserve local return-to-shadow/baseline switch and report I14 usefulness/interruption/usage together. Evaluate I04 optional abstention/repeated-advice suppression separately; reduced output/adoption/time-to-value cannot be called task benefit without observations. | Plan:98–120,156–158,945–965,1317 |

### P8 — evaluated adaptation; depends P5 and P7

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G61 | Calibrate only independent labeled evidence with train/validation/untouched temporal/family test separation; priors predate scored case. Common judged cohort uses loss 0 correct, 1 positive abstention, 2 incorrect or attempted unavailable; equal loss prefers fewer failures then frozen baseline. Missing responses/unstarted cases prevent complete comparison/promotion, never invented zeros or selective denominator removal. | Plan:668–683 |
| G62 | I11 calibration preview/apply/rollback is explicit, provenance-bound, schema/digest/conflict checked and reversible for managed ranking fields only. Preserve current networking/credentials/hooks/feedback/unrelated config; reject incompatible revisions. | Plan:144–146,670,1001–1002 |
| G63 | Optional Beta(1,4) prior is disabled until held-out benefit. `q=(useful+1)/(judged+5)`, shrink=`judged/(judged+20)`, centered capped log-odds contribution, w_prior≤0.5. Key by content/scope/evaluator/policy/phase; changed versions do not inherit labels, sparse cells back off to pooled or zero. No implicit adoption training or eligibility rescue. | Plan:684–700 |
| G64 | Optional sequential monitor uses new-harm-or-unresolved prospective family units, conditional risk null τ=.02 and fixed q=.05/.10/.20 likelihood-ratio mixture. Process once in declared order, frozen adjudication deadlines, log arithmetic; alarm at 1/alpha. Missing-only alarms are not harm findings; absence of alarm is not certification; arbitrary finite samples do not satisfy this conditional model automatically. | Plan:1235–1251 |
| G65 | Monitor binds cohort/rubric/baseline/policy/model/order and preallocated alpha across all restarts. Finalized-label correction invalidates epoch but preserves original alarms/spent alpha; corrected history descriptive only, new inference needs prospective units/fresh allocation. Corruption/full/missing state suspends rather than resets. Exhaustive short-path/log checks and shift-detection comparisons precede enablement; alarm blocks promotion but never silently mutates live config. | Plan:1251–1268 |

### P9 — separately gated optional capabilities; depends P7, P8 only where used

| Goal | Concrete testable requirement | Evidence/citation |
|---|---|---|
| G66 | I07 passage experiment indexes heading-delimited passages only inside already selected authorized bounded skill bytes; retain purpose/restriction prefix, heading/position/version and 700-scalar body cap. Full-field redaction precedes excerpt; successful no-hit uses declared lead, attempted failure unavailable. Held-out relevance benefit at same disclosure/request/latency budget precedes promotion. | Plan:128–130 |
| G67 | I08 overflow experiment uses ≤3 deduplicated request/anchor/error Quill views, each≤254 hits, pinned reciprocal-rank fusion and stable ties, total≤254. Shared aggregate term/character/fuel/memory/deadline limits; attempted view failure invalidates result, empty union retrieval-empty, ≤254 roster behavior unchanged. Measure multilingual/continuation/adversarial overflow coverage at equal budget. | Plan:132–134 |
| G68 | I10 description overlays bind source digest and remain evaluation-only, never hooks or live skill edits. Compare positive/near-miss consented cases with new compatible responses for changed descriptions and untouched final families. Description/gap doctor reports deterministic metadata/prefix issues and suspected gaps, not causal diagnosis; optional online numeric rubric is separately authorized/bounded. Retained gap prose is opt-in short-expiry, Quill-only lookup, optional lexical clustering, local review export. | Plan:140–142,741–753 |
| G69 | Optional FrankenTUI renders actual terminal surface, not agent-owned stdin; ~9 rows with display-cell sizing, freshness/source/fit/relative score distinct. Keys 1–5/r/w/e/q select reference/refresh/watch/explain/exit without load/execute; terminal restoration and clean piped selection survive resize/non-TTY/cancel. Watch one active session evaluation, ≥5s interval, generations prevent stale display. | Plan:787,968–976; AGENTS:442–447 |
| G70 | Additional native adapters need their own verified event/prompt/identity/visibility/output/deadline/delivery contracts, not normalized input or compilation alone. Chunk experiment uses deterministic ≤254+none groups, concurrency initially2, explicit request/token/round/deadline caps, common-set reductions, no cross-chunk probability comparison; incomplete chunks withhold action and measured recall/cost precede default change. | Plan:13,465–475,967,1279–1280,1319–1325 |

## Improvement coverage crosswalk

| Improvement | Delivery boundaries | Goal rows and decisive extra proof |
|---|---|---|
| I01 Why-not/stage explanation | P2 reasons → P4 rendering | G20/G35: unchanged decision, request bytes and count; first decisive exclusion versus not-evaluated/not-in-snapshot. Plan:46–54 |
| I02 Capture/replay/compare | P4 pure contract → P5 capture/import | G41/G42: frozen evaluation state, zero ambient/source/network effects, complete/no-clobber export, missing-stage honesty and incurred usage. Plan:56–72 |
| I03 Shared attempts/breaker | P4 admission seam → P6 guard | G12/G52–G54: before-send durable debit, activation/admission lock race, permits, generations and credential isolation. Plan:74–96 |
| I04 Silence/snooze | P6 control → P7 value trial | G55/G60: exact scope/expiry/explicit precedence and zero all-muted calls; repeated-turn suppression stays separate experiment. Plan:98–108 |
| I05 Onboarding/demo | P4 demo/doctor → P6 real setup | G36/G51: synthetic ≠ live health, key ≠ consent, separate ledger initialization, useful isolated-home failures. Plan:110–120 |
| I06 Disclosure/minimal | P3 normalization → P4 bytes/rendering | G25/G34: actual transmitted fields and essential-context failure; smaller payload ≠ equal quality without comparison. Plan:124–126 |
| I07 Passage retrieval | P9 only | G66: already selected skill only, lead no-hit fallback, shared existing budget, held-out gain. Plan:128–130 |
| I08 Multi-view overflow | P9 only | G67: dedup views/no extra votes; one failed attempted view invalidates entire result. Plan:132–134 |
| I09 Roster drift | P2 command/snapshot → P5 historical use | G20/G37/G43: namespace/version/partial coverage; no false deletion or authority from saved paths. Plan:136–138,192 |
| I10 Description overlays | P9 only | G68: stale digest rejects, changed description needs new answers, no live edits/hooks. Plan:140–142 |
| I11 Effective config/rollback | P4 provenance → P8 learned rollback | G03/G36/G62: invalid configuration has no fingerprint; rollback only managed policy fields. Plan:144–146 |
| I12 Perturbation suite | P5 | G46: exact local semantic invariants versus stochastic measured Jev sensitivity; no inflated family denominator. Plan:148–150 |
| I13 Adapter compatibility | P3 fixtures → P6 real harness | G05/G27/G56: installed-version evidence per dimension plus real smoke; unknown incompatible semantics disable advice. Plan:152–154 |
| I14 Value report | P5 reporting → P7 evaluation | G44/G60: matched judged cohort, unknown cost, distinct shadow/cache/advisory denominators. Plan:156–158 |
| I15 Better-alternative feedback | P5 | G43: full historical eligibility/membership, atomic paired labels, prospective absent alternative, partial unblinded evidence. Plan:160–164 |

All improvements require focused success and consequential-failure fixtures **plus actual CLI integration**, bounded sanitized structured evidence and separate live/harness qualification; help snapshots are insufficient (Plan:190–196).


These obligations describe required behavior, not claimed execution. G01–G08 have partial foundation implementations; G09–G70 are not delivered as integrated product workflows in the inspected bootstrap. The V01–V17 and G1–G8 sections above state observed status and the bridge; the existing-owner table below supplies implementation and proof ownership.

## Existing implementation and proof owners

All suffixes below use `sr-roadmap-l1i`. These are ownership mappings, not passed milestones. Phase epics organize work; blocking acceptance edges, not parent-child membership, establish gates.

| Goals | Implementation / proof / acceptance owners |
|---|---|
| G01–G08 | P0 `.1.1`–`.1.14`; matrix integration `.1.9`, independent certification `.1.12`, reconciliation `.1.10`, gate `.1.11` |
| G09–G12 | P1 `.2.1`–`.2.12`; live Jev `.2.10`, runtime integration `.2.11`, gate `.2.12` |
| G13–G20 | P2 `.3.1`–`.3.18`; early local CLI `.3.18`, snapshot/export `.3.14`/`.3.15`, proof `.3.16`, gate `.3.17` |
| G21–G27 | P3 `.4.1`–`.4.14`; profiles/receipts `.4.11`/`.4.12`, conformance `.4.13`, gate `.4.14` |
| G28–G36 | P4 `.5.1`–`.5.22`; pipeline `.5.11`, core executable journeys `.5.19`, cache proof `.5.20`, gate `.5.21`, properties `.5.22` |
| G37–G48 | P5 `.6.1`–`.6.29`; capture/replay `.6.14`–`.6.16`, evaluation `.6.17`–`.6.25`, proof `.6.26`–`.6.28`, gate `.6.29` |
| G49–G56 | P6 `.7.1`–`.7.13`; protocol `.7.1`, installer `.7.3`, controls `.7.4`–`.7.8`, actual harness `.7.11`, latency `.7.12`, gate `.7.13` |
| G57–G60 | P7 `.8.1`–`.8.10`; relevance `.8.1`, paired cohort `.8.2`/`.8.3`, representative operations `.8.4`, value `.8.5`, promotion `.8.6`, DSR release `.8.7`, gate `.8.8`, controlled experimental emission `.8.9`/`.8.10` |
| G61–G65 | P8 `.9.1`–`.9.9`; priors/calibration `.9.1`–`.9.3`, rollback `.9.4`, monitoring `.9.5`–`.9.7`, proof `.9.8`, gate `.9.9` |
| G66–G70 | P9 `.10.1`–`.10.32`, `.10.1.1`, `.10.8.1`–`.10.8.3`; separate experiment/proof/disposition chains, with six native implementation/proof items deferred in the first inventory |

The suspected missing final CLI, release and empirical-cohort tasks already exist. `.5.19` requires actual subprocess journeys and honest positive recommendations; `.5.21` consumes accepted P1/P2/P3 and core/cache/property proof, not P9. `.8.7` explicitly owns DSR artifact/install verification. `.8.1`/`.8.2`/`.8.4` require actual independent labeled/paired/representative cohorts. `.8.9`/`.8.10` permit controlled experimental delivery before public promotion, avoiding a circular requirement to promote before collecting delivery evidence. Do not duplicate these tasks.

## Executed verification and its limits

The directly compiled bootstrap used `rustc --edition=2024 src/main.rs` with package version `0.1.0`, not a claimed complete Cargo build. Source SHA-256: `7a4a7076084abe59f93ee9bcdb40828e84231ec2217f0e2e69dc24905efbf701`; binary SHA-256: `d46bd7aa6203fd906f4e92df308407db31cca62f38c0d100c6d5c186feae4061`. Retained binary: `/data/tmp/skillranker-reality-avp2cdyz/sr`.

| Actual invocation | Exit | Observation |
|---|---:|---|
| `sr --help` | 0 | Foundation-only usage and Jev/key/consent requirement |
| `sr --version` | 0 | `sr 0.1.0` |
| bare `sr` | 2 | Empty stdout; unsupported arguments diagnostic |
| `sr demo --case useful` | 2 | Same unsupported-command behavior |
| `sr rank --offline` | 2 | Same unsupported-command behavior |
| `sr capabilities --json` | 2 | Same unsupported-command behavior |
| `sr hook claude` | 2 | Not yet a nonblocking hook boundary |
| `sr tui` | 2 | No interactive viewer |

- `python3 -m unittest discover -s scripts -p 'test_*.py'`: 42 passed; policy artifacts `/data/tmp/skillranker-policy-tests-0jglgh_h`. Expected negative-fixture diagnostics are not suite failures. This run predates the reference-validator repair; the repaired suite is the 10-test `scripts/test_contract_matrix.py` record below.
- `python3 -I -B scripts/e2e/test_runner.py`: 20 passed in 17.043 seconds; artifacts `/data/tmp/sr-runner-tests-4nsd1w38`. These exercise fixture-interpreter mechanics, not ranking, provider TLS or actual harness integration.
- Original runner result was 18 passes and one fixed-marker assertion failure. Per-run UUID correction was already applied by a peer; two concurrent full 19-test runs and a final full 19-test run passed. A later controlled test passed while an unrelated owned process carried the old marker. One additional focused attempt timed out; retained as an unsuccessful attempt. Subsequent peer fault-observed assertions prevent absence-only timeout proof; Main did not overwrite those source changes.
- `cargo fmt --check`: passed for the observed checkout.
- First RCH `cargo test --locked -j 4` timed out after 600 seconds fetching dependencies. Resumed run `30024414133223666` on `ovh-a` completed the remote test command with exit 0, but the source-content barrier detected four deltas and RCH exited 103. **Neither run supplies accepted frozen-source Cargo proof.** The remote test log is diagnostic evidence, not a release or current-tree certificate.
- `br dep cycles --json`: zero active cycles. `bv --robot-triage` ran successfully and identified P0 acceptance and P4 core acceptance as central bottlenecks; a high triage score does not make a blocked task ready. Its early 182-item view and the final 183/189-issue views reflect a moving inventory; scope and denominators differ and are reported separately.
- No personal transcripts or credentials were used; no Jev request, installed hook mutation, quality trial or release was performed.

## Refinement rounds and applied dispositions

1. **Completeness:** appended concrete false-reference/inventory/receipt requirements to `.1.9` and independent negative/positive certification cases to `.1.12`. Existing requirements remain; no duplicate feature task.
2. **Ownership and dependencies:** appended exact dry-run/demo/installer/shadow owner corrections and explicit aggregate coverage to `.1.9`; appended output reason/status, report framing, stale inspection and provenance reconciliation to `.1.3`. Preserved core-first and experimental-emission ordering.
3. **Observable proof and authority:** appended canonical nine-dimensional model, imported-claim versus trusted evidence, optional-field typing and downstream actual-harness proof to `.1.6`; appended owned fault witness and retained failure-history requirements to `.1.12`. Closed P0 adapter contracts were not reopened merely because native runtime is future work.
4. **User-facing usability:** appended current-versus-historical status, meaningful key/consent onboarding, estimate independence, empty TUI feature, report bounds and example verification to `.1.10`. Did not rewrite another owner's README, plan or changelog while their contract work was active.
5. **Convergence:** final full-inventory reconciliation and the reference-validator repair are recorded below. A no-change conclusion is valid only after the remaining review completes; grouping earlier findings into five headings does not constitute five completed rounds.

All tracker amendments use `br comments add`; no owner, dependency, status or existing acceptance text was overwritten. The comments are self-contained requirements with proof boundaries, not completion claims.
## Final graph validation (2026-09-17T12:47Z)

- `br dep cycles --json`: zero active cycles after all closures.
- `bv --robot-triage`: 183 issues, 173 open, 5 in progress, 13 actionable, 160 blocked; zero ready P0 gates. Central bottlenecks remain acceptance gates `.1.11` (0.605) and core CLI `.5.21` (0.418), followed by P7 gate `.8.8`. No cycle or reachability failure surfaced; triage scores rank, they do not pass gates.
- Contract matrix regression suite: 10/10 pass; production validator exit 0 over 69 boundaries; all ten non-planned executed references resolve to real declarations. Remaining executed refs cover Python (`scripts/check_dependency_graph.py::main`, `scripts/test_eval_policy.py::EvaluationPolicyContract`, `scripts/e2e/test_runner.py::RunnerTests`, `scripts/test_contract_matrix.py::ContractMatrixTests`) and six exact Rust test fns. Matrix ownership repairs and validator tightening were peer work; Main verified by execution only.
- Rust/Python regression evidence remains as recorded above; no new gate claims from triage.

## Convergence record

Five refinement rounds were executed as a plan-space loop, each applying real changes and closing with a no-further-change decision for that scope:
1. Evidence mapping completeness (`.1.9`, `.1.12`).
2. Ownership corrections and phase ordering (`.1.9`, `.1.3`).
3. Canonical authority and observer/fault witnesses (`.1.6`, `.1.12`).
4. Public reconciliation usability (`.1.10`).
5. Convergence: no additional bead-level gap beyond recorded gates; validator/repair cycle closed with 10/10 tests and exit-0 production validation; remaining `.1.12` certification, `.1.10` doc reconciliation and the P0 acceptance gate stay open by design.

## Final live inventory and coverage verdict

- Initial audit inventory (historical, not the resumed snapshot): 189 issues — 168 open, 5 in progress, 10 closed, 6 deferred. Structurally: 406 blocking + 188 parent-child edges, 13,274 transitive blocking pairs, **zero cycles**, zero dangling endpoints, all 189 reachable from the root, and **no non-P9 issue transitively depends on a deferred item** (`.10.16/.10.18/.10.20`, `.10.8.1–.10.8.3` remain explicitly deferred with qualification owners `.10.27–.10.29`).
- Initial review limitation, superseded by the resumed review below: only 18/189 bodies had complete recovery evidence; archived bulk reads were tool-elided. The later per-record review closes that reading residual, not the product's acceptance gates.
- I01–I15 all have live label owners: I01 `.3.12/.5.14`; I02 `.6.14–.6.16/.6.27`; I03 `.2.8/.7.4–.7.7/.7.10`; I04 `.7.1/.7.2/.7.8/.10.10/.10.30/.10.31`; I05 `.5.16/.5.17/.7.9`; I06 `.4.11/.4.12/.5.15`; I07 `.10.1/.10.1.1`; I08 `.10.2/.10.21`; I09 `.3.14/.3.15/.6.6`; I10 `.10.3/.10.4/.10.22`; I11 `.5.1/.5.17/.9.3/.9.4`; I12 `.6.24/.6.28`; I13 `.1.6/.4.13/.7.11/.10.8/.10.27–.10.29`; I14 `.6.13/.8.5`; I15 `.6.12/.6.26`.
- A new `.1.15` (in_progress, SilentFinch) owns the schema-independent adversarial runner-certification slice; it explicitly excludes matrix ownership and preserves `.1.12`'s remaining requirements. Do not duplicate it or convert it into whole-matrix proof.
- Initial disposition (historical): no missing roadmap bead was established, but exhaustive review was then incomplete. The nonexistent-reference validator defect was repaired and verified, not all evidence-integrity gaps. An isolated archive of commit `02c08a32301591a98988f27d66da2e0bd2d518da` passed `python3 -I -B scripts/test_contract_matrix.py` (10 tests) and `python3 -I -B scripts/validate_contract_matrix.py` (69 boundaries, exit 0). These checks establish source-reference validation, not execution of every referenced test or product acceptance. At that snapshot, `.1.9`'s closure remained disputed and `.1.12`, `.1.10`, and `.1.11` remained open; later snapshot verdicts below supersede those status observations. Completing work with genuinely satisfied external evidence—not exit zero, synthetic arithmetic, or elapsed trial time—is what closes the scoped vision.
- Resumed full acceptance review (2026-09-17, snapshot sha256 `e3b81e5f…` of the git-tracked `.beads/issues.jsonl` export; 195 rows: 189 roadmap records + root epic + `sr-eval-audit-dwew` + 4 peer repair records): **every record individually reviewed** against README/AGENTS/plan requirements via elision-proof per-record Markdown renders (renderer fidelity verified: zero content failures across all 195 rows under whitespace-stripped comparison). Verdicts: 20 OK, 4 CONTRADICTION, 4 STALE, 1 GAP, 166 NEEDS-PROOF. NEEDS-PROOF dominates open product beads with unchecked acceptance criteria — that is the expected disposition of unaccepted obligations, not a defect.
- Snapshot findings: **CONTRADICTION** `.1.5` (effective-policy receipt acceptance versus narrower P0 closure evidence), `.1.9` (comment-73 closure dispute lacks explicit reconciliation), and `.5.16`/`.5.17` (inherited documentation-only wording conflicts with the reviewers' then-current finished-product instructions). **STALE**: root epic's present-tense unstarted-work claim, plus documentation-only wording in `.7`, `.7.9`, and `.8.5` (not `.7.13`). The current user instructions retain finished-product README voice; this engineering assessment records actual implementation status separately. **GAP**: the P2 handoff omits existing child `.3.18`, consumed by `.3.13`; this is a handoff omission, not a missing implementation owner. `.2.4` additionally needs revision-bound provenance for its recorded 8/8 endpoint tests.
- Durable evidence: [complete acceptance-review payloads](reality-check-acceptance-review.json) preserve all five reports, per-record evidence, snapshot render hashes, totals, and integration corrections. All 195 snapshot IDs occur in the reports; all six P9 deferrals have qualification owners. Review findings identify no missing roadmap implementation owner. This is complete snapshot review, not fresh runtime verification or certification of subsequent tracker changes.
- Tracker delivery follow-up: all seven formerly undelivered dispositions are now verified through `br comments list --json`: `.1.5` comment 120, `.5.17` 121, P2 parent `.3` 122, root epic 123, hook parent `.7` 124, onboarding `.7.9` 125, and value report `.8.5` 126. Each expected comment matched exactly once. `br sync --flush-only` reported no dirty issues. These are scope/wording/handoff dispositions, not acceptance or closure of product work. The I05 comments reflect the instructions available to this review; any later direct README-voice instruction takes precedence without turning this audit into runtime proof. No direct database writes, lock removal, or acceptance-criterion changes were used.
