# Self-Hosted Shadow Deployment Plan (sr-1uf4)

Pre-registration for running `sr hook claude` in **shadow mode** on this
repository's maintainer environment to produce the operational cohort the
promotion gates consume (`.8.4`: ≤5% operational fallback over ≥500
representative hook invocations; latency p50/p95/p99 across cold/warm/cache
strata). The initial protocol and later revision-bound deployment receipts
are recorded here. Deployment and controlled checks do not establish cohort
acceptance. This document changes no defaults for anyone else.

## 1. Consent and privacy boundary

- Scope: sessions of coding agents working in this repository on this
  machine, including subagents. The maintainer operates all of them.
  Only verified Claude hook traffic is covered by this installation; other
  agent harnesses are not covered merely because they use the repository.
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

Shadow evaluation can call Jev on each covered user prompt: at most 2 logical
requests (wide + rerank when the gate passes), up to 4 HTTP attempts. The durable
shared allowance (`.7.4`–`.7.8`) is implemented. The current local deployment's
`sr budget --json` reports its guard **disabled**, so this deployment has no
enforced shared cap. Its existing mitigations are:

- the provider circuit behavior of `sr` itself (outages fail closed, quietly);
- per-invocation caps (2 requests / 4 attempts / 3 s deadline);
- manual inspection via `sr stats` (attempts, known tokens, unknown usage);
- an explicit revocation path (§6) that stops spend in one command.

The original several-hundred-per-day estimate was not measured. The September
17–21 Claude transcript count was 96 prompts over five active days (about 19 per
day); that historical sample does not establish today's rate. Before expanding
coverage or starting a prospective cohort, freeze the population and sampling
policy and approve an affordable shared allowance. No cap, sampling, consent or
coverage setting changed during the redeployments below.

## 4. Blinding for the paired harm cohort (`.8.2`)

The paired advice/baseline design over bead worktrees is **not** part of this
deployment. Shadow mode injects nothing, so this deployment cannot measure
advice effects at all; it produces the operational cohort and candidate
sessions for sr-uv2v. A paired-arm experiment needs its own pre-registration
(randomized arm assignment, blinded outcome reviewer who is not the evaluated
agent) and is out of scope here.

## 5. Installation procedure (each step verified before the next)

1. **Build a release binary at a frozen revision through the configured DSR
   and mandatory RCH route** — the hook must not point
   at the fleet's debug target, which every peer build overwrites:
   locked native release build → qualify → install to `~/.local/bin/sr`
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
5. **Credential delivery**: the current managed entry uses the private wrapper
   `~/.local/libexec/skillranker/sr`, which loads the checkout's owner-only
   (`0600`) `.env` before executing the installed binary. It does not depend
   on an old Claude process inheriting the credential. Direct `sr` CLI calls
   still require an explicit export as described in `AGENTS.md`. Missing
   credentials record `credential-absent`; keep these separate from provider
   outages in the cohort report.
6. Smoke: one prompt through a real Claude session; confirm a shadow turn is
   recorded (`sr stats --json`, shadow channel) with zero bytes injected.

## 6. Revocation

The current repository-local deployment uses the private credential wrapper
recorded in the October 3 receipt. Preview removal of that exact entry:

```bash
sr uninstall-hook claude --settings-file /data/projects/skillranker/.claude/settings.local.json --binary-path /home/ubuntu/.local/libexec/skillranker/sr --timeout-secs 4
```

After reviewing the preview, repeat with `--apply`. Both the settings path and
wrapper path are required; the default settings path is global. Unrelated
settings and the global compact-check hook remain intact. Live preview and
isolated apply/repeat/conflict checks passed on October 3; the live hook has
not been uninstalled. The earlier customized shell command correctly refused
uninstallation until it was reconciled with this exact managed command.

Setting `network.enabled = false` in trusted user configuration stops Jev spend
from the existing hook while leaving local turn recording enabled. No consent
or allowance setting was changed during the October 3 redeployment.

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

## Credential delivery at the hook — 2026-09-23 22:37Z (FuchsiaCave)

Shell-rc sourcing did not reach the cohort. In the 27 hours after it landed,
every shadow row in this repository still recorded `credential-absent` (31 rows,
the last at 22:33Z). A read of each running Claude process's environment (checking
for the key's presence only, never its value) showed why. Hooks inherit the Claude
process's environment, and both Claude sessions working here had been launched
from interactive shells opened before the rc change. One of those shells dated
from 2026-09-17. Long-lived tmux and terminal shells never re-source `~/.zshrc`.
The only keyed Claude process was working in another project, outside the
project-local hook scope.

The managed entry in `.claude/settings.local.json` therefore now loads the
owner-only `.env` itself before exec'ing the pinned binary, so every session in
this repository gets the credential however it was launched:

```text
/bin/sh -c 'set -a; . /data/projects/skillranker/.env; set +a; exec /home/ubuntu/.local/bin/sr hook claude'
```

The key never appears in the settings file. `.env` is `0600` and holds only
`TYPESAFE_API_KEY`. The command still contains `hook claude`, so
`sr uninstall-hook claude` recognises it as the managed entry. A backup of the
previous settings file is kept outside the repository. Scope and consent are
unchanged: a project-local hook with trusted-user network consent.

Smoke, with an isolated ledger: the exact command, run from an empty environment
(`env -i`, as a keyless Claude process gives its hooks), with a real payload for
a session in this repository, completed a two-stage Jev evaluation. It exited 0
with zero stdout, took 2.6 s against the 4 s hook timeout, recorded `ranked`
with 18,109 input and 2,504 output tokens, and both attempts completed.

Whether a running Claude session picks up the edited hook without a restart
depends on Claude Code's settings reload. Sessions started after this change
certainly do. Live rows other than `credential-absent` for this repository are
the confirmation to look for.

## Redeployment — 2026-09-24 13:22Z (AzureJaguar, review fixes incl. secret redaction)

- Binary: `~/.local/bin/sr`, SHA256
  `25786998f434d902afd76706f3aa6a4784fee233297efaa029ef6c5c256bec55`, built
  `--release --locked` from a clean `git archive` export of the pushed revision
  `f556751`, with its sources touched so the build could not reuse stale
  artifacts. It adds the `sr-8ye6` review fixes over the previous deploy.
  These matter for the cohort the moment the credential reaches the panes:
  - `_`-prefixed secret assignments (`OPENAI_API_KEY=…`, `GITHUB_TOKEN=…`) are
    now redacted;
  - tool arguments are allowlisted before redaction;
  - a negated directive never becomes a requirement;
  - a skill reached through two names no longer fails the rank.
  The previous `ad28bdd` binary (SHA256 `e784d4db…`, this file's prior
  receipt) is kept as `~/.local/bin/sr.backup.20260924T132231Z-ad28bdd`.
- Pre-deploy smoke: `sr hook claude` replayed over the 80 most recent real
  session transcripts in an isolated sandbox (no key, no network, no real
  ledger). The outcome matched the installed binary on all 80.
- Post-install check: an offline `sr rank --dry-run` in an isolated HOME, whose
  request contains `OPENAI_API_KEY=abc123def`, previews a request containing
  `OPENAI_API_KEY=[REDACTED]`, and the value appears nowhere in the output.
- Not changed: hook scope (project-local), consent, and the credential (still
  `credential-absent` until agents start from fresh shells).

## Redeployment — 2026-09-24 17:24Z (AzureJaguar, live redaction refusal)

- Binary: `~/.local/bin/sr`, SHA256
  `835857a1a39da8cf7363a59fec37ef5b1201805aab2e3ddc79bac50699166356`, built
  `--release --locked` from a clean `git archive` export of the pushed revision
  `b3e7387`, with its sources touched. The build ran through the shared target
  directory, so the binary was copied out at once and identified by behavior
  (below), not by path.
  It adds, over the `f556751` deploy:
  - `sr-dq5c`: skill frontmatter with trailing comments, quote escapes or
    wrapped descriptions no longer excludes the skill;
  - `sr-4bql`: lost-cache recordings are counted;
  - `sr-b1ay`: the ledger keeps raw provider probabilities;
  - `sr-mk0r`: overflow retrieval admits only admitted skills;
  - `sr-p0ys`: truncation keeps redactions whole and visible;
  - `sr-wx8t`: a redacted quoted secret keeps its quotes.
  The previous binary (SHA256 `25786998…`) is kept as
  `~/.local/bin/sr.backup.20260924T172424Z-f556751`.
- Why now: two live shadow rows on 2026-09-24 (15:00:07Z and 16:03:41Z)
  recorded `unsupported-input`, "The context could not be made safe to send".
  Replaying the real transcript cut at 16:03:41Z offline (`--dry-run
  --offline`, isolated state) reproduced it with the installed binary and with
  an interim `ac046e3` build. Cause (`sr-wx8t`): redaction replaced a quoted
  value together with its quotes. `f(password="x")` became
  `f(password=[REDACTED])`, and the payload scan read `[REDACTED])` as an
  unquoted secret. The `b3e7387` binary produces the request for the same
  input, and all 120 recent fields of that cut pass the scan after redaction.
- Pre-deploy smoke: `sr hook claude` replayed over the 80 most recent real
  session transcripts in a keyless, network-less sandbox matched the installed
  binary on all 80.
