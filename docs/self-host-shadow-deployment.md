# Self-Hosted Shadow Deployment Plan (sr-1uf4)

Pre-registration for running `sr hook claude` in **shadow mode** on this
repository's maintainer environment to produce the operational cohort the
promotion gates consume (`.8.4`: ≤5% operational fallback over ≥500
representative hook invocations; latency p50/p95/p99 across cold/warm/cache
strata). Decisions 1–4 below are settled **before** any traffic counts, as
the bead requires. This document changes no defaults for anyone else.

## 1. Consent and privacy boundary

- Scope: sessions of coding agents working in this repository on this
  machine, including subagents. The maintainer operates all of them.
- Shadow recording writes bounded local metadata (events, attempts, usage,
  latency) to `~/.local/share/sr/ledger.sqlite3`. Case bodies and replay
  capture remain separately opt-in and are **not** enabled.
- Absolute workspace paths, branch names, and ledger identifiers stay local;
  they never enter provider payloads beyond the documented bounded fields.
- Jev receives the standard redacted ranking context for every evaluated
  prompt. This is the same disclosure the CLI already makes with
  `--allow-network`; the deployment consent extends it to hook traffic.
- Retention: ledger default (30-day logical retention, explicit prune).

## 2. Declared population (not a general one)

Traffic from this environment is: one repository, one language (Rust), one
unusually skill-dense library (~210 personal skills, no overflow stratum),
agent-paced rather than human-paced sessions, and a maintainer machine with
unusually fast storage/CPU. Reports against this cohort **must** carry this
declaration. It supports operational claims (fallback rates, latencies,
cache strata, provider outage behavior) for the declared population. It does
not support production-prevalence or general-user claims, and per sr-uv2v it
supplies *sessions* for relevance cases only when labels come from an
independent blinded adjudicator.

## 3. Provider budget (requires maintainer sign-off)

Shadow evaluation calls Jev on every user prompt: 2 logical requests (wide +
rerank when the gate passes), up to 4 HTTP attempts. At this fleet's current
activity that is several hundred invocations per day against ~210-candidate
Choices. The durable shared allowance (`.7.4`–`.7.8`) does not exist yet, so
there is **no hard automated cap**; the mitigations are:

- the provider circuit behavior of `sr` itself (outages fail closed, quietly);
- per-invocation caps (2 requests / 4 attempts / 3 s deadline);
- manual inspection via `sr stats` (attempts, known tokens, unknown usage);
- an explicit revocation path (§6) that stops spend in one command.

Estimated daily spend driver: `invocations × ~2 requests × ~10–20 K tokens`.
The maintainer sets the acceptable number before installation.

## 4. Blinding for the paired harm cohort (`.8.2`)

The paired advice/baseline design over bead worktrees is **not** part of this
deployment. Shadow mode injects nothing, so this deployment cannot measure
advice effects at all; it produces the operational cohort and candidate
sessions for sr-uv2v. A paired-arm experiment needs its own pre-registration
(randomized arm assignment, blinded outcome reviewer who is not the evaluated
agent) and is out of scope here.

## 5. Installation procedure (each step verified before the next)

1. **Build a release binary at a frozen revision** — the hook must not point
   at the fleet's debug target, which every peer build overwrites:
   `cargo build --locked --release` → install to `~/.local/bin/sr`
   (record the SHA256 and revision in this document when done).
2. `sr ledger init` in the real HOME (idempotent; preserves any existing
   store).
3. Trusted user configuration `~/.config/sr/config.toml` gains
   `[network] enabled = true`. Hook mode stays **shadow** (no `[hook]`
   section change); advisory remains off.
4. `sr install-hook claude` preview (verified 2026-09-21: adds one managed
   `UserPromptSubmit` matcher entry, `timeout: 4`, preserves the existing
   `claude-prompt-compact-check` entry and all unrelated settings), then
   `--apply`. Backup lands in `~/.local/state/sr/backups`.
5. **Credential environment**: `TYPESAFE_API_KEY` must be in the Claude
   process environment (hooks inherit it). Source the maintainer `.env`
   before launching agent sessions; sessions launched without the key record
   `credential-absent` unavailability, which must be separated from provider
   outages in the cohort report.
6. Smoke: one prompt through a real Claude session; confirm a shadow turn is
   recorded (`sr stats --json`, shadow channel) with zero bytes injected.

