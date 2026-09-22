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