- Gates on the exact revision: fmt, strict clippy, RCH full suite
  1365 passed / 0 failed / 10 ignored; ubs 0 critical.
- Credential correction: the previous receipt's "still `credential-absent`" is
  out of date. The settings entry that sources `.env` (FuchsiaCave,
  2026-09-23 22:37Z) delivers the key to every session. The last
  `credential-absent` row is 2026-09-23 22:33Z, and credentialed evaluations
  (ranked and abstain) have been recorded since 23:00Z that day. No agent
  restart is needed.
- Not changed: hook scope (project-local), consent, and the managed settings
  entry.

## Redeployment — 2026-09-24 18:17Z (AzureJaguar, window-stranded tool result)

- Binary: `~/.local/bin/sr`, SHA256
  `631128ee63926d393b0b0866147481f986d1843da3172e13feacf52e27f45a56`, built
  `--release --locked` from a clean export of the pushed revision `54eecc7`
  (sources touched; copied out of the shared target at once). It adds
  `sr-l1nr` over `b3e7387`. The previous binary (SHA256 `835857a1…`) is kept
  as `~/.local/bin/sr.backup.20260924T181719Z-b3e7387`.
- Why: the live row at 2026-09-24 02:16:04Z (`ambiguous-branch`) replays from
  the real transcript. The hook's 2 MiB tail began between one response's
  parallel tool calls and their results, stranding Claude's dead-end side
  result without its call, so every prompt was ambiguous until the window
  moved on.
- Verified on the real cut in sandboxed ledgers, with a placeholder key and a
  refusing loopback endpoint so nothing leaves the machine:
  - the previous binary: `ambiguous-branch` for both the notification turn
    and an ordinary prompt;
  - this binary: resolves both and stops only at the unreachable provider.
  The 16:03:41Z redaction replay still produces its request.
- Pre-deploy smoke: 80 most recent real transcripts in a keyless sandbox
  matched the installed binary on all 80.
- Gates on the exact revision: fmt, strict clippy, RCH full suite
  1367 passed / 0 failed / 10 ignored; the new test fails against the
  previous resolver.
- Not changed: hook scope, consent, and the managed settings entry.

## Redeployment — 2026-09-24 23:20Z (AzureJaguar, partial compaction)

- Binary: `~/.local/bin/sr`, SHA256
  `53d7e1d22f5dc111faf74b41e4aff6e7878e80c4f6877bce8b6dd532ed3360a7`, built
  `--release --locked` from a clean export of the pushed revision `7054402`
  (sources touched; copied out of the shared target at once). It adds
  `sr-8tlh` over `54eecc7`. The previous binary (SHA256 `631128ee…`) is kept
  as `~/.local/bin/sr.backup.20260924T232001Z-54eecc7`.
- Why: every real prompt moment in this project's six transcripts (78) was
  replayed through the deployed hook in a sandbox, with a placeholder key and
  a refusing loopback endpoint. 12 were `ambiguous-branch`, all caused by
  Claude's partial (`preservedSegment`) auto-compaction. Its boundary names a
  logical parent that is never written, or one rewritten after the boundary
  beneath it, which closes a cycle. Every later prompt in such a session
  failed. Live session `ee53c3fa` compacted this way at 21:46Z; the previous
  binary could not resolve it, and this one does.
- After: 0 of 78 are `ambiguous-branch`. Ten of the twelve now stop at
  `insufficient-context`, which is honest: this compaction format writes no
  summary to the transcript. Smoke over the 80 most recent transcripts: 79
  identical, 1 improved (`ee53c3fa`).
- Gates: fmt and strict clippy on `7054402`; RCH full suite 1374 passed /
  0 failed / 10 ignored on the same change one rebase earlier (`9c19470`);
  focused binaries on `7054402` pass. One intermittent
  `context_contract::branch_and_worktree` failure under load was rerun green
  four times and filed as `sr-bytg`.
- Not changed: hook scope, consent, and the managed settings entry.

## Redeployment — 2026-09-25 05:16Z (AzureJaguar, signals and atomic cache)

- Binary: `~/.local/bin/sr`, SHA256
  `5e0a98687b1f5ffc9a2b105470b13d38d3d6e9e70a2681b9c573d0c1426823b2`, built
  `--release --locked` through RCH from a clean export of the pushed revision
  `ca172d6`. Freshness was proven by a string only that revision contains,
  not by mtime. The previous binary (SHA256 `53d7e1d2…`) is kept as
  `~/.local/bin/sr.backup.20260925T051604Z-7054402`.
- Adds over `7054402`:
  - `sr-c4v6` (`01d437c`): SIGTERM and SIGINT cancel the running rank or
    hook, which drains and records the turn as an unavailable timeout.
    Before, the process died with its row in flight, the common outcome when
    Claude's hook timeout fires under load.
  - `sr-ron8` (`ca172d6`): a wide answer and its rerank are published in one
    transaction that also completes the lease, so the cache never pairs
    answers from two evaluations. A superseded leader stops before paying
    for a rerank.
- Pre-deploy check, now a repository tool:
  `python3 scripts/sweep_prompt_moments.py --compare <new sr>` replays
  every real prompt moment of this workspace's transcripts through both
  binaries. It runs in an owner-only sandbox with a placeholder key and a
  refusing loopback endpoint, and prints session:line and outcomes only.
  Result: 83 moments, identical outcomes. 71 reach the provider stage, 10
  are honest `insufficient-context` refusals after partial compactions that
  keep no summary, and 2 are `no-row` redeliveries sharing a prompt id with
  an earlier record of the same turn.
- Gates: `01d437c` remote full suite 1376/0/10; `ca172d6` remote full suite
  1375/0/10. Both fmt and strict clippy, and ubs 0 critical.
- Not changed: hook scope, consent, and the managed settings entry.

## Redeployment — 2026-09-25 05:58Z (AzureJaguar, idle notification turns skipped)

- Binary: `~/.local/bin/sr`, SHA256
  `b945c644ce24e0318bddff9f4158ae7e7366ae2cf77fbf2953abc773a670f699`, built
  `--release --locked` through RCH from the pushed revision `455037c`
  (freshness proven by a string only that revision contains). The previous
  binary (SHA256 `5e0a9868…`) is kept as
  `~/.local/bin/sr.backup.20260925T055814Z-ca172d6`.
- Adds `sr-sif6`: the trusted setting `hook.notification_turns`, default
  `skip`. A turn that a background task's `<task-notification>` started while
  the agent was idle is counted as a non-turn and never sent. Measured live,
  such turns were 36 of 63 credentialed evaluations and most of the provider
  spend. This deployment runs the default (`sr doctor --config`: `skip`, from
  `built-in`).
- Sweep with `--notifications` (the sweep tool gained the flag), previous
  binary against this one, over 226 real moments:
  - 141 notification turns moved from evaluated (`request-budget`) to
    skipped (`no-row`), and 2 load-overrun ones likewise;
  - one later notification record became its turn's first evaluation (a
    replay-order artifact);
  - the 83 submitted user prompts had identical outcomes.
- Cohort impact: the shadow ledger stops recording evaluations for idle
  notification turns. They appear as non-turn hook entries in `sr stats`.
  Relevance-corpus work that wanted them must opt in with `rank` in trusted
  configuration.
- Gates on `455037c`: fmt, strict clippy, remote full suite 1377/0/10; the
  public-contract and contract-matrix validators pass.

## Redeployment — 2026-09-25 06:36Z (AzureJaguar, cache schema v4)

- Binary: `~/.local/bin/sr`, SHA256
  `ee76564d4e4a8df12dfbb44b9a32ea18c8954258052f5c63a6b05964da7a8a83`, built
  `--release --locked` through RCH from `2c3dc46` (freshness proven by the new
  `received_boot_id` column name in the binary). The previous binary (SHA256
  `b945c644…`) is kept as `~/.local/bin/sr.backup.20260925T064500Z-455037c`.
- Adds `sr-4t02`: each cached response records its receipt on the boot clock,
  so a wall clock stepped back can no longer extend its life past the TTL.
  The response cache is now schema version 4.
- Planned cache reset: as the storage policy requires, the new binary refuses
  the old v3 store without migrating it. Rank would then run uncached until the
  store is replaced. The three v3 files were therefore renamed in place, not
  deleted, to `~/.cache/sr/cache.sqlite3{,-wal,-shm}` with the suffix
  `.retired-v3-20260925` (4 response rows, at most ten minutes of answers).
  The next hook invocation creates a fresh v4 store.
- Sweep of every submitted user prompt of the workspace's transcripts, the
  previous binary against this one: no moment's outcome differed. The same 13 moments right after
  compaction are refused locally by both (11 `insufficient-context`,
  2 `no-row`). A 5-moment sandbox smoke of the installed binary reached the
  provider stage on all 5.
- Pending: no live hook had fired by 06:57Z (all panes idle), so the live v4
  store's creation has not yet been observed.
- Gates on `2c3dc46`: fmt, strict clippy and the remote full suite (1379
  passed, 0 failed).
- Observed at 17:18Z: the live hook created the v4 store, its rows carry boot
  receipts, and both turns ranked.

## Redeployment — 2026-09-25 19:56Z (AzureJaguar, cheaper skill discovery)