## 6. Revocation

`sr uninstall-hook claude --apply` removes only the managed entry (preview
first; the existing compact-check hook is untouched). Setting
`network.enabled = false` stops Jev spend while leaving recording of local
turns intact. Both paths are tested by the installer contract suite.

## 7. What the cohort report must contain (before any gate claim)

All of: total invocations with denominators; fallback rate with causes
(provider outage / budget / timeout / credential-absent kept separate);
p50/p95/p99 latency across cold, warm-network, and exact-cache strata;
memory; requests and known/unknown token counts; the declared population
statement from §2; the binary revision and roster size range. Reporting only
fast successful calls is forbidden by AGENTS.md.


## Deployment receipt — 2026-09-22 (executed)

- Binary: `~/.local/bin/sr`, SHA256
  `badfa6a9a9bb5711a51873a86b15389d192ac89b4fe59b9fea342464c2df39ed`, built
  `--release --locked` from `5dcdb9a` (the same session's overlay/parser fixes;
  earlier binary backed up to `~/.local/bin/sr.backup-20260921`).
- Ledger: initialized at `~/.local/share/sr/ledger.sqlite3` (schema 1).
- Config: `~/.config/sr/config.toml` carries `[network] enabled = true`
  (trusted-user consent); hook mode remains shadow (built-in default).
- Hook: managed entry applied to `~/.claude/settings.json` (timeout 4,
  existing compact-check entry preserved; backup in `~/.local/state/sr/backups`).
- Smoke: three overlay/parser defects were found and fixed before the hook
  could process a real transcript (sr-j4k9 plus follow-ups): unmodeled
  harness records were fatal, thinking-only/unknown blocks were fatal, and
  subagent sidechain leaves made branch resolution fail. Post-fix smoke on a
  real 79 MB session transcript: overlay succeeds, and with
  `TYPESAFE_API_KEY` present a full two-stage Jev evaluation completed and
  recorded 21,738 known tokens across two attempts. Shadow mode emitted zero
  bytes throughout.
- Known gaps found by the smoke, filed as beads: sr-e8vm (deduplicated
  redelivery accounting), sr-ksjn (a test wrote into the operator's real
  ledger).
- Credential environment: agent sessions launched without
  `TYPESAFE_API_KEY` record honest credential-absent turns; sessions needing
  live evaluations must be launched with the maintainer `.env` sourced.

## Scope correction — 2026-09-22 (sr-shadow-consent-scope-ne8b)

The initial install put the managed entry in the **global**
`~/.claude/settings.json`, which recorded sessions from every project on the
machine — wider than §1's one-repository consent. The hook now lives only in
`/data/projects/skillranker/.claude/settings.local.json` (gitignored), so only
this repository's sessions are recorded or transmitted; the global file keeps
just the pre-existing compact-check hook. Sessions recorded while out of
scope remain in the ledger pending an explicit maintainer retention decision;
physical removal is an explicit ledger operation, not something done
unilaterally. A peer audit (`b07403e`) identified the scope violation.

## Credential delivery — 2026-09-22 (sr-hook-credential-env-3i1n)

The maintainer chose shell-rc sourcing: a guarded block in `~/.zshrc` and
`~/.bashrc` sources `/data/projects/skillranker/.env` with `set -a` when the
file exists, so hook processes spawned by any newly launched agent session
carry `TYPESAFE_API_KEY`. Validated with `zsh -n` / `bash -n` and a fresh
interactive shell (presence confirmed, value never printed). Long-running
sessions started before this change keep recording credential-absent turns
until restarted; those rows must be separated from provider outages in the
cohort report (§7).
## Redeployment — 2026-09-22 20:00Z (sr-hook-success-dead-ends-yphr)

- Binary: `~/.local/bin/sr`, SHA256
  `c8ce3f9ffb739761b1f6ed9e5ee7fe5c741f7fd320201fc8f357b7ec9250d64a`, built
  `--release --locked` from a clean worktree at the pushed revision `c6999c3`.
  It includes the side-branch resolver fix (`e261dd4`) and the peers' failure
  counting and credential-absent split (`4c96fae`). The previous `5dcdb9a`
  binary is kept as `~/.local/bin/sr.backup.20260922T200000Z-5dcdb9a`.
