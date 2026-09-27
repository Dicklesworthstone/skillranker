# Latency, Memory and Fallback Measurements (P6)

Revision-bound measurements for sr-roadmap-l1i.7.12. The README's performance
figures are targets. This file records what was measured, where, and how.
Numbers from a loaded shared host are evidence about that host, never
provider latency or a general-user claim.

## 2026-09-27, local paths, binary `0c7bd54`

- Binary: `--release --locked` built through RCH from `0c7bd54` (SHA256
  `7e392b88…`). The deployed `9ac7caf` binary (`143c2394…`) was measured the
  same way, for comparison.
- Host: `threadripperje`, AMD Ryzen Threadripper PRO 5995WX, 128 CPUs,
  Linux 7.0.0-30, shared with other agents' builds and tests.
- Roster: the maintainer's `~/.claude/skills`, 206 `SKILL.md` files, below the
  254-skill overflow threshold. Private throwaway XDG state; no Jev request.
- Command: `scripts/measure_local_latency.py BINARY --runs 30 --transcripts
  ~/.claude/projects/-data-projects-skillranker`

Quiet host, load average 9-10 (16:24Z), n = 30 each:

| Path | `0c7bd54` p50 / p95 / p99 | `9ac7caf` p50 / p95 / p99 | Peak RSS |
| --- | ---: | ---: | ---: |
| Offline cache miss (whole local phase, exit 11) | 54 / 63 / 85 ms | 53 / 67 / 81 ms | 16 MiB |
| Exact cache hit (fixture-filled, served offline) | 73 / 83 / 84 ms | 71 / 73 / 78 ms | 17 MiB |
| Offline hook over 6 real transcripts (0-78.6 MiB) | 76 / 80 / 81 ms | 74 / 79 / 79 ms | 17 MiB |

A cache hit used 30-40 ms of user CPU and 20-30 ms of system CPU over 60-70 ms
of wall time.

Busy host, load average 30-52 (05:44Z), `0c7bd54`: exact cache hit
313 / 487 / 537 ms; offline cache miss 155 / 323 / 633 ms; offline hook
301 / 416 / 419 ms. Other runs of both binaries that hour varied from p50
223 ms to p95 655 ms.

**The exact-cache-hit target (p95 ≤ 100 ms) is met on a quiet host (83 ms)
and missed under heavy shared load (487 ms).** The process does about 60 ms
of CPU work; the rest of the loaded-host time is waiting for CPU and I/O. A
traced hit (strace) shows the order of the work:
- startup and the discovery walk;
- one read of each skill file;
- the two git commands behind project signals, which the request fingerprint
  includes;
- the cache lookup;
- a second full discovery and read of every skill file, to revalidate content
  before publication.

The second pass is required. Concurrent reads and overlapping signals with
discovery (sr-w4in) would shorten the waiting under load, not the CPU work.

A 78.6 MiB real transcript does not slow the hook: its read is bounded to the
2 MiB tail.

## 2026-09-22 to 2026-09-27, real shadow hook traffic

Source: the maintainer's recorded shadow ledger
(`~/.local/share/sr/ledger.sqlite3`), read-only. The data covers every turn of
the self-hosted Claude Code hook in this repository (docs/self-host-shadow-deployment.md).
Model `jev-latest` against the production TypeSafe endpoint, from this host.

| Cohort | Turns | Network path p50 / p95 / p99 | Fallback (unavailable) |
| --- | ---: | ---: | ---: |
| All revisions, 2026-09-22 to 2026-09-27 | 148 | 1397 / 2279 / 2533 ms (n = 88) | 60 of 148 |
| `075f928` (2026-09-25 22:05Z to 2026-09-27 04:31Z) | 15 | 983 / 2533 / 2533 ms (n = 15) | 0 of 15 |

The all-revision fallback count is mostly setup, not provider failure:
- 31 `credential-absent`, before the key reached hook processes;
- 14 `authentication`;
- 7 `ambiguous-branch` and 4 `unsupported-input`, none recorded since the
  `9c1c026a` install;
- 3 `in-flight`;
- 1 `invalid-provider-response`.

`075f928` spent 30 attempts, with 0 unknown-usage attempts, and about
21,800 known tokens per ranked or abstaining turn. Its sample of 15 turns is
far below the 500 representative invocations the P7 availability gate (8.4)
needs.

**The warm network target (p50 ≤ 600 ms, p95 ≤ 1,500 ms) is not met** on
this host and roster. The provider pair alone took 0.9-1.2 s before the
connection-reuse change in `075f928`. The local phase adds about 60-80 ms on a
quiet host, and 0.15-0.65 s under heavy load.

## Bounded failure behavior

Stalled leaves are covered by tests rather than by these measurements:
- `rank_pipeline::a_run_that_overruns_its_deadline_records_its_failure_not_in_flight`
  is an uncooperative stall past the deadline;
- the hook deadline and quiet-failure cases are in the `shadow-hook` product
  suite;
- `breaker_contract` bounds a failing endpoint to three attempts.