- Binary: `~/.local/bin/sr`, SHA256
  `4009260592e160e9802fe98e516913fa023c27ee6d1673c4d3ac06078b3ca764`, built
  `--release --locked` through RCH from `2c77b46` (freshness proven by a
  string from `11ba4dc` that the previous build lacks). The previous binary
  (SHA256 `ee76564d…`) is kept as
  `~/.local/bin/sr.backup.20260925T195800Z-2c3dc46`.
- Discovery no longer walks a skill's own subdirectories. On the live roots
  `sr roster --json` output is byte-identical to the previous binary (205
  records, same counts and causes). openat falls from 5,667 to 1,117 calls,
  and median CPU time from 388 ms to 224 ms.
- Also carries `11ba4dc` (live evaluation batches, `sr eval --online`), which
  the hook does not use.
- The sweep tool now skips compaction summaries and harness meta records. The
  13 "local refusals" of earlier sweeps were such records, not live failures.
  Sweep, previous binary against this one: all 74 submitted prompts end at the
  provider stage with both, and no outcome differs.
- Gates on `2c77b46`: remote full suite 1383 passed, with 2 wall-clock-bound
  failures on a slow run (writer lock 576 ms, Retry-After 655 ms), both green
  on rerun (storage_contract 25/0, transport_failures 8/0); strict clippy on
  the change.

## Redeployment — 2026-09-25 21:53Z (AzureJaguar, Rerank reuses the Wide connection)

- Binary: `~/.local/bin/sr`, SHA256
  `9c1c026a80eef8e32c3c14e14b1d9db501b13d880f77f033423f6334222699c3`, built
  `--release --locked` through RCH from `075f928` (freshness proven by the
  `send_stage_accounted` symbol, absent from the previous build). The previous
  binary (SHA256 `40092605…`) is kept as
  `~/.local/bin/sr.backup.20260925T212500Z-2c77b46`.
- Live turns split into about 0.9-1.2 s of provider exchanges plus the local
  work. Each exchange opened its own TLS connection: 323 ms fresh against
  94 ms reused over HTTP/1.1, measured with curl without credentials. The
  Wide attempt now keeps its connection for the Rerank, and the Rerank closes
  it. Before this deploy, live Rerank sent-to-completed times were 485-640 ms.
- Also carries the peers' `l1i.6.20` evaluation work up to `075f928`, which
  the hook does not use.
- Sweep, previous binary against this one: all 76 submitted prompts end at
  the provider stage with both, and no outcome differs.
- Gates on `075f928`: remote full suite 1391 passed, 0 failed; strict clippy
  and ubs on the change.
- Pending: no live turn had run on this binary by 21:55Z. The expected effect
  is a shorter Rerank sent-to-completed time in `provider_attempts`.
- 21:58Z: the first live turn on this binary never finished. Its row stayed
  `in-flight`, with the Wide attempt `sent` and never completed. The binary
  was rolled back to `40092605…` at 22:00Z while this was investigated.
- Investigation, with synthetic input only: an invented one-prompt transcript
  against the production endpoint.
  - With a placeholder key (401), both binaries behave the same.
  - With the maintainer key, 7 of 7 turns on this binary completed. Rerank
    sent-to-completed was 175-289 ms, against 480-497 ms for the previous
    binary on the same input (2 turns). About 280 ms is saved per ranked turn.
- Diagnosis: at 21:58Z the host's load average was about 100. The failed turn
  spent 1.7 s on local work before its Wide attempt left, leaving about
  1.1 s of a 3 s budget for the provider. It matches `sr-9fzp`: a run that
  overruns its deadline cannot record its failure and stays `in-flight`. The
  previous binary behaves the same under that load; this change shortens the
  turn.
- Redeployed `9c1c026a…` at 22:05Z.
- Live turns afterwards, all finished (no `in-flight` rows):

  | Time (UTC) | Outcome | Elapsed | Wide | Rerank |
  | --- | --- | --- | --- | --- |
  | 22:37 | ranked | 2,533 ms | 850 ms | 410 ms |
  | 02:00 | abstain | 846 ms | 372 ms | 193 ms |
  | 02:01 | ranked | 983 ms | 296 ms | 259 ms |

  Before the change, live Reranks took 374-640 ms. Host load at 02:00Z was
  about 30, against about 100 at 21:58Z, so part of the lower total is load.
  The Rerank times match the synthetic measurement.

## Redeployment — 2026-09-27 04:31Z (AzureJaguar, budget, breaker and snoozes)

- Binary: `~/.local/bin/sr`, SHA256
  `143c23944d4d15607285bbaf5bc888865fcffd36796b42ffe207fe4ce5d8d5dc`, built
  `--release --locked` through RCH from `9ac7caf` (freshness proven by the
  `sr budget` help string, which the previous build lacks). The previous binary
  (SHA256 `9c1c026a…`) is kept as `~/.local/bin/sr.backup.20260927T043000Z-075f928`.
- Carries the peers' P6 work since `075f928`: the shared request allowance
  (`sr budget`), the fenced provider breaker, snoozes, the sr-9fzp overrun
  finalization, and the sr-azlc lease retry. Also `9ac7caf`, which caps a
  persisted provider Retry-After at one hour (sr-0f7i item 1).
- No allowance is configured: `sr budget` reports `Allowance: disabled`. With
  no guard file, a hook attempt checks one path and opens no accounting store.
- Sweep over the 88 submitted prompts, previous binary against this one: every
  moment reaches the provider stage with both. With the refusing endpoint, the
  new breaker opens after three transient failures, so this binary records
  `provider-cooldown` where the previous one recorded `request-budget`. The
  sweep script now counts `provider-cooldown` as the provider stage.
- Live check with synthetic input (an invented one-prompt transcript, maintainer
  key, 4 provider requests): both turns completed, in 807 ms and 993 ms end to
  end, with Reranks of 190-199 ms.
- Gates on `9ac7caf`'s tree: remote full suite 1468 passed, 0 failed; strict
  clippy clean. `0d2b43c` alone: 1467 passed, 0 failed.

## Redeployment — 2026-09-27 17:37Z (AzureJaguar, hook-path review fixes)

- Binary: `~/.local/bin/sr`, SHA256
  `89d6525a1921a85f56f945847bc393f7df5e0a088dc106e866ebb711467196ed`, built
  `--release --locked` through RCH from `ed60dc4` into a target directory
  private to that build tree. The previous binary (SHA256 `143c2394…`) is kept
  as `~/.local/bin/sr.backup.20260927T174000Z-9ac7caf`.
- Adds the hook-path fixes since `9ac7caf`:
  - `6929d63` (sr-0f7i): the breaker store is created only on a failure, busy
    waits are bounded, the late-failure window is 3.5 s, and lease retries
    only on busy;
  - `16fbfd7`: an unrepresentable Retry-After saturates to the one-hour cap
    instead of being dropped;
  - `ed60dc4` (sr-azlc): a contended cache store open is retried for 400 ms
    instead of five tries.
- Sweep over the 90 submitted prompts, previous binary against this one:
  identical outcomes, all at the provider stage.
- Live check with synthetic input (maintainer key, 4 requests): both turns
  completed, in 681 ms and 969 ms.
- Gates at `ed60dc4`'s tree: remote full suite 1472 passed, 0 failed; strict
  clippy clean.
- 21:12Z: 3 of the 4 live turns after this deploy were `in-flight` (18:03:58,
  18:04:06, 21:11:07), so the binary was rolled back to `143c2394…` while
  this was investigated.
  - The stuck turns sit in two sessions at usage-limit interruptions and
    resumptions of those Claude Code sessions. Two had both stage requests
    `sent` and never completed; the third was admitted and never sent.
  - Synthetic turns under the same host load (average 57): 3 of 3 completed on
    each binary.
  - SIGTERM mid-request, with a 1.2 s grace before SIGKILL like the harness:
    both binaries exited within 0.1 s and recorded the turn
    (`unavailable/timeout`, the open attempt `unknown`), or completed it.
  - So those turns were killed without a SIGTERM, which no process can record,
    and the ledger honestly counts them as in flight or killed. Redeployed
    `89d6525a…` at 21:14Z.

## Redeployment — 2026-09-29 18:48Z (AzureJaguar, Cloudflare provider on main)

- Binary: `~/.local/bin/sr`, SHA256
  `72ddf91fe668074b322c9ad5404007192303013a225bb42c020a0263d17b032f`, built
  `--release --locked` through RCH from `5f71eac`. The previous binary (SHA256
  `89d6525a…`) is kept as `~/.local/bin/sr.backup.20260929T185000Z-ed60dc4`.
- Carries the optional Cloudflare-hosted Jev transport (c031d75, 6a3245d,
  5f71eac). This host runs the default TypeSafe provider, whose request
  bytes, endpoint and allowance and breaker keys are unchanged. This host's
  CLOUDFLARE_* variables pass the new strict validation (exit-code check
  only); sr-sgca tracks the variables that do not.
- An earlier sweep under load average 180 showed only load noise: both
  binaries overran on the same moments. Rerun on a quiet host (load average
  4.6), all 104 submitted prompts had identical outcomes.
- Live check with synthetic input (maintainer key, 4 requests): both turns
  completed, in 456 ms and 608 ms.
