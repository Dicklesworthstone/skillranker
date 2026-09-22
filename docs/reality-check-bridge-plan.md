# SkillRanker reality check and bridge plan

Latest assessment: 2026-09-21 UTC, second executable-journey pass at `a82abbb`, plus a 2026-09-22 execution addendum (beads closed, shadow deployment live). Earlier reviews and receipts are retained below as
history. Inventory and ownership statements describe their stated snapshots, not
a frozen release or a product-completion percentage.

## Current assessment — 2026-09-21 UTC, second executable-journey pass (plus 2026-09-22 execution addendum)


**The two defects the morning review filed that had owners free have been fixed and are verified
closed in this pass; the two strategic data-acquisition beads it filed remain the critical path.
The central claim is still unproven: nothing here shows a SkillRanker recommendation makes agent
work better.** The measurement apparatus is now demonstrably connected end to end — this review
re-ran the entire documented journey against the TLS fixture at `a82abbb` and every stage behaved
as documented — but `gate_status` still reads `not-established`, and correctly so.

### Scope and evidence boundaries

Read all of AGENTS.md and README.md in full, the plan's promotion gates and implementation order,
and the morning review. Built `sr` locally (RCH artifact transfer failed with exit 102 on both
hz2 and ovh-a; `RCH_CARGO_WRAPPER_BYPASS=1` local build used instead) and drove it in an isolated
owner-only HOME with a three-skill workspace and a synthetic Claude session, against the project's
own TLS fixture provider. Assessed revision `a82abbb` (origin/main); the tree moved 38 commits
during the review, and every behavior claim below was re-verified against a binary rebuilt at
`a82abbb` after the move. The provider proves the pipeline over genuine TLS through the production
transport, decoder and admission path; it proves nothing about Jev's answer quality.

### Gates run at `a82abbb`

