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

## 2026-10-05, two-worker read/hash/parse experiment rejected

This experiment moved bounded file reads, hashing and metadata parsing to two
owned Asupersync workers, retained sequential quota/authority merging, and
overlapped Git signals with discovery. It is not shipped. Its focused behavior
checks passed, but the complete paired comparison did not meet the original
`sr-w4in` acceptance criteria.

The clean baseline is `f951fb42038b888c4b312a17cd6a76e8dc6d27e8`; the experiment
is that revision plus the retained seven-path patch with SHA256
`2f693669cf92a103bacc901a608e74f5b7ab2619e8c6afb32e61a4b1ef0d9a2b`.
Both executables used the pinned compiler, default features, explicit Linux
x86_64 target and unoptimized development profile, with only debug symbols
stripped. Their SHA256 values are:

- Baseline: `2b4ce0c28c36ec166374783f17d7559fa363eac5ec3f50d9a3d5014ff626f660`.
- Experiment: `2ef3233f6b59dbc78a611edbf639467566d6211cc4cfd8a38d40ea7439e9b508`.

Normal RCH source-content receipts verified the frozen source and lockfile,
including all 407 selected tracked non-Beads files. The existing credential
filter excludes `.env.example`; it was not weakened. Baseline focused checks
had 104 passes and one existing ignore; the experiment had 112 passes and the
same ignore, with eight additional focused tests. These results establish
those executed behaviors, not performance or full-suite qualification.

The native comparison used 30 alternating pairs for each path (180 samples)
on `threadripperje`, with the same synthetic 205-file/4,812,565-byte roster,
an actual private Git index with dirty tracked `Cargo.toml`, and separate
private XDG state. Both loopback TLS fills returned ranked decisions; the
four HTTP attempts used only a synthetic key and the checked-in local CA.
There were no public-provider calls or organic hook observations. Host load
was 121.65 before and 115.17 after on 128 CPUs; it varied during the run.

All process exits are included in the percentiles:

| Path | Baseline p50 / p95 / p99 | Experiment p50 / p95 / p99 | Peak RSS, baseline / experiment |
| --- | ---: | ---: | ---: |
| Offline miss | 2118 / 3463 / 3655 ms | 2168 / 4112 / 4442 ms | 31.22 / 30.95 MiB |
| Explicit request | 2247 / 3369 / 4356 ms | 2174 / 3401 / 3464 ms | 33.50 / 32.20 MiB |
| Offline cache replay attempts | 2382 / 3856 / 4802 ms | 2011 / 3413 / 3455 ms | 33.12 / 34.41 MiB |

The baseline miss path returned 30 exit-11 misses; the experiment returned
27 misses and three exit-6 timeouts. Explicit resolution had 30 successes
before and 28 successes plus two timeouts after. Cache replay had only 22/30
confirmed hits before (eight misses) and 17/30 after (eleven misses and two
timeouts). The cache row therefore is not an exact-cache-hit latency claim.
User/system CPU totals across the 30 runs were 34.67/24.48 versus 38.18/23.55
seconds for misses, 35.54/23.72 versus 41.73/22.72 for explicit resolution,
and 41.42/24.98 versus 38.80/21.69 for cache replay.

Both roster pages were byte-identical, but full paired outcomes differed.
The traced explicit baseline succeeded and the experiment timed out; neither
started Git. The complete report has `valid=false`. Faster medians in some
rows do not compensate for added timeouts, fewer confirmed hits, or differing
outcomes. The seven experimental paths were restored after exact ownership
and hash checks. No deadline, quota, assertion or publication reread was
relaxed, and no release, usefulness or phase gate is claimed.

Retained report, source hashes, patch, checksums and focused receipts:
`/scratch/tmp/skillranker-w4in-read-parse-s7vmxexo/paired-debug/report.json`
and its parent directory. The original `sr-w4in` remains unfinished.

## Git/discovery overlap: qualified development comparison, 2026-10-05

A smaller variant keeps the original sequential roster resolver and all its
resolution tests, while running it in one invocation-owned blocking leaf and
polling bounded Git collection concurrently. Explicit requests perform no Git
probe. The whole roster is still reopened before advisory publication. It
delivers no concurrent skill-file reads and cannot close the original `sr-w4in`
scope by itself.

Both binaries use pinned nightly-2026-08-31, the native
`x86_64-unknown-linux-gnu` target, default features and the unoptimized development
profile. Debug symbols were stripped into separate retained executables. The
baseline is `f951fb42038b888c4b312a17cd6a76e8dc6d27e8`; the five-path candidate is
frozen by `overlap-v3-source.json`. This is a development comparison, not
optimized-release, organic-hook, live-provider or usefulness qualification.

Thirty alternating pairs per path retain all 180 outcomes, child CPU and peak
RSS. The synthetic 205-file roster contains 4,812,565 bytes; the private real Git
index contains a tracked dirty Cargo.toml. Shared host load was
22.59/26.30/58.09 before and 24.76/26.01/55.20 after, on 128 CPUs.

| Path | p50 wall ms, before → after | p95 wall ms | p99 wall ms | Peak RSS MiB |
|---|---:|---:|---:|---:|
| Stateless offline miss | 348.663 → 320.833 | 430.254 → 450.377 | 549.12 → 485.982 | 30.684 → 32.09 |
| Explicit local request | 432.082 → 395.204 | 582.213 → 546.666 | 637.903 → 547.275 | 32.742 → 33.82 |
| Exact cached rank | 570.724 → 505.666 | 696.612 → 699.897 | 818.111 → 752.747 | 32.723 → 34.527 |