- Gates at `5f71eac`'s tree: remote full suite 1532 passed, 0 failed; strict
  clippy clean; rustfmt fixed in 4244c14 (formatting only).

## Redeployment — 2026-10-03 (BeigeCompass, verified source catch-up)

- The existing project-local shadow hook now loads the release binary from frozen
  source `f820005ee478568ecf7adb2154712bb058c5109a`. This brings the deployed
  `5f71eac` build forward to the selected-provider configuration repair, bounded
  provider-specific wire previews, and the smaller dynamic-marker parser scan.
  `999b431` changes only the tracker ownership record; its production source,
  dependencies and toolchain are identical to the compiler source.
- Binary: `~/.local/bin/sr`, SHA256 `7d1abb6b16dbe1ca743641b38cdb25bbe16fbfad44077372bab0a88995c6085e`; size `23504360` bytes, mode
  `0755`. Deployment completed at `2026-10-03 04:42:09 UTC`. The prior executable,
  SHA256 `72ddf91fe668074b322c9ad5404007192303013a225bb42c020a0263d17b032f`,
  is preserved at `~/.local/state/sr/deployments/20261003T044209Z-5f71eac/sr`.
  Its hash was checked before and after the atomic replacement. Cooperating installers are locked; hashes, inodes and
  modes are checked again immediately before replacement. This does not claim
  compare-and-swap against noncooperating external editors.
- Build: DSR `0.2.1` with private configuration/state, run `9d6099ef-873b-4f0a-ab8d-dea8514b8216`, delegates
  compilation through required-remote RCH on `ovh-b`:
  `cargo build --locked --release --target x86_64-unknown-linux-gnu --bin sr --jobs 4`.
  RCH source is exactly `f820005`, with no working-tree overlay, fingerprint
  `fe0d5151031be8fda7951fe7fe1f42f7ce344018fdb3ed21e6ada866b230b195`.
  The actual native ELF, archived executable and installed bytes agree.
  The worker's pinned `nightly-2026-08-31` reports Rust `1.100.0-nightly`,
  commit `90850177249efe0321573c569aec5d12b257f8d6`, LLVM `23.1.0`.
  Default features are empty; `Cargo.lock`, toolchain and `LICENSE` are unchanged.
  This is a private Linux x86-64 native deployment, not a portable release,
  advisory promotion, other-platform qualification or new tag. DSR's ordinary
  `publishable` metadata is not one of those acceptance claims.
- Failed build-wrapper attempts remain recorded: one RCH argument rejection
  before compilation, and two packaging failures after successful remote native
  builds because DSR expected a target-triple subdirectory. A boolean opt-out
  was ineffective with the installed tooling. The final wrapper passes the
  target explicitly to remote Cargo and retains architecture validation.
  Unsupported DSR option attempts exited before build admission. No local
  Cargo fallback, global build configuration change or GitHub Actions was used.
- Verification: `/scratch/tmp/sr-shadow-candidate-0ukvc9eb/report.json` records 20 positive/adversarial comparison records
  from 32 actual CLI executions, using bounded synthetic input, credential-free
  environments, offline mode and private HOME/ledgers. Explicit resolution,
  missing requirements, offline misses, dry-run, strict parsing and provider
  selection behaved as declared before execution. A positive shadow hook
  produced empty stdout/stderr and exactly one explicit turn with zero emissions;
  malformed and unsupported input remained quiet on stdout with sanitized stderr.
  Malformed inactive Cloudflare environment blocks the old TypeSafe build but
  permits the new explicit decision; selected malformed Cloudflare still refuses.
  `/scratch/tmp/sr-shadow-candidate-jxqchks4/report.json` checks the installed path against the preserved old
  executable. These are actual binary/input-contract checks, not real Claude
  delivery, organic traffic, live provider success, performance or quality proof.
- The maintainer's actual roster output matched byte for byte before installation:
  47,722 bytes, SHA256
  `1e62f8e16e819057666683eed1213793db43cfd70052d96be9e7dc562cb80523`.
  There is no latency claim from this single pair. Source-level acceptance already
  passed on the same committed production bytes: full remote default-feature
  suite 1,546 passed, zero failed, ten pre-existing ignored entries; all-target
  check and pinned strict Clippy passed. Those are the October 1 receipts, not
  newly executed full-suite results for this deployment.
- Rollback repair: the prior credential-loading shell command did not match the
  installer's managed command; an actual project-local uninstall preview refused
  it with exit 2. The installer correctly protects modified entries. Reconciled
  only `hooks.UserPromptSubmit[0].hooks[0].command` to
  `/home/ubuntu/.local/libexec/skillranker/sr hook claude`, with a private wrapper
  (SHA256 `fbf531ecefed9d4e49ac9c799f732951b7f9276baf24443c4af472b3aecdc965`):

  ```sh
  #!/bin/sh
  set +x
  set -a
  . /data/projects/skillranker/.env
  set +a
  exec /home/ubuntu/.local/bin/sr "$@"
  ```

  The wrapper's parent is owner-only (`0700`); `.env` ownership and `0600`
  were checked without printing its value. It uses the same credential file
  and executable as before. Original settings bytes are preserved at
  `~/.local/state/sr/deployments/20261003T044209Z-5f71eac/claude-settings.local.before-wrapper.json`.
  All other project bytes, the four-second timeout, trusted config and global
  settings are unchanged; global settings have no `sr` hook.
  Consent stays repository-scoped, mode stays shadow and raw capture stays off.
  Eight actual isolated installer cases passed: install/uninstall and repeats,
  unrelated-field preservation, modified-command and malformed-file refusal,
  and owner-only backups. The actual wrapper also passed 20 offline comparison
  records (`/scratch/tmp/sr-shadow-candidate-x1c3o1d9/report.json`). Live install
  preview reports already installed, and live uninstall preview succeeds without
  modifying settings. No live uninstall was applied. See §6 for exact commands.
- The shared allowance remains disabled. The original pre-registration's
  statement that allowance tooling was absent describes September 21; tooling
  now exists but no cap has been configured here. No new public request, budget
  change or expansion of network authorization occurred during this deployment.
- Read-only before/after stats: all counts and outcome states are unchanged;
  the query end timestamp advances with the clock. The whole mixed-history
  ledger contains 170 evaluations, 167 shadow rows, zero emissions and zero
  independent judgments; 234 provider attempts, 14 unknown-usage attempts and
  2,394,033 known tokens. Entry counters retain 312 entries, 135 recorded,
  154 non-turn deliveries and 23 unrecorded (an upper bound, not proven outages).
  Private smoke rows do not enter that store. No new qualified organic turn on
  this binary is claimed. Earlier revisions, out-of-scope history and unfinished
  rows cannot be pooled into a revision-qualified passing cohort.
- `sr-1uf4` remains open: at least 500 representative real invocations with
  revision/harness/model/policy/population identity, cold/warm/cache/outage strata,
  all-invocation fallback and latency denominators, memory and known/unknown
  usage are still required. Independent relevance labels, prospective controlled
  outcomes and advisory promotion retain their separate gates. This deployment
  enables ordinary existing shadow traffic to exercise verified source.

## Redeployment — 2026-10-07 (BeigeCompass, current hook and accounting fixes)

- The existing repository-only shadow hook now loads frozen source
  `fe399ab1d223bb7ff0c0f6294913d75b675a1573`. It includes the successful
  optional-ledger warning (`aeeac47`), invocation-scoped provider accounting
  (`54ba5ae`), configured roots for normalized sessions (`7a0ec75`), and
  validation before hook invocation counting (`fe399ab`).
- Live inspection first found an executable newer than this document's October 3
  receipt. Its hash matched the preserved October 6 native build/install receipts
  for `b46bdc7`; it was identified before any replacement. The prior executable
  `d0a4c7e8dc8b89baaf2f76a15b438d15016ed8d6ea745e3d2bcefb8f964e0af0`
  is preserved at `~/.local/state/sr/deployments/20261007T175836Z-b46bdc7/sr`.
- Installed at `2026-10-07 17:58:36 UTC`, mode `0755`, 25,038,976 bytes, SHA256
  `7e230d0c8d2a791c60e440232ece235fc647df0966ed476cea8df0d185b1b15d`.
  The cooperating-installer lock, file hashes/inodes/modes, backup, atomic
  replacement and file/directory fsync checks passed. Permissions were set before
  file fsync. Noncooperating editors are not protected by a compare-and-swap.
- DSR `0.2.3` run `657b65f5-4558-4772-8b97-55e119fe5f14` passed through
  required-remote RCH on `vmi1264463`, one build job:
  `cargo +nightly-2026-08-31 build --locked --release --target x86_64-unknown-linux-gnu --bin sr -j1`.
  Actual Rust is `1.100.0-nightly`, commit
  `90850177249efe0321573c569aec5d12b257f8d6`, LLVM `23.1.0`. All 285 frozen
  compiler inputs match the qualified source and final worker bytes; manifest
  SHA256 `e8187b8b734011a35d5606a73e8ef57474e6635bf4d651466846e388567767a9`.
  The only working-tree change during compilation was task ownership. Native
  ELF64/x86-64, archived executable and installed bytes agree. The archive
  `3434a2af64edfa35033f46a189411beedbc042bc165e8fe654f844283bb37787`
  contains exactly `sr` and verbatim `LICENSE`. Default features remain empty.
  This is a private native deployment, not a portable release, advisory promotion,
  new tag or other-platform qualification.
