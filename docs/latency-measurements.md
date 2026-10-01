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
discovery (sr-w4in) can reduce waiting, but scheduling overhead must be measured
against that benefit; concurrency does not reduce the parsing CPU work.

A 78.6 MiB real transcript does not slow the hook: its read is bounded to the
2 MiB tail.

## 2026-09-22 to 2026-09-27, real shadow hook traffic

Source: the maintainer's recorded shadow ledger
(`~/.local/share/sr/ledger.sqlite3`), read-only. The data covers every turn of
the self-hosted Claude Code hook in this repository (docs/self-host-shadow-deployment.md).
Model `jev-latest` against the production TypeSafe endpoint, from this host.

| Cohort | Turns | Network path p50 / p95 / p99 | Fallback (unavailable) |
| --- | ---: | ---: | ---: |
| All revisions, 2026-09-22 to 2026-09-27 | 148 | 1397 / 2279 / 2598 ms (n = 88) | 60 of 148 |
| `075f928` (2026-09-25 22:05Z to 2026-09-27 04:31Z) | 15 | 983 / 2533 / 2533 ms (n = 15) | 0 of 15 |

The percentiles cover only turns that reached a decision (ranked or abstain),
using nearest-rank. They leave out the 60 unavailable turns:
- Most of those ended before any network work, so including them would lower
  the percentiles and flatter the result.
- The 3 `in-flight` turns went the other way: their attempts were sent and
  never completed, so each took at least the 3,000 ms deadline. Counting them
  at 3,000 ms puts the all-revision p99 at 3,000 ms or more. Since d516c1a
  (sr-9fzp), such overruns record their failure rather than staying
  `in-flight`; that does not make them faster.
- The all-revision cohort also has 6 provider attempts with unknown usage, all
  on failed turns.

Every hook turn is a new process with a new TLS connection, so these are cold
network measurements; the "warm" target has no separate measurement here. The
exact-cache-hit rows above use `sr rank --context --offline` in a scratch
directory that is not a Git repository, so they leave out the project-signal
Git cost that the hook pays.

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

## 2026-10-01, bounded concurrent local paths

The first implementation scheduled one blocking job per skill file, in batches
of four. It preserved the observed outputs but regressed this benchmark. It is
not accepted as a performance improvement.

Both binaries were unoptimized development builds with debug symbols stripped,
not the release builds measured above. The baseline production source is
`fae2145` (binary SHA256 `aa2321d1d5b6c9cb2c6695ae3703eb838ca80da970cc3a36c652889a11f710ea`).
The first concurrent binary is the same base plus frozen seven-path overlay
`fd1292258ab3d65283aaf617375140eacd0feeab565a4b4f97a5828500d0b741`
(binary SHA256 `535932301f081873df8d3724ff31b616d8f1438086b4c7a2e0dbb174743a074d`).

The synthetic roster has 205 direct Claude skills totaling 4,812,565 bytes.
Each path has 30 alternating baseline/concurrent pairs on `threadripperje`;
all samples, exits and peak RSS are retained. Each binary has its own private
XDG state. Cache fills use only the checked-in loopback TLS peer, its local CA
and a synthetic key; measured cache hits are offline. No public provider or
live transcript was used. Host load changed from 328 to 84 on 128 CPUs, so
these are shared-host observations, not a quiet-host or release qualification.

| Path | Baseline p50 / p95 / p99 | Per-file jobs p50 / p95 / p99 | Peak RSS, baseline / per-file |
| --- | ---: | ---: | ---: |
| Offline miss, all 30 exit 11 | 696 / 1032 / 1125 ms | 750 / 1143 / 1510 ms | 30.14 / 31.43 MiB |
| Explicit request, all 30 exit 0 | 939 / 1224 / 1673 ms | 1038 / 1587 / 2631 ms | 32.54 / 33.79 MiB |
| Exact cache hit, all 30 exit 0 | 1210 / 1767 / 2670 ms | 1301 / 1850 / 2399 ms | 33.08 / 34.31 MiB |

Both roster JSON pages were byte-identical; paired decisions, errors and ranked
skills matched. Both binaries returned 30 confirmed cache hits, and traced
explicit resolution started zero Git processes. These checks establish output
equivalence for the synthetic cases, not usefulness or live cohort acceptance.