- Why: replaying the hook over all 10 recent real transcripts (isolated
  sandbox, no key, no network, no real ledger), the old binary passed context
  capture on 6 of 10 sessions and this build passes 9 of 10. Sessions that had
  run parallel tool calls, or an RCH/dcg-intercepted command, failed silently
  before. The remaining shape is tracked in `sr-interleaved-tool-batches-nwhu`.
- Live smoke into an **isolated** ledger (never the cohort store), with the
  installed binary, the maintainer credential, real TLS to Jev, and this
  repository's real session transcript: `ranked`, wide + rerank, 2 attempts,
  21,275 known tokens, 1,475 ms at host load ~130, zero stdout bytes in shadow
  mode. Only real traffic from sessions launched after the credential change
  can start the cohort; this smoke is not cohort evidence.

## Redeployment — 2026-09-22 (ProudBison, transcript-coverage catch-up)

- Binary: `~/.local/bin/sr`, SHA256
  `ed75516eea94bd4818086e57132d309405cd505aa54680e0c865de0aff4cb484`, built
  `--release --locked` from a clean worktree at the pushed revision `fabe46a`,
  adding the interleaved-tool-batch fix (`sr-interleaved-tool-batches-nwhu`),
  the compaction-boundary lineage fix (`sr-yya0`), injected-document tolerance
  (`sr-oy3a`), and hook entry counting (`sr-01h3`) over the previous deploy.
  Prior binary kept as `~/.local/bin/sr.backup-20260922-fabe46a-pre`.
- Pre-deploy smoke on the largest (82 MB) and two most recent real session
  transcripts: zero stderr, zero injected bytes, including 3/3 repeats on the
  82 MB transcript (one cleanup-warning run under load ~200 was transient).
  Note: an intermediate downgrade to an `e261dd4` build during this session
  was caught and reverted to the peers' newer build first; this deploy then
  moved the cohort forward to `fabe46a`. Deploys must check
  `docs/reality-check-bridge-plan.md` and this file's latest receipt before
  replacing the binary.

## Redeployment — 2026-09-23 22:31Z (AzureJaguar, task-notification fix)

- Binary: `~/.local/bin/sr`, SHA256
  `e784d4db23f44a42b209ed04eaff4c29d5c533a39874a4cd191439e84af8034c`, built
  `--release --locked` from a clean `git archive` export whose 102 build
  inputs (everything outside `.beads/`, `docs/`, `tests/`, `scripts/` and the
  top-level prose files) are byte-identical to the pushed revision `ad28bdd`.
  It adds `sr-jdji` over the previous deploy. A background-task notification
  that Claude delivers inside a running turn re-runs the hook with the turn's
  `prompt_id`. It is no longer ranked as the turn's request or recorded as a
  turn; it is counted in `hook-non-turns.log`, and `sr stats` reports it as
  `hook_entries.non_turn_deliveries`. The previous `fabe46a` binary is kept as
  `~/.local/bin/sr.backup.20260923T223042Z-fabe46a`. (A copy made by mistake
  on 2026-09-23, `sr.backup.20260923T011950Z-c6999c3`, is also `fabe46a`,
  SHA256 `ed75516e…`, despite its name.)
- Checked before replacing: this file's latest receipt (`fabe46a`,
  `ed75516e…`, which matched the installed binary) and
  `docs/reality-check-bridge-plan.md`.
- Pre-deploy smoke: `sr hook claude` replayed over the 80 most recent real
  session transcripts in an isolated sandbox (no key, no network, no real
  ledger). The candidate matched the installed binary's outcome on all 80:
  54 clean, 24 stopped at the continuation gate by the replay's synthetic
  prompt, 1 genuine fork, and 1 transcript that two processes append to.
- Live check, before installing: the same binary was the `UserPromptSubmit`
  hook of a throwaway Claude Code 2.1.280 session driven through NTM, with an
  isolated `--dir`, `--offline` and `--shadow`. A background task finished
  mid-turn, and the queue showed enqueue then remove. The ledger counted 2
  hook entries and 1 non-turn delivery, and recorded 1 row, for the real turn.
  The notification was neither ranked nor recorded.
- Not changed: hook scope (project-local), consent and the credential. Live
  turns still record `credential-absent` until agent panes are started from
  shells that source the credential block (`sr-hook-credential-env-3i1n`).