- Two failed preparations are retained: DSR `cdaf0c46` rejected an unrelated
  ambient Cargo-cache license symlink before RCH admission; `2e160bff` reached
  remote Cargo but lacked the pinned Asupersync checkout in offline mode
  (Cargo exit 101). The successful run used a supported empty private local
  Cargo seed because compilation delegates to RCH, with locked dependency
  downloads into RCH's normal worker cache. No shared cache edit, DSR guard
  bypass, local compilation, timeout increase or GitHub Actions was used. DSR's
  local cache receipt does not certify the remote compiler's dependency isolation.
- Candidate and separately installed-path checks each passed 28 comparison
  records from 40 actual offline CLI executions with synthetic bounded inputs
  and private HOME/SQLite stores. All three invalid preflight controls append
  54 bytes on the old binary and zero on the new binary; valid hooks still count.
  Explicit decisions, missing requirements, offline misses, dry-run, parsing,
  provider configuration and shadow silence retain their successful controls.
  These are input/SQLite contracts, not real Claude delivery or organic traffic.
  The existing roster matched byte for byte in a fresh pair: 47,722 bytes,
  SHA256 `e3fd5e68a7740cb6fb2bef6d3bd635d398cc0909879dce5375ba6515314d5b82`.
- Wrapper verification passed 29 records/42 executions. Its initial reused
  harness failed one expectation: the actual credential file overwrites planted
  malformed inherited Cloudflare credentials. The failed report is retained; a
  separate wrapper test explicitly checks that precedence with zero HTTP usage
  and adds an invalid-provider refusal. The direct-binary malformed-selected-
  provider refusal remains unchanged and passed. No credential file was edited.
  Eight isolated installer controls passed on the old, candidate and installed
  binaries, including modified-command refusal, repeats, unrelated settings and
  private backups. Actual install/uninstall previews succeeded before and after
  without changing settings; the exact revocation command in §6 still applies.
- The wrapper remains SHA256 `fbf531ecefed9d4e49ac9c799f732951b7f9276baf24443c4af472b3aecdc965`.
  Project/global/trusted configuration hashes and the four-second outer timeout
  are unchanged. Owner-only credential permissions were checked without printing
  values. Repository-only consent, shadow mode, raw-capture-off and disabled
  shared allowance are unchanged. No new public Jev call occurred.
- Read-only ledger counts remain 170 evaluations/167 shadow rows, zero emissions
  and independent judgments, 234 attempts, 14 unknown-usage attempts and
  2,394,033 known tokens; counter 312/recorded 135/non-turn 154/unrecorded 23
  (an upper bound, not proven outages). A coarse comparison initially mistook
  nested query-clock changes for data changes. Separate verification checked
  their exact retention/query relationships and equality of every remaining
  field, including unknowns. Private controls did not enter the real ledger.
- Source qualification is the previously executed same-input default suite:
  1,583 passed, zero failed, eleven existing opt-in ignored entries, with
  all-target check, strict Clippy and formatting passed. It was not rerun for
  this executable-only deployment. Fresh verification was by the author; no
  independent review, live provider success, runtime latency or usefulness claim.
  Raw receipts and failures: `/scratch/tmp/skillranker-shadow-upgrade-20261007-_ehsgz5u`.
- `sr-1uf4` remains open with its original requirements: at least 500
  representative real invocations and the declared revision/harness/model/policy/
  population, strata, fallback/latency/memory and known/unknown usage evidence.
  Relevance, paired outcomes, native platform and original `sr-9fzp` gates remain
  separate. Ordinary future traffic can now exercise the current fixes.

## Redeployment — 2026-10-07 (BeigeCompass, observation boundary repair)

- Installed source `06bfabd3e221f33e251477b48165724087684efe` at
  `2026-10-07 21:28:52 UTC`. The installed CLI now includes the shared
  configuration validation, bounded regular-file reading, configured roots and
  harness-specific inventory policy for `observe` (`sr-roadmap-l1i.6.31`).
- Executable SHA256:
  `069f8d8b69c27d9dc35d66edf91d08c9fd5eeb4ca85d3e8a4e1c7ce6ce3bd4aa`,
  25,144,192 bytes, mode `0755`. The preceding `fe399ab` executable remains at
  `~/.local/state/sr/deployments/20261007T212852Z-fe399ab/sr`, SHA256
  `7e230d0c8d2a791c60e440232ece235fc647df0966ed476cea8df0d185b1b15d`.
  Cooperating lock, identity rechecks, atomic replacement and file/directory
  fsync passed; concurrent noncooperating editors still lack a CAS guarantee.
- DSR run `b43ce784-6288-4861-932d-c4108b7b2626` passed in 279.897 seconds
  through mandatory remote RCH on `vmi1264463`, using the same pinned native
  release command and one build job as the preceding receipt. All 285 compiler
  and test inputs match committed `06bfabd`, the worker and the final qualified
  source manifest. Only tracker ownership changed during compilation. ELF64
  x86-64 qualification passed. Archive SHA256
  `39abdd2429a65d68ea7e34f62772b00e088c8e2f96191b41d73e90a830d6c607`
  contains exactly the identical executable and verbatim `LICENSE`.
- Native observation controls used synthetic bounded events, isolated HOME and
  actual SQLite. The original executable reproduced the declared defects in
  21 controls/38 CLI executions; the candidate and fresh installed executable
  each passed the repaired expectations for the same 21 controls/38 executions.
  Positive controls include two configured loads with exact skill IDs and
  idempotency, explicit inventory replacement, valid Claude input and safe
  aliases. Invalid configuration and escaping/nonregular input cannot advance
  cursors; the original FIFO control was killed and reaped by its supervisor.
- Candidate and installed-path CLI/hook comparisons each passed 28 records;
  the actual wrapper passed 29. Eight isolated installer controls passed for
  each of the original, candidate and installed executables. Live install and
  uninstall previews are byte-identical before/after; wrapper, settings, trusted
  configuration, consent and four-second hook timeout are unchanged. No credential
  file was edited; owner-only credential permissions were rechecked.
- Retained validation failures: `budget status` is unsupported (corrected to
  `budget --json`); one post-install comparison named a nonexistent backup;
  another inherited the Python driver's stdin and correctly received input-mode
  refusals on both executables. The corrected comparison derives the backup
  from its receipt and supplies empty stdin. Expectations, product code and
  existing tests were not relaxed. CASS timed out after six seconds, limiting
  historical coverage.
- The prior same-input source suite passed 1,592 tests, with zero failures and
  eleven existing opt-in ignores, plus all-target check and strict Clippy. Those
  checks were not rerun for this executable-only deployment. Fresh native
  verification is author-only, not independent review or real Claude delivery.
  No provider requests or organic cohort traffic were generated. Raw receipts:
  `/scratch/tmp/skillranker-observe-native-20261007-f0d9bruf`.
- `sr-1uf4` retains its original 500-invocation and cohort requirements and stays
  open. Independent relevance/paired-outcome gates and the original `sr-9fzp`
  timeout cause remain unresolved. This receipt qualifies a private native Linux
  deployment, without advisory promotion or other-platform qualification.

## Redeployment — 2026-10-08 (BeigeCompass, native session discovery)

- Installed source `87dabdab4d0e7e2b6309e11babbd3fa6a9c7e100` at
  `2026-10-08 04:45:48 UTC`. Native session discovery now checks the original
  invocation deadline and cancellation during its walk, preserves timeout
  classification, and rejects impossible Gregorian dates as recency
  (`sr-roadmap-l1i.4.15`). Stalled kernel filesystem operations remain outside
  the cooperative-interruption guarantee; the original `sr-9fzp` cause is open.
- Executable SHA256:
  `d6f25bea819be64addbaee4fff53ee758030ebc6e725552f09ba39784f829030`,
  25,169,672 bytes, mode `0755`. The preceding `06bfabd` executable is retained at
  `~/.local/state/sr/deployments/20261008T044548Z-06bfabd/sr`, SHA256
  `069f8d8b69c27d9dc35d66edf91d08c9fd5eeb4ca85d3e8a4e1c7ce6ce3bd4aa`.
  Cooperating lock, identity rechecks, atomic replacement and file/directory
  fsync passed. These checks do not provide CAS against noncooperating editors.
- DSR run `44c052bb-8c3c-4228-b631-5b005e8f51f8` passed in 1,215.432 seconds
  through mandatory RCH on `vmi1264463`, with the pinned native release command,
  default features and one build job. All 285 compiler/test inputs match the
  committed source, worker and qualified manifest
  `115477548261eb6d98cf071cd16de9a08a40cd3299fcdd46d9c9e6556573a7a3`.
  Preserved untracked peer paths are excluded from that source identity.
  ELF64 x86-64 and license qualification passed. Archive SHA256
  `28ab4b91a4e130242e66708bb275b17eb90d1da8bb1b660a6ef002d2729adcd6`
  contains exactly the identical executable and verbatim `LICENSE`.