Both roster pages are byte-identical and all paired JSON outcomes agree,
excluding only measured elapsed time and cache age. Each miss cohort contains
30 exit-11 outcomes; each explicit cohort contains 30 successes. Each cache
cohort contains 30 successes and 30 confirmed exact hits. Both traced explicit
requests succeed with zero Git subprocesses. The loopback TLS peer is synthetic:
four HTTP attempts fill two private caches; no public inference or production
ledger rows are used.

The median improvement is about 8–11%, but miss and cache p95 worsen, and peak
RSS rises about 1–2 MiB. Total child user/system CPU seconds for the thirty runs
are miss 7.41/2.56 → 7.60/2.60, explicit 8.76/3.68 → 8.85/2.93, and cache
12.07/4.76 → 11.84/4.38. Do not infer a uniform tail-latency improvement.

Focused source-content RCH qualification executed 242 passes, zero failures and
one existing ignore across seven suites; receipt
`a81fb635e5d56b02538067871e5dbddf5888e00b28461fca9f735484b073836b`
binds all five Rust paths, Cargo.lock and 407 tracked non-Beads inputs. Ordinary
guarded canonical-source RCH then executed the full default suite: 1,530 passes,
zero failures and 11 existing ignores across 148 suites. All-target check and
strict Clippy also passed, with all 407 inputs verified before and after.

A temporary production mutation running the filesystem callback on the executor
compiled and caused the unchanged real-Git overlap test to fail at its two-second
wait; restoring the exact candidate bytes made the same test pass. The earlier
offline dependency-fetch failure and critical-memory admission refusal executed
no tests and remain separate failed attempts. Differential UBS returned exit 1
with two critical unchanged test-fixture panics; its findings were reviewed, not
claimed as a clean scan.

Private source manifests, paired samples, failed attempts and terminal receipts:
`/scratch/tmp/skillranker-w4in-read-parse-s7vmxexo`. The retained candidate patch
SHA-256 is `d49ec6808782a1b391560f8250566e2e645896dc676551916fb8f78c8ad2cf70`.

### Optimized counterpart and disposition

The same procedure then executed 180 native optimized-release samples: thirty
alternating pairs per path, the same compiler/target/default features, the same
205-file fixture and actual private Git index. Shared host load was
30.91/33.57/56.08 before and 28.27/32.74/55.21 after, on 128 CPUs. These samples
are separate from the development cohort and from the rejected larger prototype.

| Path | p50 wall ms, before → after | p95 wall ms | p99 wall ms | Peak RSS MiB |
|---|---:|---:|---:|---:|
| Stateless offline miss | 85.791 → 76.347 | 164.787 → 115.209 | 166.204 → 127.909 | 14.215 → 14.152 |
| Explicit local request | 87.664 → 91.259 | 210.472 → 158.059 | 213.749 → 162.026 | 15.168 → 17.148 |
| Exact cached rank | 127.713 → 122.078 | 265.78 → 277.341 | 282.287 → 284.587 | 16.117 → 17.133 |

Both roster pages and every paired full outcome agree; only elapsed time and
cache age are excluded from JSON equality. Both miss cohorts contain 30 exit-11
outcomes. Both explicit cohorts and both cache cohorts contain 30 successes,
and each cache cohort has 30 confirmed hits. Explicit process traces succeed
with zero Git invocations. The bounded loopback TLS peer again makes four
synthetic HTTP attempts; its five diagnostic records drain without errors.
There are no new public-provider or organic-hook observations.

Total child user/system CPU seconds across thirty runs are miss
0.99/1.41 → 1.00/1.33, explicit 1.05/1.56 → 1.09/1.38, and cache
1.67/2.32 → 1.68/2.35. Miss median improves about 11% and cache median about 4%.
Explicit median worsens by 3.595 ms; cache p95/p99 worsen by 11.561/2.300 ms.
Explicit/cache peak RSS rises by about 1–2 MiB. This supports keeping the
smaller overlap change with these tradeoffs, not a uniform latency or memory
improvement. The 100 ms cache p95 target is not established by this cohort.

DSR orchestrated both ordinary private builds through guarded RCH on native
Linux worker vmi1264463. Compiled input proof uses separately verified canonical
source hashes rather than DSR's staged-source metadata: baseline inputs match
before/after, and candidate inputs match during compilation and after. The
baseline build's first attempt fails with OS ENOENT before compact_str's compiler
process starts; its cached source directory is missing while compiler and project
inputs remain intact. Ordinary DSR resume succeeds without a pressure override,
global cache edit or peer cleanup. No failed attempt is a passing build cell.

The retained baseline executable is 23,458,048 bytes, SHA-256
`e72e7d96f9dfd866c0bb505cf0cb9143b5687d62cc314293ea9fe4fb3d3146c0`;
the candidate is 23,645,720 bytes, SHA-256
`0d1427ff5cf240a509b31620f0609c79e26f1987aa84a46937b8ea9f4fe438d3`.
Both match their DSR manifest checksums and native ELF machine 62. Baseline DSR
run `18d2ca5d-b185-4224-a532-de5a8d9b22f6` succeeds on attempt two; candidate run
`0f62dedf-d77e-40fc-9e0a-b710c12afc12` succeeds on attempt one. These are local
measurement artifacts, not public release or cross-platform qualification.

The original `sr-w4in` remains incomplete: concurrent skill-file reads and the
original task's full acceptance still need their own implementation and evidence.
No relevance, controlled-harm, live-provider, actual-hook, or broader performance
gate is closed by these synthetic comparisons.