The first benchmark attempt produced no valid report because the loopback peer
expired during offline measurements. It contributes no performance result.
The valid report is retained at
`/scratch/tmp/sr-w4in-measure-sfvhl8ja/report.json`; the one-off harness is
`/scratch/tmp/skillranker-w4in-measure.py`.

A second design scheduled at most four jobs per bounded group of files. The
same 30-pair comparison at host load 11.6 to 8 still showed a small regression:

| Path | Baseline p50 / p95 / p99 | Grouped jobs p50 / p95 / p99 | Peak RSS, baseline / grouped |
| --- | ---: | ---: | ---: |
| Offline miss, all 30 exit 11 | 291 / 311 / 314 ms | 298 / 312 / 322 ms | 29.83 / 34.19 MiB |
| Explicit request, all 30 exit 0 | 404 / 416 / 418 ms | 411 / 431 / 432 ms | 32.50 / 36.71 MiB |
| Exact cache hit, all 30 exit 0 | 457 / 474 / 480 ms | 461 / 480 / 482 ms | 32.73 / 37.48 MiB |

Outputs and cache-hit counts again matched. Report:
`/scratch/tmp/sr-w4in-measure-495jyfx4/report.json`; binary SHA256
`853f45897efddebfe21703554a28a19d830d505de99dfb7f4b7d2c2212f7dedb`;
source `cdebf58` plus overlay
`136b814380fb0102525646999092a88cf70459251f588ac93cf84a4ffc02edfb`.
Both comparisons used warm synthetic files on `/scratch`'s XFS filesystem,
not the maintainer roster on the project's Btrfs filesystem. Neither result
establishes a general benefit from concurrent reads.

Five further synthetic invocations under GDB produced 413 timed stack samples:
145 included dynamic-content detection, 168 included metadata parsing and
three included authorized reads. These are inclusive, overlapping stack counts,
not additive CPU percentages or benchmark timings. The profile identifies the
adjacent-byte `$N` scan as a smaller optimization to compare separately.
Samply and perf were refused by kernel policy; no kernel settings changed.
The valid GDB samples are retained in
`/scratch/tmp/skillranker-w4in-gdb-stacks.json`.

The selected change replaces only the `$N` adjacent-byte scan with a scan of
dollar-sign positions and the same immediate ASCII-digit check. The larger
concurrency prototype was removed from the working tree after both comparisons;
its patch is retained at `/scratch/tmp/skillranker-w4in-rejected-concurrency.patch`.
The original overlap/concurrent-read task remains open.

The isolated change passed the same 30-pair comparison at host load 6.6 to 26.3:

| Path | Baseline p50 / p95 / p99 | Dollar-position scan p50 / p95 / p99 | Peak RSS, baseline / selected |
| --- | ---: | ---: | ---: |
| Offline miss, all 30 exit 11 | 453 / 579 / 615 ms | 366 / 557 / 578 ms | 30.56 / 32.34 MiB |
| Explicit request, all 30 exit 0 | 595 / 783 / 809 ms | 398 / 493 / 546 ms | 33.11 / 34.58 MiB |
| Exact cache hit, all 30 exit 0 | 746 / 999 / 1120 ms | 553 / 817 / 833 ms | 32.71 / 34.54 MiB |

The selected binary was faster in 27/30 miss pairs, 30/30 explicit pairs and
26/30 cache-hit pairs. Both roster JSON pages remained byte-identical, paired
outcomes matched, each binary returned 30 confirmed cache hits and explicit
resolution started zero Git processes. The observed peak-RSS increase is
retained as a countermetric. This establishes a gain for this synthetic
development-build scenario, not a release latency or live quality gate.

Source: `cdebf58` plus two-path overlay
`8116736fccb7297d3f096e6b504d4596b6c262e1231bb5432ff89e7c7652020c`;
binary SHA256
`403466dfd99e80e673f24629309499afdac9f184a9463ff626738aeb9b557d05`.
Report: `/scratch/tmp/sr-w4in-measure-zmrxa8bw/report.json`; harness:
`/scratch/tmp/skillranker-w4in-minimal-measure.py`.