- The same source passed the full default suite: 1,596 tests, zero failures,
  eleven unchanged ignored entries (paid/manual checks and subprocess entry
  points), plus all-target check, strict Clippy and formatting. A separate
  fresh author execution passed 207 focused tests.
  All successful remote gates verified the same 285 input hashes afterward.
  UBS remained nonzero with reviewed public-name/mode comparison heuristics;
  no suppression was introduced. The aborted baseline and incorrect initial
  test selector executed no tests; their failures remain in the raw evidence.
- Five predeclared native cases used synthetic input and the actual CLI.
  The original executable reproduced the wrong timeout classification and
  impossible-date selection. Candidate and fresh installed-path runs passed
  all five repaired expectations, including exact-source and valid-date
  positives. The installed large-directory case returned timeout exit 6 at
  103.711 ms, versus the original insufficient-context exit 7 at 263.958 ms.
  These are individual observations, not a benchmark or hard real-time proof.
  Candidate and installed CLI/hook comparisons each passed 28 records; the
  actual wrapper passed 29. Eight isolated installer controls passed for each
  candidate and installed executable.
- The first native build, DSR `2ff55bcc-2a49-4dad-a9fb-c4fb0725bbbd`, failed
  after Cargo could not process dependency information: all 873 referenced
  Asupersync Git-source paths were absent. The generated dependency-info file
  itself existed; an initial diagnosis saying otherwise was corrected.
  SBH quarantine record `b59a19df7c8c` records the Git cache move during the
  active compilation. Official `sbh protect` added markers and protection
  registry entries for `/data/tmp/rch-cargo-cache-vmi1264463` and the held copy
  `/data/tmp/.sbh/quarantine/b59a19df7c8c`. Both protections are retained.
  The successful ordinary locked retry fetched pinned dependencies into the
  protected cache, with source, profile, commands and timeouts unchanged.
- Wrapper, settings, trusted configuration, consent and four-second hook
  timeout are unchanged; real install/uninstall previews match byte for byte.
  Credential files were not edited, and owner-only permissions were checked.
  All operational statistics remain unchanged: 170 evaluations, zero emitted
  suggestions, zero independent judgments, 234 attempts, fourteen unknown-usage
  attempts and 2,394,033 known tokens. Four statistics query-time fields were
  validated before comparison. The first private comparison also treated the
  budget's displayed UTC hour as durable state and failed across an hour
  boundary. Its original script and snapshots are retained. The corrected
  check validates both hourly windows against the source formula and compares
  every other budget field exactly; the allowance guard remains disabled.
- This is author-only native Linux verification. No provider requests,
  organic cohort traffic, independent relevance labels, real Claude delivery
  or other-platform acceptance were established. `sr-1uf4` retains its original
  cohort requirements. Raw failures, source manifests, controls and deployment
  receipt: `/scratch/tmp/skillranker-session-deadline-q1l380g6`.

## Redeployment — 2026-10-08 (BeigeCompass, delivery persistence)

- Installed source `dc17cc54c0a1e2baebf887c8b4cfba741ce48d6d` at
  `2026-10-08 11:51:29 UTC` (`sr-roadmap-l1i.6.32`). Post-output delivery
  recording now requires both an enabled ledger policy and confirmation of
  the actual ranking-event write. Rendered JSON, including a preview's nested
  decision or cache-only persistence metadata, cannot authorize that write.
  JSON and table decisions share the recording path; advisory hooks honor
  the same opt-outs, and recorded shadow decisions remain prepared.
- Executable SHA256:
  `ca1ddcc5c1ca80eaae41ed0a63f450402cba57159b0dbf86d98d825207b5d3a1`,
  25,154,120 bytes, mode `0755`. The preceding `87dabda` executable remains at
  `~/.local/state/sr/deployments/20261008T115129Z-87dabda/sr`, SHA256
  `d6f25bea819be64addbaee4fff53ee758030ebc6e725552f09ba39784f829030`.
  Cooperating lock, identity rechecks, atomic replacement and file/directory
  fsync passed; these do not provide CAS against noncooperating editors.
- DSR run `0d3963e5-1db3-497f-aabf-405dbb737d37` passed in 1,504.050 seconds
  through mandatory RCH on `vmi1264463`, using the pinned native release
  command, default features and one build job. All 285 compiler/test inputs
  match the committed source, local/worker trees and qualified manifest
  `9bf2e58a0321cf2bdf553ef1c945ef4e59ead6dc81257d62eaa6637be4ac4fc4`.
  Preserved untracked peer paths are excluded from that source identity.
  ELF64 x86-64 and source-linked artifact checks passed. Archive SHA256
  `a6510c52297239bfa786967865cb43649ac20fe2cb2d38c0a13e2f5aa8ec4beb`
  contains exactly the identical executable and verbatim `LICENSE`.
  No public release or additional platform was qualified.
- The same frozen source passed 192 focused tests, all-target check, strict
  all-target Clippy, formatting, and the full default suite: 1,601 tests,
  zero failures, eleven unchanged paid/manual or subprocess entry-point
  ignores. The full suite re-executed all five new regressions after fresh
  source review. No redundant third focused run was counted. UBS diff/staged
  scans remained nonzero (four critical, 701 warnings, 418 informational):
  the critical findings are two unchanged test panic assertions and two
  public stage-name comparisons misclassified as secret comparisons.
  No suppression, assertion relaxation, timeout or dependency change was made.
- Actual native old/candidate and old/installed comparisons each passed all
  26 predeclared delivery records using synthetic input and real SQLite.
  The old executable reproduced opt-out/dry-run ledger writes and missing
  table emission. Candidate and installed versions preserve all ledger table
  contents under opt-outs/previews while retaining useful explicit output,
  and record authorized JSON/table/advisory delivery. CLI/hook smoke checks
  passed 28 records for each candidate and installed path; the actual wrapper
  passed 29. Eight isolated installer controls passed for each new executable.
- Retained failures: the first draft confused coarse serialized persistence
  with a confirmed ledger write; author review caught it, cancelled its
  compilation before tests, and replaced it with a separate local write fact.
  A numeric cancellation selector was refused; exact wrapper cancellation
  required official recovery before the source owner was released. The
  intermediate source-acquisition refusal (103), recovered termination (137),
  stale local-manifest refusal and incorrect private input setups remain in
  the raw evidence. The first source push omitted `AGENT_NAME` and was
  refused; the registered-identity retry passed with the guard enabled.
  CASS and later Agent Mail timeouts limit history and coordination evidence.
- Wrapper, settings, trusted configuration, consent and the four-second hook
  timeout are unchanged; real install/uninstall previews match byte for byte.
  Credential files were not edited and owner-only permissions were checked.
  Operational totals remain 170 evaluations, zero emitted suggestions, zero
  independent judgments, 234 attempts, fourteen unknown-usage attempts and
  2,394,033 known tokens. Four query-time fields and both displayed budget
  windows were validated before comparing every other field; the allowance
  guard remains disabled. No public provider or organic cohort traffic was
  generated. Verification is author-only and does not establish real Claude
  delivery, independent relevance or policy/advisory promotion. The original
  `sr-9fzp`, `sr-1uf4` and corpus/cohort gates remain open. Raw evidence and
  deployment receipt: `/scratch/tmp/skillranker-emission-review-kx5pmyjg`.

## Redeployment — 2026-10-08 (BeigeCompass, owned cancellation and completion time)

- Installed source `a5a27c2046582072b1d00b7a7339ec055d8fc338` at
  `2026-10-08 22:26:06 UTC`. This activates the already-qualified `3f5c18b`
  owned-cancellation drain and `78ad660` monotonic completion-time repairs.
  Cooperative cleanup runs before bounded teardown; a timely prepared ranking
  survives a delayed CLI continuation. Genuinely late work remains withheld.
- Executable SHA256:
  `9df692b0c5c024359532e66e71e9a845eadbce6f7c8d398af41ac0cf46be0a1c`,
  25,165,232 bytes, mode `0755`. The previous `dc17cc5` executable is preserved
  at `~/.local/state/sr/deployments/20261008T222606Z-dc17cc5/sr`, SHA256
  `ca1ddcc5c1ca80eaae41ed0a63f450402cba57159b0dbf86d98d825207b5d3a1`.
  Cooperating lock, inode/hash/mode rechecks, backup, atomic replacement and
  file/directory fsync passed; this is not CAS against noncooperating editors.
- DSR run `907cc67e-7041-499c-9f78-c77e1dd6dc7c` passed in 268.446 seconds
  through mandatory RCH on `vmi1264463`, locked native Linux x86-64 release,
  default features and one job. All 285 committed compiler/test inputs match
  the frozen local and worker manifest. The dirty-tree marker reflects Beads
  activity; preserved untracked peer paths are outside that source identity.
  ELF64 x86-64, archive bytes and source-linked artifact checks passed. Archive
  SHA256 `d813510a70d814d7943b0f79b89e62ce4e9ce960430bd296b93d33a800545d66`
  contains exactly the identical executable and verbatim `LICENSE`. No tag,
  public release, other target or provider qualification was created.
- Production/compiler/test source bytes match the previous full qualification
  at `78ad660`: only the two coverage TOML files differ among its 285 inputs.
  That prior check/strict-Clippy/full-default evidence is reused, not rerun:
  1,604 unfiltered passes plus one nested positive control, zero failures and
  eleven existing opt-in ignores. Fresh RCH runs passed 72 focused and 132
  cache tests with three declared existing opt-ins; all five cache catalog
  cases passed. Matrix validation and its adversarial controls passed separately.