| Gate | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --locked --all-targets -- -D warnings` | clean |
| `cargo test --locked --no-fail-fast` | **1,236 passed, 0 failed, 10 ignored** |
| `python3 scripts/validate_public_contracts.py` | passed |
| `ubs --diff` | anomaly: reported "no supported languages detected" in a Rust repo, exit 3; not a pass, not a failure finding |
| RCH remote builds | broken today (exit 102, artifact transfer after successful remote compile, twice); local cargo is the working gate |

The one suite failure observed today (`a_late_rerank_answer_is_never_published`) appeared in two
full-suite runs while host load average exceeded 110, then passed 3/3 in isolation. It asserts an
owned runtime shuts down inside the remaining deadline budget; under scheduler starvation the
assertion trips. This is the same shape as open `sr-5n0b` (concurrent runtime construction
exhausting subprocess test budgets); a deadline-sensitive suite that cannot run green on a loaded
shared box is a real CI-robustness gap, not a product defect found today.

### Journey re-driven, end to end (all at `a82abbb`, all observed by this reviewer)

`ledger init` → `rank --allow-network` over loopback TLS: `ranked`, one suggestion
(rust-test-triage), 2 logical requests / 2 HTTP attempts, 275 known tokens, `persistence: recorded`
→ `observe` attributed the session after the transcript filename was made to match its `sessionId`
(durable identity requires stem == sessionId plus a `cwd` record for this workspace — two misnamed
fixtures were refused with `missing-session`, correctly) → `feedback` committed → `stats --by-skill`
reported every documented cohort → `--save-case` then `replay` reproduced `ranked` with 2/2 stages
and `actionable: false` → `eval --dataset` replay returned `kind: report`, `run_status: complete`,
`gate_status: not-established`. Local-only paths: `--require-skill` resolved offline with zero HTTP
attempts; `--dry-run` produced a `preview` envelope with provider request and `disclosure` receipt;
an offline rank over the cached inputs returned `ranked` with zero requests, and the same command
with a different endpoint identity correctly missed (exit 11); roster snapshot/diff round-tripped;
prune preview computed a sane 30-day cutoff; hook in shadow emitted zero bytes with exit 0 and
recorded a separate shadow-channel turn; malformed hook input produced empty stdout, sanitized
stderr, exit 0; `install-hook claude` previewed an exact settings diff.

Two contract-honoring refusals worth recording: an advisory-mode hook with network consent emitted
nothing and logged `native advice disabled for unverified harness (UnverifiedHarness)` — the
publication boundary now enforces harness qualification, superseding the historical review's
finding 2. And a group-accessible ancestor directory makes `ledger init` refuse, as documented.

### Status of the morning review's six beads

| Bead | State at `a82abbb` | Evidence from this pass |
|---|---|---|
| `sr-an94` (late tool-result upgrade) | **Fixed, verified live** | tool_use pass records `attempted`; a later pass seeing only the result upgrades to `loaded` (session-d experiment; control shows `is_error: false` required for recognition) |
| `sr-x4s8` (planned flags refuse generically) | **Fixed, verified live** | `eval --sample-size/--seed/--explain` and `doctor --descriptions` now refuse naming their phase; `capabilities --json` exposes `planned_flags` |
| `sr-2wda` (attempt identity collision) | Closed | not re-exercised |
| `sr-oufi` (judgment keyed by invocation name) | **Still live on main** | `feedback u3 --skill rust-code-review` stores the invocation name in `judgments.skill_id` while `ranking_candidates` holds the stable id; `stats --by-skill` splits one skill into two rows. Fix exists on unmerged peer branch (`6d2c2b5`) |
| `sr-488b` (owner-only refusal names no path/mode) | **Still live** | `ledger init` under a group-accessible ancestor says only "cache path does not satisfy owner-only permissions" |
| `sr-uv2v` / `sr-1uf4` (corpus and shadow traffic) | Open | unchanged; still the critical path to every promotion gate |

### New findings this pass

1. **The installed hook's outer timeout equals the inner deadline instead of exceeding it
   (NO_BEAD before this review).** `DEFAULT_HOOK_TIMEOUT_SECS = 3` (src/installer.rs:32) against a
   3,000 ms internal deadline that starts at process entry; README promises a harness timeout
   "above the ranker's deadline" and says it "is initially four seconds." Because startup precedes
   the internal clock, the harness can kill `sr` during its 200 ms output reserve. Filed as
   **`sr-83cc`**.
2. **The circuit breaker is plumbing without a producer.** `AdmissionRefusal::ProviderCooldown`,
   `RankingStage::Probe`, exit-4 mapping and ledger exclusions all exist, but nothing in src/
   constructs a cooldown or issues a probe (src/jev/retry.rs disclaims owning persistent cooldown).
   Honest staging — `sr budget` is planned-P6 and `.7.7` owns the implementation — but anyone
   reading the types could mistake them for function. No new bead; `.7.7` covers it.
3. **`sr eval --online` is a clean refusal, not a capability.** "live batch execution is not
   implemented; supply recorded replay cases in offline mode" (src/evaluation/batch.rs:265).
   Owned by `.6.19` (in progress). Fine as staged; worth remembering the README flag table's live
   path is not yet reachable.
4. **`sr-jgez` (peer-filed during this review) is real and matters for hooks:** `sr observe`
   re-reads transcripts from byte 0 every pass, so observation cost grows with session length on
   the per-turn hook path.

### What this means for the bridge

The morning review's ordering survives with its first row half-complete:

| Order | Work | State after this pass |
|---|---|---|
| 1 | `sr-oufi` (+ merge the branch fix), `sr-an94` | an94 done; oufi is one reviewed merge away |
| 2 | `sr-1uf4` self-hosted shadow deployment | unchanged — still the cheapest source of the operational cohort, and now additionally the only realistic soak for `sr-jgez`-class observation costs |
| 3 | `sr-uv2v` adjudicated corpus pilot | unchanged |
| 4 | `.6.20`–`.6.28` evaluation/reporting proof suite | unblocked by nothing new; `sr-x4s8` done removes the flag-honesty objection |
| 5 | `.7.11`, `.7.9`, `.8.9`/`.8.10` harness qualification and trials | the publication boundary now enforces qualification (verified today); the trials themselves remain |
| 6 | `.8.1`–`.8.6`, `.7.12`, `.8.4` promotion cohorts | still the only row that answers whether the product works; still gated on rows 2–3 |
| 7 | `.7.4`–`.7.8`, `.7.10` allowance/breaker/snooze | breaker currently surface-only (finding 2); also a prerequisite for spending real budget on row 2 |
| 8 | `.5.23`, `.8.7` release | still hostage to macOS CI infrastructure, not beads |
| 9 | `sr-x4s8`, `sr-488b`, `sr-83cc` | x4s8 done; 488b and the new timeout bead are cheap and user-visible |

Backlog health: ~265 issues, no dependency cycles, 19 actionable of 88 unresolved; the longest
chain (29 beads) still roots at `.6.7`. No vision goal has zero bead coverage after `sr-83cc`;
the weakest coverage is macOS release qualification (one infra-blocked bead) and deferred
non-Claude adapters.

The one-sentence answer to "are we there yet": **the pipeline works and is honest about its own
limits; the product claim (Jev-ranked suggestions improve agent work) has excellent instruments
and no readings yet — rows 2 and 3 are the entire ballgame.**

This update revises the existing bridge in place. Its current-state claims retire when the next
revision-bound assessment supersedes them. The sections below are historical snapshots and must
not be read as the current implementation inventory.

### Execution addendum — 2026-09-22 (ProudBison)

Acting on the ordering above, in one working session:

| Item | Outcome |
|---|---|
| `sr-83cc` hook outer timeout | **Closed.** Default now 4 s with strict deadline-plus-reserve validation; the hook's clamp budget derives from the installed entry instead of a hardcoded 3000 ms. Verified live; 14/14 installer tests. |
| `sr-488b` owner-only refusal | **Closed.** Peer implementation verified live: refusal names the offending path, actual mode, the rule, and the exact `chmod` remedy. |
| `sr-oufi` judgment identity | **Verified** at 39c6d3b (fix merged): stable-id judgments join correctly; name lookups without a roster snapshot refuse honestly. Closure left to owner. |
| `sr-uv2v` relevance corpus | **Engineering half delivered** (`16f75cc`): pre-registered rubric/frame/split contract in `docs/relevance-corpus-contract.md` plus the mechanical corpus validator (`scripts/validate_corpus.py`, 18/18 tests). Remaining is maintainer adjudication + the pilot, best fed by shadow traffic. |
| `sr-1uf4` self-hosted shadow | **DEPLOYED** (receipt in `docs/self-host-shadow-deployment.md`). The smoke exposed a fatal chain: the hook could not read any real current-version transcript (sr-j4k9 — unmodeled harness records fatal, thinking-only blocks fatal, sidechain leaves unresolvable, metadata-only system records fatal, lineage shattered through attachment nodes). All fixed and verified live; a real two-stage Jev evaluation completed over the real provider. Without this attempt, shadow traffic would have recorded nothing while looking healthy. |
| `sr-5n0b` deadline-test flake | **Closed.** Recurring full-suite failure traced to near-expiry drain windows; tests now use `shutdown_within` with a fresh bounded window. Full suite **1318/1318/0 twice**, second run under load ~81. |
| New defects filed | `sr-e8vm` (deduplicated redelivery accounting: real spend attaches to an unavailable-looking event row), `sr-ksjn` (a test wrote into the operator's real ledger). |

Gates at `ae5d101`: full suite 1318 passed / 0 failed / 10 ignored (twice),
fmt and clippy `-D warnings` clean, `validate_public_contracts.py` passed.
RCH artifact transfer remains broken (local cargo used throughout).

## Historical assessment — 2026-09-21 UTC, executable-journey review


**The reporting and evaluation seam the previous review called the bottleneck is now
built and connected. `sr stats` and `sr eval` exist, the whole P5 journey runs end to
end over a real HTTPS round trip, and the full suite passes 1,180 tests at this
revision. The central claim is still unproven: nothing here shows that a
SkillRanker recommendation makes agent work better.** What changed since the previous
review is the apparatus for measuring usefulness, not the evidence of it. TypeSafe.ai
Jev remains the essential ranking engine; a fixture provider, offline replay and
synthetic demos are not a key-free inference backend.

This review drove the product rather than reading it, and that turned up four defects
the previous reading did not, three of them inside the newly delivered reporting
surface. Two have no existing owner and are now tracked as `sr-oufi` and `sr-an94`; a
third refusal-contract gap is `sr-x4s8`.

### Scope and evidence boundaries

Read all 674 lines of AGENTS.md and all 2,034 lines of README.md in full, then worked
from the code and the running binary rather than from either document. Verified source
revision `16743e9` (`origin/main`), built `sr` from that content, and exercised it in
isolated private homes with no credential and no network consent beyond an explicit
`--allow-network` against a local fixture endpoint. Every figure below is from a
command this review ran; nothing is carried over from a peer report or a previous
snapshot. Findings that contradict the previous section are marked as superseding it.

The provider in the journey is the project's own TLS fixture
(`tests/fixtures/jev-tls/provider_server.py`), not TypeSafe. That choice proves the
pipeline over a genuine TLS round trip through the production transport, decoder and
admission path without spending provider budget or sending anything off the machine.
It proves nothing about the quality of Jev's answers, and no latency figure here is a
provider benchmark.

### What the binary actually implements

`sr capabilities --json` reports 16 implemented commands — `help`, `version`, `rank`,
`hook`, `roster`, `doctor`, `capabilities`, `demo`, `install-hook`, `uninstall-hook`,
`stats`, `observe`, `feedback`, `replay`, `eval`, `ledger` — and five planned:
`snooze` and `budget` (P6), `calibrate` (P8), `tui` and `gaps` (P9). `features` reports
`tui: {compiled: false, implemented: false}`.

The five planned commands each refuse with exit 2 and a message that names the phase
and points at the registry: *"This build does not implement that command; it is planned
for phase P6. Run sr capabilities for command status."* That is the AGENTS.md
requirement working as intended, and it is the standard the flag surface fails to meet
(see finding 3).

Supersedes the previous section: `sr stats` and `sr eval` are no longer absent.

### The documented journey, end to end

One run, one workspace of three real skills on disk, one discovered Claude session,
one ledger:

| Step | Result |
|---|---|
| `ledger init` | new store, schema 1; `ledger status` then reports **ready**, not `needs_migration` |
| `rank --allow-network --json` | `ranked`, one suggestion, 2 logical requests / 2 HTTP attempts, 275 known tokens, `persistence: recorded`, 483 ms |
| warnings on that decision | `discovered-session`, `unverified-visibility`, `source-not-enumerated` — all three honest and all three documented |
| `observe --transcript --harness claude_code` | observation recorded and **attributed** to the ranking event, keyed by the stable skill id |
| `feedback EVENT --skill ID --verdict useful` | judgment committed |
| `stats --json --by-skill` | every documented cohort present: turns, latency with `excluded_unfinished`, provider attempts and known tokens, judgments, observations, per-skill rows |
| `rank --save-case` then `replay CASE` | `kind: replay`, `actionable: false`, reproduced the historical `ranked` decision |
| `eval --dataset D --json` | `kind: report`, `run_status: complete`, `gate_status: not-established`, completeness accounting for cases and stages, `loss_summary` |
| `hook claude`, `install-hook claude` | both exit 0; hook defaults to shadow |

Local-only paths verified separately: explicit `--require-skill` resolved offline with
**zero HTTP attempts**; `--dry-run` produced a `preview` envelope with
`actionable: false`, a real redacted provider request naming both candidates, and a
disclosure receipt with per-category included/omitted/redaction counts;
`--context-profile minimal` produced its own bounded receipt; `roster --snapshot` and
`--diff` round-tripped; all four `ledger` subcommands previewed correctly.

Two refusals worth recording as correct rather than as defects. A group-writable state
directory is refused with *"cache path does not satisfy owner-only permissions"* until
the whole ancestor chain is private — the product enforcing its own documented
owner-only rule. And piped stdin without an explicit input mode is refused, as the CLI
contract requires.

`sr ledger prune --before 30d` now resolves a cutoff of `2026-08-22T13:54:15.910Z`.
The previous review did not cover prune; it computed a 1969 cutoff until `9b77a8f`, so
retention was never enforced through the CLI. That repair is in this revision.

### Findings that change the next work

1. **A judgment is stored under the invocation name, not the stable skill id.**
   Observed directly in the ledger: `judgments.skill_id = 'rust-code-review'` while
   `ranking_candidates.skill_id = 's_6d9c784e05bfd6ba…'` for the same skill, with the
   event's roster snapshot already carrying the mapping. Observations key correctly, so
   the judgment write is the sole outlier. `stats --by-skill` therefore splits one skill
   into two rows — recommendations on one, the label on the other — and per-skill
   usefulness can never be joined to per-skill recommendation volume, which is the
   entire purpose of that view. It also breaks P8 priors before they are built, since
   those are specified as keyed by skill revision. Violates the AGENTS.md identity
   invariant. **No existing bead named it; now `sr-oufi`.**
2. **An attempted load is never upgraded to observed when its tool result arrives in a
   later ingestion pass.** With both records present in one pass the state is `loaded`.
   With the tool_use read first and the result appended afterwards, the state is
   `attempted` and a second `observe` cannot repair it, because the cursor has advanced
   past the tool_use and the result has nothing to associate against. `sr stats` then
   reports `observed_loads=0` and `suggestion_adoption_rate=0.0` for a skill the agent
   did load. Periodic observation is the documented usage and the hook runs per turn, so
   this is ordinary, one-directional, and invisible in the output: `attempted` is a
   legitimate state, so nothing distinguishes *could not confirm* from *saw the try and
   discarded the confirmation*. **No existing bead named it; now `sr-an94`.**
3. **Planned flags refuse generically while planned commands refuse well, and one
   planned flag is accepted and inert.** `sr eval --explain` returns output
   byte-identical to `sr eval` — 710 bytes, same keys, same records — while README
   promises explanation cards in four separate places. `sr doctor --descriptions` and
   `sr eval --sample-size/--seed` return *"Unsupported or conflicting arguments; use
   --help"*, telling a user who followed README that their syntax is wrong rather than
   that the feature is planned. The mechanism to do better exists and is empty:
   `PLANNED_RANK_FLAGS` is `&[]` with a doc comment describing exactly this purpose. A
   silently inert flag is worse than a refusal, because the user believes they received
   the explanation. Features owned by `.6.25`, `.10.3` and `.6.22`; the refusal contract
   is now `sr-x4s8`, and each owner carries a note.
4. **The usefulness question is untouched by any of this.** The apparatus improved
   substantially — ten defects were repaired in the reporting surface at this revision,
   six of them one shape: a figure derived from partial data inheriting the partiality
   silently. But `gate_status` on a real report reads `not-established`, and that is
   correct. There is still no relevance holdout, no paired-harm cohort, no
   500-invocation operational cohort, no measured latency percentiles against a real
   provider, and no released binary. Better instruments have not produced a reading.
5. **Nothing owns producing the two inputs the promotion gates consume.** This is the
   largest gap in the backlog and it is strategic rather than technical. `.8.1` is
   titled *execute* the relevance holdout and assumes the holdout exists; `.8.4`
   validates fallback over 500 representative hook invocations and assumes that traffic
   exists. `.8.8`, which gates on "at least 300 adjudicated primary cases", explicitly
   disclaims the work — "do not hold the usable CLI hostage to … corpus mining" — and no
   other task picks it up. So the project has built excellent measurement apparatus with
   no data-acquisition plan for either of the things it measures. **Now `sr-uv2v`
   (the adjudicated corpus) and `sr-1uf4` (self-hosted shadow traffic).**
6. **The cheapest source of both inputs is this repository.** SkillRanker's own
   development environment is its target environment: a skill-dense workspace worked
   continuously by coding agents through the exact Claude `UserPromptSubmit` hook the
   adapter implements. A shadow deployment here would generate the operational cohort as
   a byproduct of ordinary work, and — because shadow mode injects no advice and
   therefore cannot influence agent behaviour — the same sessions can supply the real
   rosters and moments that relevance cases need, with labels still coming from
   independent adjudication. That composition is what makes the corpus tractable: it
   removes the expensive half without introducing circularity. Nothing in the backlog
   exploited this before now.
7. **One first-contact diagnostic misdirects.** `sr ledger init` under a group-writable
   ancestor refuses correctly but names neither the offending path nor the required mode,
   and its hint says to inspect configuration when the fix is a `chmod`. Locating it took
   four attempts. Contrast `sr stats` with no store, which says *"run sr ledger init
   first"* in the message itself — the affordance the permission refusal should copy.
   `ledger init` is step 6 of the Quick Start and the gateway to every P5 feature.
   **Now `sr-488b`.**

### Checks and what they establish

- Full suite at `16743e9`, this reviewer's own run: **126 reports, 1,180 passed, 0
  failed, 10 ignored**. Establishes that the tree is green at the assessed revision; it
  does not establish product quality, and 10 ignored cases remain ignored.
- `cargo fmt --check` clean, `clippy --locked --all-targets -- -D warnings` clean,
  `validate_public_contracts.py` passed, `ubs --diff` reporting one critical that
  predates this work in a test fixture. Gate receipts for the revision, nothing more.
- The journey table above. Establishes that the documented P5 loop runs end to end
  against a real HTTPS provider. Establishes nothing about recommendation relevance.
- The four findings, each from a command this review ran and a ledger row it read.
- `br dep cycles`: none. Beads at review time: 251 issues, 82 open, 13 actionable, 69
  blocked, 7 in progress. Not a completion percentage.

### Existing backlog coverage and the shortest useful bridge

The previous review's conclusion mostly holds: the documented *feature* scope has owners
and duplicating it would be waste. It was wrong in one respect, and that correction is
this review's most consequential output — the two inputs the promotion gates consume had
no owner at all, which a survey of feature coverage could not surface.

Six beads were added: three for defects with no owner (`sr-oufi`, `sr-an94`, `sr-488b`),
one for a refusal contract (`sr-x4s8`), and two for the data-acquisition gap (`sr-uv2v`,
`sr-1uf4`). Findings belonging to existing tasks were recorded as notes on `.6.25`,
`.10.3` and `.6.22` rather than re-filed. No epic was duplicated; each new bead was
checked against the existing backlog before creation.

The ordering below replaces the previous section's list, whose first two rows are now
done:

| Order | Work | Concrete exit evidence |
|---|---|---|
| 1 | `sr-oufi`, `sr-an94` | One joint case: rank, observe across an ingestion boundary, judge by name, then `--by-skill` shows a single row with top-1, load and label together. Fails twice over today. |
| 2 | `sr-1uf4` | A declared population, consent and retention boundaries settled before installation, a verified uninstall, and real shadow traffic with per-stratum fallback rates and p50/p95/p99. |
| 3 | `sr-uv2v` | A 15–25 case pilot under a pre-registered rubric giving a measured per-case cost and a disagreement rate, then either the full corpus or an explicitly narrowed population justified by that pilot. |
| 4 | `.6.20`, `.6.22`, `.6.25`, `.6.28` | A labelled cohort produces a report whose numbers a reader can recompute, with sampling flags that exist and explanations that differ from their absence. |
| 5 | `.7.11`, `.7.9`, `.8.9`/`.8.10` | Hook qualification enforced at the publication boundary, then a real installed Claude receiving bounded advice, with default shadow and denied scope staying quiet. |
| 6 | `.8.1`–`.8.6`, `.7.12`, `.8.4` | The relevance holdout, paired-harm cohort and operational cohort run against their prespecified gates. This is the only row that can answer whether the product works. |
| 7 | `.7.4`–`.7.8`, `.7.10` | Durable shared allowance and scoped snoozes, still absent commands. The allowance is arguably a prerequisite for row 2 rather than a parallel task, since shadow traffic spends real provider budget. |
| 8 | `.5.23`, `.8.7` | A signed artifact from a frozen accepted revision, installed and used per claimed target. |
| 9 | `sr-x4s8`, `sr-488b` | Every documented flag either works or names its phase; the owner-only refusal names the path and the mode. |

Rows 1, 9 are cheap and make everything else legible. Rows 2 and 3 are the ones that
changed in this review: they are now the critical path to row 6, because row 6 cannot
start without them and nothing previously owned them. Row 6 is the product. More scoring
features, the TUI, or additional statistical machinery do not substitute for row 6, and
this review found no evidence that anyone is confusing the two.

One ordering consequence worth stating plainly: rows 2 and 3 compose in one direction
only. If the shadow deployment runs first, the corpus pilot can draw real cases from real
traffic and its cost estimate transfers to the full corpus. If the pilot runs first
against constructed fixtures, the estimate does not transfer. Sequence accordingly.

This update revises the existing bridge in place. It adds no runtime capability and
closes no gate. Its current-state claims retire when the next revision-bound assessment
supersedes them. The sections below are historical snapshots and must not be read as the
current implementation inventory.

## Historical assessment — 2026-09-21 UTC, delivered-workflows review

**SkillRanker has a real, connected core CLI and substantial local evidence
machinery. It has not yet demonstrated that its recommendations reliably improve
agent work.** That distinction is now more important than whether the executable
starts. The next valuable work is durable attempt accounting, usable evaluation
and reporting, and an actually qualified harness trial. Additional scoring
features, the TUI, and more statistical machinery should not displace those jobs.
TypeSafe.ai Jev remains the essential ranking engine. Local explicit resolution,
offline replay and synthetic demos do not supply a key-free inference backend.

### Scope and evidence boundaries

This review read all 674 lines of AGENTS.md, all 2,034 lines of README.md and the
1,367-line comprehensive plan; checked the existing G01–G70 and I01–I15 crosswalks;
traced CLI dispatch, ranking, provider admission, hook publication, capabilities,
storage and evaluation callers; inspected tests and qualification records; and
consulted the active owners through Agent Mail. Initial inventory was at
`c62d526`; the later frozen Linux verification target is `3edff71`. Shared main
and peer working files continued changing. No peer source changes are included
in this assessment's edits.

The current source registry implements `rank`, `roster`, `doctor`, `capabilities`,
`demo`, `ledger`, `observe`, `feedback`, `replay`, `hook`, `install-hook` and
`uninstall-hook`, in addition to help/version. This is a command-path inventory,
not acceptance of every behavior or a production-advisory qualification.

Direct macOS execution used the already-installed arm64 development binary,
SHA256 `cd6b248074fc93cf3ac122d6b8aa61ee9696bd7e11659fdbdf0dc505aa00f796`,
in a new private synthetic home/workspace, with no credential and no network
consent. It is an older build, not a build of `3edff71`. The retained remote
receipt is `/Users/jemanuel/sr-reality-g4d6uqdi/receipt.json` on `mac-mini-old`.
Its four demos completed with explicitly synthetic, non-actionable envelopes;
doctor and roster ran; ledger init/status ran; explicit `--require-skill`
resolution returned the requested local skill with zero HTTP attempts. An
ordinary offline request without a complete cached result returned exit 11,
not a fabricated abstention. Malformed hook input produced empty stdout and
exit zero. The receipt does not establish real Claude delivery or Jev relevance.

### Current vision-to-code map

Statuses apply to the stated boundary. WORKING is not a whole-product claim;
PARTIAL includes substantial library implementation without the promised CLI.
The goal numbers refer to the full requirements crosswalk retained below.

| Goals | Current reality | Status / remaining ownership |
|---|---|---|
| G01–G08: foundations and public contracts | One Rust package, forbidden unsafe roots, pinned Asupersync/Quill, bundled SQLite, typed authority and bounded formats are real. Capabilities distinguishes planned commands. Old engineering narratives and parts of README lag delivery. | WORKING core; documentation reconciliation remains part of acceptance. |
| G09–G12: transport, validation, deadlines | Production transport, retry/admission, strict decoding and cancellation tests exist. Historical live maximum-shape Jev pair passed through the production decoder. This is not a current availability or latency cohort. | WORKING tested boundaries; operational qualification G59 remains UNPROVEN. |
| G13–G20: visible roster, explicit requests, redaction, Quill, drift | Connected discovery/import, identity/collision handling, bounded redaction, deterministic retrieval, revalidation and roster snapshot/diff. Native visibility is still marked unverified. | WORKING local paths; native harness authority is PARTIAL pending `.7.11`. |
| G21–G27: exact context and Cass | Source selection, native/normalized input, prompt overlay, task anchors, disclosure and Cass production adapter are connected. Older claims that Cass is always refused are superseded. | WORKING scoped paths; real installed-harness/version qualification remains separate. |
| G28–G36: core recommendation and inspection | Wide/rerank pipeline, per-candidate none/fit eligibility, scoring, exact cache/coordination, explanations, dry-run, demo and doctor have production callers and tests. | WORKING core; no inference-quality certificate follows from synthetic or local-server responses. |
| G37–G40: durable events, observations and usage | Real SQLite ledger lifecycle and `observe` exist. Provider attempts currently settle with ranking evidence; hard-kill accounting is still being repaired by the owner. Fresh-store status was misleading in the audited binary; a later source fix landed during review. | PARTIAL: `.6.7`, `.6.26`; `sr-pnt3` repair `32d17b6` requires its own verification. Do not erase unknown paid attempts. |
| G41–G43: capture, replay, corrective feedback | `--save-case`, offline replay/policy comparison, and historical feedback are connected. Replay has a checked-in executable suite. | WORKING paths, with phase acceptance `.6.29` still open. |
| G44: useful operational/value reports | Durable source data exists; `sr stats` is absent. No operational reporting workflow yet exposes all promised cohorts, unknowns and costs. | PARTIAL: `.6.13`, waiting on `.6.7`. |
| G45–G48: evaluator, metrics, robustness and sampling | Frame/label ingestion, copied narrow numerical primitives, sampling and weighted-bound code exist. No production CLI caller of the evaluation module was found; `sr eval` is absent. | PARTIAL: `.6.19`–`.6.25`, `.6.28`; library tests are not an evaluation product. |
| G49–G51 and G56: Claude hook and onboarding | Shadow wrapper, bounded advisory rendering and managed installer/uninstaller exist. Capabilities lists no tested Claude version; all native conformance dimensions remain not evaluated. The final hook path does not consult the native-advice eligibility method. | PARTIAL: `.7.9`, `.7.11`, `.7.13`, controlled trial `.8.9`/`.8.10`. |
| G52–G55: durable allowance, breaker and snoozes | Per-invocation attempt bounds exist. `sr budget` and `sr snooze` do not. Do not confuse a per-run counter with durable shared enforcement or trusted scoped controls. | NOT DELIVERED: `.7.4`–`.7.8`, `.7.10`. |
| G57–G60: relevance, harm, latency and rollout | Contract fixtures specify the required cohorts. No completed primary relevance holdout, paired-harm cohort or representative operational cohort was found. | UNPROVEN: `.8.1`–`.8.6`, `.7.12`; public promotion `.8.8` remains open. |
| G61–G65: calibration, learned priors and monitoring | Policy/numerical foundations are not an end-to-end learning lifecycle. `sr calibrate` is absent; no completed held-out improvement or prospective monitor acceptance was found. | NOT DELIVERED: P8 `.9.1`–`.9.9`. |
| G66–G70: later experiments, TUI, native adapters | `tui = []` is a reserved empty feature, with no FrankenTUI dependency or UI. `gaps` is absent. Additional native adapters and retrieval experiments retain separate tasks and gates. | NOT DELIVERED: P9 `.10.*`; do not hold the useful core behind them. |
| Distribution and native platforms | Native arm64 APFS execution and substantial revision-bound tests exist; Intel execution used Rosetta. GitHub release listing is empty. | PARTIAL: `.5.23`, `.8.7`; no signed public binary release, physical Intel or HFS+ behavioral qualification. |

### Findings that change the next work

1. **The useful-feedback loop is the bottleneck.** Storage and replay can preserve
   cases, but there is no `eval` command to turn an independently judged cohort
   into the promised comparison, and no `stats` command to inspect usage and
   outcomes conveniently. `src/evaluation.rs` and its four child modules are
   useful implementation, not proof that those workflows ship. The numerics are
   narrow copied implementations with provenance, not imported whole scientific
   workspaces; lack of those dependencies is not itself a defect.
2. **Hook qualification is descriptive rather than enforced at publication.**
   `AdapterRecord::advice(EmitNativeAdvice, installed_version)` contains the
   qualification logic. The production hook path refreshes trusted mode and
   proceeds to `render_claude_hook` without calling it. All Claude qualification
   cells are unevaluated. Fix the runtime boundary and give the existing
   experimental-trial path explicit scope; a warning or a capability row alone
   does not enforce the promised rule. Preserve explicit user directives and
   default shadow behavior. Notify the hook owner rather than collide with its
   active CLI edits.
3. **Hard termination can lose paid-attempt evidence.** The ledger owner confirmed
   that settlement-time writes do not cover a process killed during an in-flight
   request. `.6.7` remains open for progressive recording. Normal error-path
   tests, cache follower tests and unknown-token fields do not close this case.
   Required proof includes killing a reached live attempt and recovering an
   unknown-cost record without inventing a successful ranking or duplicate charge.
4. **New-store status is misleading.** The isolated Mac run reproduced
   `ledger init` → schema 1 / created, immediately followed by `ledger status` →
   needs_migration / target 2. Both exited zero. During this review the owner
   landed `32d17b6`: usable schemas report ready, with `upgrade_available` kept
   separate. The source diff and new lifecycle tests were inspected, but this
   later commit is outside the frozen test run and installed binary used here.
   The owner subsequently reported RCH migration/retention checks and a replay
   suite with 109 passing tests (Agent Mail 42448); this audit did not independently
   execute that later binary. No destructive store repair is warranted.
5. **macOS is no longer an absent implementation.** The open `.5.23` description
   still describes Linux-only exports/storage. Keep its broader release-matrix
   acceptance honest, but update its current-state facts using native receipts.
   README also says both “Linux or macOS” and “supported local platform is Linux”;
   its architecture table names former sketch paths. These are factual drift,
   not grounds for adding a design-stage banner.
6. **No evidence yet answers the central usefulness question.** The checked-in
   `tests/eval/README.md` explicitly excludes the 300-family relevance holdout,
   controlled harm cohort and 500-invocation operational cohort. A live maximum
   request-shape pass proves interoperability, not that selected skills help.

### Checks and what they establish

- Historical provider receipt: `docs/jev-contract-spike.md`, source `99de43e`,
  one synthetic live pair, 254-real-option wide capacity and maximum detailed
  shape; 685 ms wide and 628 ms rerank, two attempts. The shortlist was fixed for
  codec qualification. Do not report this as an end-to-end ranking benchmark,
  a latency percentile, or held-out relevance evidence.
- Historical native receipts: `docs/installation.md`, full arm64 suite at
  `6b5d17c`: 1,001 passed, zero failed, 10 ignored; 129 focused Linux portability
  passes; later `8523ab6`: 88 native library/coordination/storage passes and 27
  hook/installer passes. These remain tied to those revisions.
- Fresh native full-suite attempt at `c62d526`: RCH refused before compilation
  with `RCH-I006 missing_runtime`; no local fallback was allowed. SSH confirms
  the pinned nightly is installed, so this is unresolved worker qualification,
  not evidence of a Rust compilation failure. Log:
  `/data/tmp/sr-reality-20260921-full-native.log`.
- Fresh Linux full-suite attempt at frozen `3edff71` downloaded dependencies,
  then waited on the shared worker's Cargo build-directory lock. After seven
  minutes, only this audit's build `30030202658619464` was cancelled through RCH,
  releasing its slot rather than adding to peer build contention. No test
  summaries were produced. The retained log is
  `/data/tmp/sr-reality-20260921-linux-full.log`. There is no fresh full-suite pass
  from this audit; neither the lock wait nor cancellation is a source failure.
- Peer-reported Linux suite at `77f302b`-equivalent source: 1,070 passes, zero
  failures, 10 ignored (Agent Mail 42442). This is useful corroboration, not
  independently inspected current-source proof. The peer's local Clippy receipt
  does not satisfy the requested RCH build workflow.
- Fresh isolated Mac execution confirms the limited workflows above and planned
  command refusals. Demos' embedded elapsed/usage fields are synthetic historical
  values; they are not timings or provider charges incurred by this audit. The
  local receipt copy is `/data/tmp/sr-reality-20260921-macos-cli.json`.
- Public-contract validation passed (46 local links, 22 shell examples, five
  JSON examples, 19 capability phases); it explicitly reports documentation
  consistency only and does not execute those examples. The owned Markdown
  diff passed whitespace checks. `ubs --staged` detected only Markdown and
  exited 3 without running a scanner; that is not a code-scan pass.
- `br dep cycles`: no cycles. GitHub releases: empty at review time.

### Existing backlog coverage and the shortest useful bridge

The 00:51 UTC snapshot contains 249 beads: 163 closed, 76 open, three in progress,
six deferred and one blocked. This is **not** a product completion percentage.
The G01–G70/I01–I15 obligations have existing implementation or acceptance owners;
no additional unowned vision family was found. The deficiencies above belong in
those tasks, not a duplicate roadmap. Findings were sent to the owners and the
current Beads export lease holder (Agent Mail 42444–42445), and recorded as
comments on `.7.11`, `.5.23` and `sr-pnt3` using `br --no-auto-flush` to preserve
the peer's active JSONL export reservation.

Completing every existing task **to its behavioral acceptance criteria** would
cover the documented scope, including the later optional features. Merely
implementing the code tasks would not: independent labels, a real supported
harness, controlled runs, measured latency and actual release artifacts remain
necessary. Deferred native adapters may legitimately end with an explicit
unsupported disposition; that is not implementation of those adapters.

| Order | Existing work to finish | Concrete exit evidence |
|---|---|---|
| 1 | `.6.7` attempt accounting and `sr-pnt3` first-run status | Reached-attempt hard kill/restart retains unknown usage; repeated/cache/follower paths add no invented cost. Fresh init/status agree without destroying history. |
| 2 | `.6.13`, `.6.19`, `.6.20` reporting and evaluator | An isolated CLI journey captures a real case, observes/labels it, replays it and produces stats plus a batch comparison. Missing, failed and unstarted cases remain in the right denominators; live evaluation requires separate consent and caps. |
| 3 | `.6.22`–`.6.29` integrate and accept evidence workflows | Numerical helpers are actually consumed by reports; adversarial inputs cannot fabricate a passed gate. Keep independent endpoint/baseline and partial-evidence tests. |
| 4 | `.7.11`, `.7.9`, `.8.9`/`.8.10` qualified installed trial | Runtime checks enforce harness/trial scope; actual supported Claude receives bounded advice; default shadow, denied scope and return-to-shadow stay quiet, including an in-flight stop. |
| 5 | `.7.4`–`.7.8`, `.7.10`, `.7.12`, `.8.1`–`.8.6` operational and usefulness proof | Durable shared controls, scoped snoozes, declared latency/fallback cohort and independently judged relevance/harm evidence pass their prespecified gates. No post-result threshold changes. |
| 6 | `.5.23`, `.8.7` publish the proven platform subset | DSR builds signed artifacts from the frozen accepted source; test clean installation and actual use per claimed target, preserving explicit unqualified targets. |

The highest-impact integration is the evaluator/reporting seam, after attempt
accounting. It makes defects and usefulness measurable using code already built.
The highest-impact correctness repair is enforcing hook qualification at the
actual output boundary. The highest-impact product proof is an independent
relevance cohort followed by controlled agent outcomes. More math, a TUI, or a
larger backlog does not substitute for any of those.

This update serves the user's explicitly requested comprehensive assessment;
its current-state claims retire when superseded by the next revision-bound
assessment. It adds no runtime capability and closes no implementation gate.
It revises the existing bridge in place. Coverage, dependency ordering,
positive/refusal test pairs, independent evidence, and external acceptance were
rechecked against the existing tasks; no speculative feature or duplicate epic
was added. The following sections are historical snapshots and must not be read
as the current implementation inventory.

## Historical assessment — 2026-09-19, evening integration review

**The standalone core ranker is real. The complete live-session recommendation
product is not finished, and the current integration is not accepted.** The
highest-value work is connecting and verifying the existing boundaries, then
delivering observations and a recorded shadow hook. More ranking features are
not the present bottleneck. Jev remains the essential inference engine; local
explicit resolution, demos and previews are not a replacement backend.

This assessment read all 672 lines of AGENTS.md and all 1,994 lines of README.md,
reviewed the comprehensive plan and relevant contracts, traced command dispatch
through context, roster, provider, cache and output, and checked live Beads and
peer evidence. The fresh executable/test snapshot is
`c9be6a7c5c7ae6936b9dade84fc3aea84c26eec2`. Peers continued changing main during
the review; later commits require their own receipts. This is a comprehensive
workflow assessment, not certification of every source line or closed bead.
The earlier reviews and full G01–G70/I01–I15 requirement crosswalks remain below.

### Current vision-to-code map

`WORKING` means the named path has executable evidence, not whole-phase
acceptance. `PARTIAL` identifies a connected path with material missing work;
`UNPROVEN` identifies an external claim lacking qualification.

| Goal / user workflow | Status and source reality | Remaining owner or acceptance boundary |
|---|---|---|
| 1. Standalone safe Rust CLI and explicit effects | WORKING core: one package, forbidden unsafe roots, Asupersync, Quill and bundled SQLite; no runtime ms or second inference backend | P0 contracts; P1/P4 acceptance remain separate |
| 2. Inspect setup without disclosing a session | WORKING `doctor`, config provenance and `capabilities`; key presence is distinct from consent/authentication | `.5.17`/`.5.18`; real onboarding `.7.9` |
| 3. Inspect, snapshot and compare a roster | WORKING real CLI; private export directory and no-clobber refusal verified | `.3.14`/`.3.15`/`.3.17`; harness qualification remains |
| 4. Bind discovery to selected harness and roots | WORKING scoped repair `f9f5b7d`: configured Claude roots reach rank; another harness needs an explicit roster | `.5.26` independently verified; visibility remains unverified |
| 5. Select exact native/normalized session | WORKING bounded core paths and explicit discovery; no implied native advice qualification | `.5.24` and P3 receipts; `sr-fpww` and `.4.7.1` retained as acceptance checks |
| 6. Read an exact Cass archive | PARTIAL at audited snapshot: module/tests exist, but rank dispatch returns unsupported/7 | `.5.25`; three fresh failing binary tests, owner notified |
| 7. Honor explicit skill requests locally | WORKING explicit offline resolution and manual-only handling; no Jev call | `.5.11.1`, `.5.14.2`; preserve full explicit list/revalidation |
| 8. Redact and preview bounded context | WORKING stateless wide/shortlist preview, profiles and disclosure components | P3 and `.5.15`; no proof that reduced disclosure preserves relevance |
| 9. Quill overflow, Jev wide/rerank and scoring | WORKING connected core with real local TLS tests, none/fit filtering and finite scores | `.5.19`/`.5.21`; live maximum-shape pair `.2.10` still unqualified |
| 10. Revalidate every publication dependency | WORKING scoped repairs include abstentions and indexed candidates outside the cutoff | `.5.27`, `.5.27.1`; tested under mutation, not a filesystem freeze |
| 11. Exact cache isolation and reuse | PARTIAL: namespace repair `575d54f` and real reuse tests pass; production coordination differs from fixed helper | New `.5.29` blocks P4; no blanket single-flight guarantee |
| 12. Explain exclusions and retrieve all pages | PARTIAL: cursor command works, but evaluated-history/answer binding and zero-new-request continuation remain incomplete | `.5.14` reopened after narrow tests passed |
| 13. Whole-invocation deadline and cleanup | PARTIAL: bounded runtime/leaf tests exist; contention-sensitive startup/shutdown qualification remains | `sr-5n0b` directly blocks `.2.12`; stdin `sr-fpww` blocks P4 |
| 14. Observe current loads and persist history | PARTIAL library/storage primitives; rank still constructs empty loaded records; no ledger command/schema workflow | `.6.1`–`.6.10`, `.6.26`; `.6.9` explicitly owns transient as well as durable wiring |
| 15. Record judgments and useful value statistics | NOT_STARTED public workflow; feedback/stats commands rejected | `.6.11`–`.6.13`, `.6.26`; adoption must not become usefulness |
| 16. Capture and replay surprising cases | PARTIAL contracts/export primitives; `--save-case` deliberately refused, replay absent | `.6.14`–`.6.16`, `.6.27`; frozen evidence, inert paths and no-clobber proof |
| 17. Evaluate and sample within a budget | PARTIAL synthetic policy oracles; no usable eval command or integrated numerical backends | `.6.17`–`.6.25`, `.6.28`/`.6.29`; all failed/unstarted/unknown cases accounted for |
| 18. Safe Claude shadow/advisory delivery | NOT_STARTED executable hook/installer; `hook` currently emits CLI error JSON and exit 2 | `.7.1`–`.7.3`, `.7.9`/`.7.11`/`.7.13`; do not install this binary as the proposed hook |
| 19. Shared attempt allowance, breaker and snoozes | NOT_STARTED user workflows; per-invocation admission is not a durable shared allowance | `.7.4`–`.7.8`/`.7.10`; accounting, recovery and authority tests required |
| 20. Demonstrated usefulness and operational benefit | UNPROVEN: no accepted relevance, paired harm or representative hook cohorts | `.8.1`–`.8.6`, `.8.9`/`.8.10`; fixtures and unit counts cannot pass these gates |
| 21. Calibration, priors, rollback and monitoring | NOT_STARTED public workflows; policy/math contracts exist | `.9.1`–`.9.9`; independent labels and held-out benefit remain mandatory |
| 22. TUI, watch, description/gap tools and retrieval experiments | NOT_STARTED public workflows; `tui = []` is reserved only | `.10.1`–`.10.32` and dedicated proof children; preserve optional scope |
| 23. Other native harnesses | UNPROVEN; normalized input plus explicit roster is the general boundary | Individual `.10.15`–`.10.20` qualification/implementation chains; six deferred records |
| 24. Installable qualified Linux/macOS release | UNPROVEN release: GitHub Releases still empty; storage module Linux-gated | `.5.23` macOS, `.8.7` DSR packaging and artifact/install proof |

### Newly exposed integration defects

**Production coordination is not the repaired coordinator helper.** At the
audited revision, `pipeline::persistent::wait_for_leader` stops waiting when less
than one second of work remains. A cache miss then falls through to a provider
call without acquiring a successor lease. An already-completed lease with a
missing pair has the same problem. Wide/rerank cache writes call
`CacheStore::record_response`, which checks store incarnation/generation but
not request lease ownership; completion of the separate `leases.sqlite3` record
happens afterward. `c70cbd1` correctly repairs the helper's atomic publication,
but production does not call that helper. `.5.29` owns the actual integration,
with real two-process TLS/SQLite barriers, stale-owner overwrite refusal, bounded
follower timeout, complete-pair success and zero-new-usage twins. This is a
source-confirmed defect; this assessment has not executed its race reproducer.
RubyCat has claimed the repair.

**Trace continuation is not yet a frozen evaluation.** `TraceQueryScope` hashes
latest request text, selected flags and thresholds, but omits preceding context,
task anchor, model and actual evaluated answers. Equal prompt/roster can therefore
accept a cursor for a different evaluation. `--cursor` reruns ranking and may
make new provider calls. Six new continuation tests pass but do not test these
cases. The existing `.5.14` now explicitly requires exact evaluated evidence,
no additional inference for continuation, and refusal when that evidence cannot
be recovered. No hidden persistent trace store is permitted under no-persist.

**Cass code was committed without its effective dispatch.** Three fresh
`cass_session` tests expected success/0, missing-session/3 and unusable-roster/5;
all received unsupported-source-mode/7. Both committed and working pipeline
source showed the stub. This is a demonstrated integration failure, not a
speculative stale-build explanation. CopperWren and MistyCoyote received the
receipt to reconcile their overlapping pipeline work. CopperWren subsequently
confirmed the dispatch hunks had been held back during another agent's reservation,
not lost from an earlier working commit. Their integration remains separate from
this failed snapshot; no source-overwrite cause is asserted.

**Evidence and dependency drift can hide unfinished work.** The previously
closed `.5.19` still depends on reopened `.5.14`, while new stdin/tool defects
were not directly blocking core acceptance. `.5.21` now directly depends on
trace, Cass, `.5.29`, `sr-fpww` and `.4.7.1`, in addition to its existing gates.
`sr-5n0b` directly blocks runtime acceptance `.2.12`. This does not erase valid
earlier focused results or require reopening every ancestor.

### Bridge order and ambition passes

1. **Finish and independently verify the connected core.** Integrate `.5.29`,
   restore Cass, finish trace identity, retain stdin/tool/runtime proof and
   qualify the live Jev pair under the approved `0.1` sum tolerance. Freeze a
   source revision after peer integration, then run core CLI and concurrency
   journeys. A branch containing passing components but a lost dispatch is not
   an accepted core.
2. **Deliver minimum observable shadow operation.** Implement ledger schema,
   incarnation/generation checks, explicit init, ranking/attempt records and
   source-scoped observations. Wire transient load evidence even when durable
   history is disabled. Then implement the dedicated shadow boundary and managed
   installation with actual harness evidence. Do not enable priors or claim
   advisory usefulness from recorded loads.
3. **Make mistakes reproducible and measure value.** Complete capture/replay,
   feedback and bounded evaluation; collect the already planned independently
   judged relevance, controlled harm and representative operational cohorts.
   Preserve every denominator, unknown outcome and paid failed attempt. Use
   results to decide advisory promotion, not another heuristic feature pass.
4. **Finish distribution and separately gated optional features.** Qualify macOS
   filesystem/storage behavior and DSR artifacts. Retain TUI, diagnostics,
   learning, numerical tooling and adapter experiments under their existing
   owners; they do not become prerequisites for the useful core.

The first ambition pass strengthens the actual product boundary: one production
coordination path and reproducible evaluation identity, rather than more isolated
helper proofs. The second shortens time to useful shadow integration: `.7.1`
now depends on concrete `.6.4`/`.6.8`/`.6.9` prerequisites instead of waiting for
the entire evaluator. **Full P5 acceptance `.6.29` is retained as a direct
prerequisite of P6 acceptance `.7.13`.** No feature, acceptance criterion or
public-advisory gate was removed. The third checks whether extra features improve
the next measurable outcome: no new model call, daemon, retrieval engine or UI
was justified before these existing workflows are connected.

Five refinement passes covered (1) current dispatch versus old status prose,
(2) causal ownership/publication boundaries, (3) negative tests and honest success
twins, (4) real dependency ordering and complete vision ownership, and (5) final
proof scope and graph health. Changes were applied to existing beads/comments
through `br`; only the previously uncovered production coordination integration
needed a new bead. Later fixes still need their own tests; this review stops at
a concrete handoff rather than manufacturing implementation credit.

### Executed evidence and limits

- Required-remote, frozen `c9be6a7`, no working-tree overlay, worker
  `vmi1153651`: cache identity **5/0**, capabilities **5/0**, then Cass **0/3**.
  Cargo stopped at Cass. Log `/data/tmp/sr-reality-refresh-tests-20260919.log`,
  job `j-30027234903327433`, remote exit 101.
- A separate run of the previously unexecuted targets, on the same frozen
  source with `--no-fail-fast`: demo **6/0**, rank acceptance **56/0**, rank
  inputs **15/0**, trace continuation **6/0**; remote exit 0, job
  `j-30027234903327446`. Log
  `/data/tmp/sr-reality-refresh-remaining-20260919.log`. Together these are
  **93 passing and three failing tests**, not a full-suite pass.
- Retrieved the tested build's CLI, matched worker/local SHA-256
  `be952a6444fe71daf3b21ea755fcf13816d60a0d488a4e4ed3ec4eba372b222f`,
  and ran **34 isolated synthetic CLI probes** with cleared credentials,
  private HOME/XDG and no network opt-in. Demos, doctor/config, capabilities,
  custom-root dry-run, explicit offline resolution and private roster snapshot/
  diff succeeded. Fourteen unimplemented commands returned usage/2; save-case
  also refused. Offline inference returned cache-miss/11, foreign harness without
  a roster refused/5, and Cass refused/7. Export to a non-private directory and
  overwrite of an existing snapshot correctly refused/9; the initial diff had
  no snapshot and refused/7. No cache/data directories were created. Report:
  `/data/tmp/sr-reality-refresh-smoke-20260919.json`.
- Historical full receipts remain distinct: `3820bd1` passed **754/0/9 ignored**;
  CopperWren reports **760/0/9 ignored** for the harness/roots repair. They do not
  cover the later Cass integration failure. Earlier runtime-shutdown and
  contention failures remain in `sr-5n0b`; a later pass does not diagnose them.
- Independent source review accepted the scoped `39296de` tool-association
  repair and its success/negative tests. Owner's focused **25/0** receipt is not
  an independently rerun whole-project gate, nor proof of ledger wiring.
- Live-provider receipts still establish small HTTPS success and separate large
  stage observations, not a qualified maximum-shape production pair under the
  current tolerance, measured recommendation quality or hook latency targets.
  No fresh Jev requests, personal transcript reads, hook installation or release
  were performed in this assessment.
- `python3 scripts/validate_public_contracts.py` passes documentation consistency;
  it explicitly does not execute examples. GitHub Releases returned `[]`.
  Graph checks found zero active cycles and no dangling dependency IDs. Final
  `bv --robot-triage` read 228 records and correctly kept P4 unclaimable behind
  its six then-active blockers. Its history enrichment timed out; graph triage
  completed, but no complete history-analysis claim is made.

**Backlog coverage:** the G01–G70 and I01–I15 maps still cover the product vision.
The checked 228-record snapshot had all 29 P5, 13 P6, ten P7 and nine P8 children
open; P9 had 30 open and six deferred children. These are ownership counts, not
completion percentages. No major unowned product feature was found. The new
`.5.29` and existing-bead refinements close specific integration/proof gaps in
that map. Completing the code tasks is necessary but insufficient: actual Jev,
harness, platform and independent outcome evidence must also satisfy the named
gates. Neither this review nor a green Cargo run can supply those observations.

## Historical assessment — 2026-09-19, earlier core review

**SkillRanker now has a real executable ranking pipeline. It is not yet the
complete, qualified live-session recommendation system described by the README.**
The previous assessment's central claim that `rank` is not reachable is obsolete.
The immediate bottleneck is correctness and qualification of the connected core,
followed by durable observations and a real shadow hook. More optional features
will not close those gaps.

This review read all of AGENTS.md and README.md, reviewed the comprehensive plan,
command dispatch, context/roster/cache/provider/publication boundaries, capability
and evaluation contracts, recent commits, historical sessions and live Beads.
It used the `reality-check-for-project` skill and retained the existing vision,
70-goal and I01–I15 crosswalks below. This is a workflow and architecture assessment,
not an exhaustive security audit of every source line. The executed Rust/CLI
snapshot is `29dddb8cd3ca62a8808c96dc0b33c65aed1ff8a2`; subsequent peer changes
are separately identified. README keeps the requested finished-product voice.

### What works, and what the evidence establishes

| Workflow | Actual implementation | Current proof boundary |
|---|---|---|
| Configuration and inspection | `doctor`, `doctor --config`, roster listing/snapshot/diff, capabilities | Real CLI dispatch and focused tests; configured roots work in inspection but are not connected to ranking |
| Local explicit requirements | Bounded context input, exact skill resolution and offline explicit output | Actual binary returned explicit success without credentials or persistent state; this is local resolution, not key-free probabilistic ranking |
| Rank preparation | Normalized/native Claude input, redaction, disclosure, bounded Quill overflow, dry-run wide/explicit-shortlist preparation | Source plus subprocess tests and direct no-network preview; inventory and native identity qualifications remain |
| Jev ranking | Owned Asupersync transport, sequential wide/rerank, gates, none/fit eligibility, scoring and JSON/table output are connected | Synthetic transport/CLI tests establish integration; live maximum-shape pair and operational qualification remain open |
| Exact response reuse | Protected fingerprints, SQLite-backed response storage, coordination and cache-only refusal are connected | Existing focused cache tests cover important paths; identity omissions and competing-process failure prevent blanket isolation/reliability acceptance |
| Explanation and demonstration | Four non-actionable demo cases and stage/why-not trace data | Demos execute; `.5.14` remains open for complete trace pagination/query identity, not a claim that all explanation promises are finished |
| Session archive input | Cass capability-checked library adapter exists | `rank --session PATH` still returns `unsupported-source-mode`, exit 7; `.5.25` owns wiring |
| Persistent observations and feedback | Storage primitives and contracts exist | No observation-ledger schema or reachable ledger/observe/feedback/stats workflow; ranking reports ledger disabled/unavailable |
| Capture, replay and evaluation | Artifact schemas, synthetic oracles, policy validators and export primitives | No complete save-case/replay/eval command; fixtures are not measured usefulness evidence |
| Claude integration | Context adapter and hook contracts | No reachable dedicated hook or managed installer, no verified real shadow/advisory deployment |
| Learning, monitoring and UI | Contracts/plans | Calibration/monitor/TUI are not reachable; `tui = []` supplies no implementation |
| Distribution and other platforms | Linux binary builds through RCH | GitHub Releases returned `[]`; Linux-gated storage prevents inferring macOS support from Rust portability |

TypeSafe.ai's Jev remains the essential ranking engine. Live ranking requires the
user's own TypeSafe API key and separate trusted network consent. The successful
offline explicit/demo/preview paths do not replace it. This audit did not retrieve
credentials, send session content, make fresh Jev calls, install hooks or release
artifacts.

### Three newly identified core acceptance gaps

All three are now open bugs and explicit blockers of `sr-roadmap-l1i.5.21`.
Their acceptance criteria require the real integration boundary and both refusal
and honest-success cases, not just helper tests.

1. **Rank does not bind discovery to effective roots and harness — `.5.26`.**
   `src/pipeline/roster.rs::Source::load` always constructs a Claude plan and does
   not consume effective `roster.roots` or the normalized harness. A direct CLI
   reproduction on the frozen binary configured `custom/alpha`: `sr roster`
   listed it, but rank dry-run returned `empty-roster`, exit 5. Adding only
   `.claude/skills/beta` and changing normalized harness to `codex` produced a
   wide preview containing beta. Inspection correctly labels the custom inventory
   unverified; the repair must preserve that distinction rather than declare all
   configured files loadable. The closed `sr-odcb` inspection fix does not fix rank.
2. **Abstention paths bypass final dependency checks — `.5.27`.**
   Source review found wide `LowNeed` refreshes policy but does not revalidate
   roster; the post-rerank `Verdict::Abstain` returns before both final roster
   validation and publication policy refresh. Local all-excluded paths need the
   same audit. Positive ranked/explicit publication tests do not prove negative
   decisions remain valid after roster or policy changes. This is source evidence;
   this audit did not execute a deterministic mid-flight race reproduction.
3. **Cache/lease identity omits producer and agent binding — `.5.28`.**
   The pipeline namespace includes harness/workspace/session and supplied branch/
   epoch, but not normalized producer/agent identity; adapter identity is not set.
   Native normalized construction sets branch to `None` rather than carrying the
   resolved branch. Those private identity fields also do not appear in provider
   request bytes. The earlier `sr-yac4` repair correctly framed included fields;
   it cannot distinguish omitted fields. Actual cross-producer reuse remains to
   be reproduced through rank, with same-identity reuse as the positive twin.

These are concrete gaps in already connected code, not additional product scope.
A large count of passing component tests did not expose them. Keep their proof
classes distinct: the first has a direct CLI reproduction; the other two have
specific source paths and require behavioral reproduction before repair closure.

### Live-provider and concurrency qualifications

The existing live receipts in [the Jev spike](jev-contract-spike.md) establish a
small successful production HTTPS request, intermittent large-wide results and a
separate successful large rerank. The successful 32-candidate rerank was an
explicit evaluation-stage probe, not evidence that a production wide/rerank pair
completed. A 255-option wide response totaled approximately 0.99 and was rejected
under the previous `1e-4` sum tolerance. That is historical evidence of the former
contract, not a permanent provider failure.

Concurrent commit `5ae2431` changes the accepted total deviation to `0.1`, retains
raw values and uses normalized probabilities downstream. This audit's 82-test
receipt and copied binary predate that change. The new policy can address the
observed rejection, but does not itself prove a successful full-size live pair,
end-to-end budget compliance or ranking quality. `.2.10` and P1 acceptance `.2.12`
remain the qualification owners. Do not silently widen the claim from a decoded
response to a useful recommendation.

Preserve both full-suite results: the earlier frozen `b509af2` run passed 723
reported tests with nine ignored entries, while a later run on that production
base plus the roster-test overlay stopped at the competing-process coordination
test with child exit 101 (179 reported passes, one failure, two ignored entries).
Its discarded child diagnostics do not establish the cause. `.5.10.1` now owns
retained diagnostics and deterministic synchronization. The owner subsequently
identified a plausible late-child/initial-cache-hit assertion race; this is not
retrospective proof of the original failure. `sr-5n0b` separately tracks load-
sensitive runtime startup/shutdown behavior. Neither is cleared by this audit's
focused pass. The coordinator owner also reproduced expired/late calls at the
library boundary (`.5.10.2`) and identified stale-owner cache publication before
fenced completion (`.5.10.3`). That helper is not called by the production pipeline;
its fixes must not be credited as end-to-end rank proof.

### What remains between the current CLI and the promised product

The source has one coherent Rust package with mostly pure normalization,
validation, retrieval and scoring behind visible effect boundaries. The principal
architecture choices are in place: Asupersync, bundled SQLite and bounded Quill,
without a second inference backend. The missing work is concentrated at the
integration and evidence boundaries:

- **Core acceptance:** repair `.5.26`–`.5.28`; finish native context/discovery
  verification (`.5.24`, `.5.24.2`), stdin proof (`sr-fpww`), trace completeness,
  Cass wiring, coordination/runtime failures and revised live Jev qualification.
  The actual installed harness's visibility/precedence must be verified rather
  than inferred from familiar directories.
- **Observation and reproducibility:** implement the P5 ledger, cursors, load
  attribution, feedback, capture/replay and evaluator. Current rank initializes
  empty loaded-reference records; the promised observation-based suppression and
  learning loop are not connected merely because context models exist.
- **Real session delivery:** implement P6's quiet shadow hook, managed settings
  edits, bounded delivery/accounting and operational controls, then exercise the
  actual harness. The audited binary treats `hook` as ordinary invalid usage with
  JSON stdout and exit 2. It must not be installed as the proposed fail-open hook.
- **Demonstrated benefit:** collect the planned independently judged relevance
  holdout, controlled harm cohort and representative hook invocations. The
  checked-in synthetic cases are contract oracles, not the required 300-family,
  150-family or 500-invocation cohorts. Passing tests proves neither useful
  recommendations nor latency/cost/harm targets on real sessions.
- **Release and optional work:** native macOS storage/build tests, DSR artifacts
  and installation verification remain. Priors/calibration/monitoring, TUI,
  watch/description experiments and other native adapters keep their own gates.
  They should not delay a trustworthy, useful core and measured shadow workflow.

The refreshed inventory contained 225 records: 103 closed, 108 open, seven in
progress, one blocked and six deferred. This includes concurrent peer additions
and is not a completion percentage. All 29 P5 children, 13 P6 children, ten P7
children and nine P8 children were open in that snapshot. Existing P9 records
cover later interfaces/experiments and explicitly defer six native implementation/
proof tasks pending qualification. No new giant roadmap is needed.

**Would completing the backlog close the vision?** The phase and I01–I15 maps
already cover the major promised workflows. They can close the planned scope if
acceptance includes actual integration and independent external evidence. The
three new bugs were necessary because earlier narrow closures did not cover the
whole connected boundary. Implementing open code tasks alone cannot manufacture
labeled cohorts, live service behavior, installed-harness support or platform
receipts. Unknown future defects remain possible; this review does not certify
all closed beads.

### Bridge sequence and refinement decisions

1. Finish the core acceptance blockers with one coordinated build lane and exact
   source receipts. Prioritize harness/roster authority and cache isolation;
   include all abstention paths. Finish the revised live pair and retained
   concurrency diagnostics before claiming P4 acceptance.
2. Deliver the minimum complete P5 observation/capture/replay path against actual
   persisted state, then the real P6 shadow hook. Keep optional learning disabled
   and failures nonblocking. Do not substitute synthetic fixtures for the real
   observation and delivery boundaries.
3. Run the existing independent relevance, harm and operational protocols. Use
   the results to decide advisory promotion and tune policy; do not design more
   clever ranking machinery before learning whether the current one helps.
4. Complete release/platform proof and separately gated optional capabilities.
   Keep the full vision in its existing owners rather than quietly removing it.

Refinement in this pass changed the existing assessment in place. The first pass
reconciled command and phase claims; the second followed authority and publication
paths and found the three bugs; the third challenged proof strength and preserved
both failing and passing receipts. A fourth checked existing owner coverage to
avoid duplicate ledger/hook/evaluation work. The final graph check found zero
active dependency cycles after the new core blockers. The ambition is stronger
observable delivery, not a new architecture or more routine model calls.

### Verification from this assessment

- Required-remote clean-overlay run on `29dddb8`, worker `vmi1153651`, passed
  **82 tests / 0 failures / 0 ignored** across `capabilities_contract`,
  `demo_contract`, `native_identity`, `rank_acceptance`, `rank_inputs` and
  `transcript_request`. Receipt overlay fingerprint:
  `fe0d5151031be8fda7951fe7fe1f42f7ce344018fdb3ed21e6ada866b230b195`.
  Log: `/data/tmp/sr-reality-current-tests-20260919.log`.
- Retrieved that run's `sr` executable and matched remote/local SHA-256:
  `30dd63dd41259a07bea1284131fcf750fc233ec7dd3dccbaccc2216aca036853`.
  Executed **22 isolated synthetic CLI probes** with cleared credentials,
  private HOME/XDG paths and no opted-in network. Capabilities, doctor, all four
  demos, default dry-run and explicit offline resolution succeeded; offline
  inference returned cache-miss/11; Cass returned unsupported-source-mode/7;
  nine missing commands returned invalid-usage/2. The configured-root and foreign-
  harness probes reproduced `.5.26`. These are observations, not 22 product gates.
  No cache/data directories were created. Report:
  `/data/tmp/sr-reality-smoke-20260919.json`.
- `python3 scripts/validate_public_contracts.py` passed documentation consistency
  checks. It does not execute README examples or certify the report's findings.
- `br dep cycles --json` found zero active cycles; `bv --robot-triage` completed.
  GitHub Releases returned an empty array. No release or macOS test was performed.
- No source code, README product copy or existing test assertion was changed by
  this assessment. Three gap Beads and the existing bridge report are the durable
  changes. Peer edits and their evidence remain separately owned. This is a
  self-reviewed assessment, not independent certification of every closed task.

This section serves Jeffrey's explicit reality-check request and the core
acceptance decision. Retire its current-status authority when a newer
revision-bound assessment supersedes it; preserve historical receipts under the
repository's no-deletion rule. The review itself earns no implementation credit.

## Historical assessment — 2026-09-18, evening source review

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
| G11 | Validate every requested answer/type and exactly its option IDs; reject duplicates, foreign/missing IDs, non-finite/out-of-range values, invalid usage and bad chosen-option argmax. Distribution sum is positive and within the maintainer-approved `0.1` tolerance; renormalize accepted distributions and retain raw estimates. The earlier `1e-4` bound is superseded. Enforce decoded/decompressed response bound. | Plan:522–533; current AGENTS Jev contract |
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