- Candidate and installed paths each passed 28 offline CLI/hook records, eight
  isolated installer controls and 26 delivery/persistence records using real
  SQLite. Both comparison paths already contain the delivery repair and were
  required to preserve it; old defects were not expected or fabricated. The
  actual credential wrapper passed 29 records. Held-open hook stdin under an
  explicit 300 ms invocation budget returned exit zero, empty stdout and a
  sanitized deadline diagnostic in approximately 114 ms on baseline, candidate
  and installed paths. These are synthetic input controls, not real Claude
  delivery or a population latency measurement.
- Wrapper, trusted config, repository/global settings and four-second outer
  hook timeout retain all four original identities. Live install/uninstall
  previews are byte-identical. Real budget and stats fields are unchanged after
  validating query-clock relations: 170 evaluations, zero emissions or independent
  judgments, 234 attempts, fourteen unknown-usage attempts and 2,394,033 known
  tokens. The shared allowance exists and remains disabled. No provider call,
  real ledger mutation or artificial cohort traffic was generated.
- Bounded metadata inspection found three Claude-named processes, none in this
  checkout, and six local native transcript files last modified September 29.
  This supports an absence-of-local-prompt-traffic explanation without establishing
  causality; process-name detection and file mtimes are incomplete evidence.
  The original `sr-1uf4` representative-cohort and independent outcome gates and
  `sr-9fzp` historical stalled-leaf/unfinished-row investigation remain open.
- Retained preparation failures: the standard shared DSR configuration had no
  SkillRanker entry, so the existing reviewed private configuration was reused;
  a read-only Beads formatter assumed a missing description field. Wrapped Git
  publication and a large documentation shell wrapper were refused by DCG;
  direct explicit commands and structured patches succeeded without override.
  Fresh author review also corrected the new corpus export's Unicode and
  destination-rename boundaries. No product assertion, timeout, gate, dependency
  or consent setting was weakened. Verification remains author-only. Raw
  artifacts and deployment receipt:
  `/scratch/tmp/skillranker-gap-execution-ghhj0f_f/native`.

## Redeployment — 2026-10-09 (BeigeCompass, first native prompts)

- Installed source `b5445ca896aa2f7f73c7a00357fd323a75a08674` at
  `2026-10-09 07:14:16 UTC`. Claude 2.1.295 submitted its first prompt before
  creating a fresh workspace's transcript directory. The prior executable
  rejected that prompt before ranking. The overlay now accepts descriptor-verified
  absence of the implicit parent and retains the submitted prompt and identity.
  Unavailable configured roots, broken links, traversal, non-directory parents
  and malformed existing files remain failures. The broader absence-check anchor
  cannot supply transcript content, and the overlay creates no native files.
  This release also activates the earlier `bfcfe4d` final-timeout ledger warning.
- Executable SHA256:
  `dcfe6a544cfa0336012e2022f039c445a9f242181e24d694934a3c3c7ef4a966`,
  25,174,576 bytes, mode `0755`. The preceding `a5a27c2` executable remains at
  `~/.local/state/sr/deployments/20261009T071416Z-a5a27c2/sr`, SHA256
  `9df692b0c5c024359532e66e71e9a845eadbce6f7c8d398af41ac0cf46be0a1c`.
  Cooperating lock, identity rechecks, backup, atomic replacement and file/directory
  fsync passed. These do not provide CAS against noncooperating editors.
- DSR run `ddc3652e-bdbc-474c-bb7b-9e78031af9f2` passed in 1,678.016 seconds
  through mandatory RCH on `vmi1264463`, locked native Linux x86-64 release,
  default features and one job. All 285 compiler/test inputs match the frozen
  local and worker manifest. Preserved untracked peer paths are outside that
  identity. ELF64 x86-64, artifact bytes and source-linked manifest checks passed.
  Archive SHA256
  `925dddb9b045bc681081de0c3d807b80f9444cc3dbb69e56d460f5f59d5c2313`
  contains exactly the identical executable and verbatim `LICENSE`. No tag,
  public release, other platform or provider qualification was created.
- Original-source missing-directory regression: two passes and one failure.
  Repaired focused context/authorized-read/hook controls: 40 passes. Locked
  all-target check and strict all-target Clippy passed, as did formatting,
  whitespace and 61 Python runner controls. Final default full suite on RCH
  `hz4` passed in 1,512.249 seconds: 1,610 distinct unfiltered passes plus one
  nested positive control, zero failures, eleven unchanged opt-in ignores and
  zero fixture tracebacks. The existing parser reconciled every result with
  149 unfiltered target summaries. Source hashes stayed unchanged; clean-overlay
  fingerprint `705e038cf1f9ed0a6ce8efa1c4309205036e95f2e6940ec697415fc97b396447`
  and tree `7cb289191cd15f35ef3c1fbc8bf15c1ebbbb382f` bind this qualification.
  Compilation and temporary test staging used private owner-only memory storage
  to avoid that worker's severe disk waits; this is not production-disk proof.
- Three earlier unchanged full runs on `vmi1264463` failed timing assertions:
  paid-rerank cleanup had no reserve left; a second run admitted only one
  attempt; a later transport refusal took 542 ms against its 500 ms assertion.
  Unchanged isolation and two mixed retry runs passed. CPU pressure was elevated
  near the later failure, but correlation does not establish its cause. All
  failures remain retained. No assertion, invocation deadline, reserve or default
  test-concurrency setting was changed. The final pass does not resolve the
  original `sr-9fzp` intermittent stalled-leaf/in-flight investigation.
- Actual Claude 2.1.295 three-turn probes used fresh private workspaces and
  ledgers, synthetic requests and offline `sr`. The old executable failed three
  of 49 predeclared checks. Candidate and installed paths each passed all 49,
  including an observed absent transcript parent, preserved native prompt IDs,
  silent shadow delivery, an accepted explicit envelope, and nonblocking invalid
  flags/unsupported events. Identical explicit text retained distinct prompt IDs.
  Tools were disabled: this establishes hook acceptance, not skill loading,
  visibility, nine-dimension native advisory qualification or relevance quality.
  Each probe recorded zero Jev attempts. Claude's own synthetic Haiku calls were
  separately bounded to three turns per executable and $0.10 per turn.
- Candidate and installed paths each passed 28 CLI/hook records, eight installer
  controls and 26 delivery/persistence records with real SQLite. The actual
  credential wrapper passed 29 controls. Both comparisons preserved the earlier
  delivery repairs; no old defect was expected or fabricated. Four configuration
  identities, live install/uninstall previews and operational metrics remained
  unchanged after activation: 170 evaluations, zero emitted suggestions or
  independent judgments, 234 attempts, fourteen unknown-usage attempts and
  2,394,033 known tokens. Query-clock and displayed budget-window relationships
  were validated before comparison. The allowance remains disabled. Credentials
  were not edited; owner-only permissions were checked.
- Retained preparation failures include an RCH source-drift refusal after my
  formatting, CASS's exhausted search fuel, and a private telemetry SSH timeout
  that interrupted the first alternate-worker build before tests. Official
  same-wrapper recovery finalized that job as exit 137; it received no test
  credit. Missing and then incorrect wrapper-test arguments also failed before
  the correct wrapper invocation passed; expectations were unchanged. UBS
  staged/diff findings remain 13 critical, 512 warnings and 133 informational;
  critical findings match unchanged test-panic/dummy-secret heuristics, with new
  setup warnings reviewed without suppression. Verification is author-only.
  The original `sr-1uf4` representative 500-turn cohort, independent corpus/paired
  outcome gates and native promotion gates remain open. Synthetic probes receive
  no organic cohort credit. Raw evidence, failing logs, audit and deployment
  receipt: `/scratch/tmp/skillranker-claude-native-3_xe6z4x`.

## Redeployment — 2026-10-10 UTC (BeigeCompass, evidence parsing and queued work)

- Activated source `e7e2acd3d35b9c0c98dec6cc2f97d11eafe7bf6f` at
  `2026-10-10 01:38:36 UTC`. This connects the published strict evidence-file
  parsers and queued blocking-work admission repair to the existing shadow
  executable. Running blocking closures remain nonpreemptible; this does not
  resolve the original historical `sr-9fzp` cause.
- Installed SHA256
  `61983b861e3bc72e190e9b79272f71f9a1d4e69407e0f7dfd860f9181a4065f9`,
  25,332,064 bytes, mode `0755`. The old `b5445ca` executable is preserved at
  `~/.local/state/sr/deployments/20261010T013836Z-b5445ca/sr`, SHA256
  `dcfe6a544cfa0336012e2022f039c445a9f242181e24d694934a3c3c7ef4a966`.
  Cooperating lock, inode/hash/mode rechecks, backup, atomic replacement and
  file/directory fsync passed; external noncooperating edits are not CAS-protected.
- DSR `a138165d-7009-47f0-804e-352a8fde5959` passed in 1,690.210 seconds
  through mandatory RCH `vmi1264463`, locked native Linux x86-64 release,
  default features and one job. All 285 committed/local/worker inputs match
  the earlier qualification manifest
  `1aeadfd290832df00fdb405ef33496f3d12a18e149ce862c6df67d3197ae189c`.
  That same source's all-target check, strict Clippy and full default proof
  are reused: 1,618 distinct passes, one nested positive control, zero failures
  and eleven existing opt-in ignores. Those tests were not rerun for this
  executable-only deployment. ELF64 x86-64, artifact manifest and archive checks
  passed; archive SHA256
  `19f7484e9c72ec74fc530cb295f8f9d7913d6d3c0d9e55be4bfe5cfd31772b9f`
  contains exactly the identical executable and verbatim `LICENSE`.
- The actual old binary failed 28 of 33 raw evidence-file controls by accepting
  invalid JSON whitespace or escaped duplicate keys. Candidate and installed
  paths each pass all 33, including three valid counterparts and thirty
  malformed-input refusals, zero HTTP accounting and non-actionable reports
  with quality `not-established`. Each comparison run also passed 28 CLI/hook
  and 26 real SQLite delivery records; each new binary passed eight isolated
  installer controls. The actual credential wrapper passed 29 records.
- Four configuration identities, live install/uninstall previews and every
  non-clock operational field remain unchanged. Query-clock relationships were
  checked before comparison: 170 evaluations (167 shadow), zero emissions or
  independent judgments, 234 attempts and fourteen unknown-usage attempts.
  Credentials, consent, allowance, hook timeout and capture policy were not
  changed. No provider or organic cohort traffic was generated.
- Preparation incident: failed Mail reservations did not stop the initial
  chained Beads claim. The author caught this sequencing error and stopped
  repository edits until coordination recovered. After graceful owner drain
  and raw DB/sidecar backup, the first supported repair failed its staged backup
  integrity check. Supported leaked-page vacuum then reclaimed two pages;
  canonical integrity/FKs passed and all 22 original table row hashes were
  unchanged before normal mailbox writes resumed. Exact reservations were then
  acquired. No archive reconstruction or row-loss acceptance was used.
  Both eight-second CASS searches timed out (124); setup path/flag errors and
  a DCG-refused large wrapper are retained without qualification credit.
  Staged UBS exited 3 because this documentation/tracker-only change contains
  no supported source files; nothing was scanned and no UBS pass is claimed.
  Documentation consistency and whitespace checks passed.
- Author-only native Linux verification; no new real-Claude delivery,
  public provider, other-platform, independent quality or phase promotion.
  `sr-1uf4` keeps its original representative 500-turn and metric requirements;
  corpus/paired-outcome gates and the historical runtime investigation stay
  open. Raw controls, failures, audit and rollback receipt:
  `/scratch/tmp/skillranker-qualified-deployment-tc2xged2`.

## Redeployment — 2026-10-10 UTC (BeigeCompass, installed latency reporting)

- Activated source `40d320ebc3d04ac04a01869873195f3cc9ff8eab` at
  `2026-10-10 05:00:49 UTC`. The installed JSON/table stats now report measured
  sample counts, p99 and per-channel latency, including failure durations and
  excluding unfinished rows. Cold-start/TLS strata and memory remain unrecorded.
- Installed SHA256
  `2e0a6a9b480cca829473bf6ca2c4ec70f17f9aff72029d304e0308fb4f93246e`,
  25,381,912 bytes, mode `0755`. Prior source `e7e2acd` is preserved at
  `~/.local/state/sr/deployments/20261010T050049Z-e7e2acd/sr`, SHA256
  `61983b861e3bc72e190e9b79272f71f9a1d4e69407e0f7dfd860f9181a4065f9`.
  Cooperating lock, binary/config identity rechecks, backup, atomic replacement
  and fsync passed; external noncooperating edits remain outside CAS protection.
- DSR `296d35b8-3122-48c4-bab4-80a464421e66` passed in 1,142.886 seconds
  through normal RCH admission on `hz2`, locked native Linux x86-64 release,
  default features and one job. All 285 local/committed/worker inputs match
  the qualified source. The existing check, strict Clippy and full default proof
  (1,620 distinct passes, zero failures, eleven unchanged opt-in ignores) are
  reused, not rerun. Build metadata includes tracker-only dirtiness while the
  original task was claimed. ELF, manifest, archive and verbatim `LICENSE`
  checks passed. No public release, tag or other-platform qualification.
- Candidate and installed paths each pass seven stats control groups on isolated
  SQLite fixtures, 33 strict-parser controls, 28 CLI/hook comparison records,
  eight installer controls and 26 SQLite delivery comparison records. The actual
  installed credential wrapper passes 29 records. These synthetic offline checks
  make no provider calls and earn no organic-traffic or independent-quality credit.
- Real stats now expose 162 measured durations: three CLI and 159 shadow, with
  eight unfinished shadow rows excluded; aggregate and shadow p99 are 2,569 ms.
  All existing non-clock metrics are unchanged: 170 evaluations, no emissions or
  independent judgments, 234 provider attempts and fourteen unknown-usage attempts.
  Four configuration identities, live install/uninstall previews and budget fields
  are unchanged. Shared guard remains disabled; consent, capture and scope unchanged.
- The first DSR attempt refused after 308.905 seconds when the selected `vmi1264463`
  worker's admission wait expired (RCH-I001; inner exit 103, DSR exit 6). No compile
  or local fallback ran. The retry used `hz2`'s available normal capacity without
  interrupting peers or changing scheduler policy. A dynamic inspection command
  was guard-refused; explicit read-only commands succeeded. All failures retained.
- `sr-1uf4` remains open under its original representative 500-turn requirements.
  Independent corpus/cohort gates and the historical timeout cause remain open.
  Author-only activation proof and rollback receipt:
  `/scratch/tmp/skillranker-stats-activation-rqps11yy/retry-hz2`.

## Redeployment — 2026-10-10 UTC (BeigeCompass, observation boundaries)

- Activated source `11ed1c5185d2d792067040e7b0711f1060c5c0db` at
  `2026-10-10 13:05:31 UTC`. Installed `observe` now refuses unresolved
  lineage, records only the selected branch, and distinguishes explicit
  producer/session/agent observation namespaces. Normalized imports retain
  their loads and cursors with unknown exposure attribution; producer-aware
  ranking/exposure provenance and historical attribution remain unqualified.
- Installed SHA256
  `6964ceb79ac8aeed3c04208b5dde64c38b6e2022661210436357a7ce0527fe05`,
  25,373,808 bytes, mode `0755`. Prior `40d320e` is preserved at
  `~/.local/state/sr/deployments/20261010T130531Z-40d320e/sr`, SHA256
  `2e0a6a9b480cca829473bf6ca2c4ec70f17f9aff72029d304e0308fb4f93246e`.
  Cooperating lock, identity rechecks, backup, atomic replacement and fsync
  passed. External noncooperating edits remain outside CAS protection.
- DSR `50b4b077-605f-4f33-80c3-4b1242cb794f` succeeded in 1,389 seconds
  through mandatory remote RCH on `vmi1264463`, locked native Linux x86-64
  release, default features, one Cargo job and normal admission. All 285
  frozen inputs matched the actual isolated worker snapshot twice. Existing
  source check, strict Clippy and full default proof are reused, not rerun:
  1,628 distinct passes, zero failures and eleven unchanged opt-in ignores
  at base `24961ea` plus overlay `104f64a4`, with committed input bytes
  matching `11ed1c5`. Build metadata includes tracker-only dirtiness;
  unrelated untracked paths were preserved. ELF, manifest, archive and verbatim
  `LICENSE` checks passed; no public release or other-platform qualification.
- Candidate and installed paths each pass 39 observation commands and 19
  private ledger initializations, 33 parser controls, seven stats groups,
  28 CLI/hook records, 26 delivery/persistence records and eight installer
  controls. Private candidate wrapper and actual installed credential wrapper
  each pass 29 records. Inputs and stores are synthetic and isolated; all
  calls are offline. The prior executable reproduced the sibling-load defect.
- Four configuration identities, live installation/removal previews, budget
  fields and all non-clock operational metrics remain unchanged: 170
  evaluations, zero emissions or independent judgments, 234 attempts and
  fourteen unknown-usage attempts. No operator observation reconciliation,
  public provider call or organic cohort traffic was generated. Consent,
  shadow mode, capture, coverage, timeout and disabled shared guard are unchanged.
- Retained failures: initial busy-worker admission refused after 308.288
  seconds without compilation or local fallback; a reused private checker
  subsequently inspected an obsolete persistent root and failed despite
  successful DSR compilation. Correct isolated-source proofs reconcile that
  error. A native fixture filename disagreed with its session ID; the fixture
  was corrected with every assertion retained, and the mismatch now has a
  separate refusal control. A duplicate exclusive-output postcheck failed;
  fresh output paths passed the final invariance check. Six bounded CASS
  searches timed out. Staged UBS exited 3 with no supported source files;
  nothing was scanned. Documentation consistency and whitespace passed.
  No failing case was given qualification credit.
- Author-only native activation. Original `sr-1uf4` representative 500-turn
  and metric requirements, independent corpus/cohort gates, and the historical
  timeout cause remain open. Evidence, failed logs, audit and rollback receipt:
  `/scratch/tmp/skillranker-observe-activation-yoyao7rl/retry-vmi1264463`.
