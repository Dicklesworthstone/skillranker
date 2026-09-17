# Process and evidence engine

This is P0 **runner mechanics** evidence. It does not certify ranking quality,
Jev, TLS, native harnesses, SQLite, or any unimplemented product command.
`product_gate` is always `not-applicable`, including a successful synthetic run.
Product schema integration and the boundary coverage matrix belong to
`sr-roadmap-l1i.1.9`; the independent engine certification is `.1.15`.
The full matrix authority and receipt certification gate remains `.1.12`.

Run from the repository, with an existing artifact parent directory:

```sh
scripts/e2e/run.sh --suite runner-smoke --artifacts /tmp
scripts/e2e/run.sh --suite runner-contract --artifacts /tmp
python3 -I -B scripts/e2e/test_runner.py
python3 -I -B scripts/e2e/test_runner_cleanup.py
python3 -I -B scripts/e2e/test_runner_contract.py
```

The mechanics suite's optional `--binary` is a Python interpreter, not `sr`.
The exact interpreter bytes are hashed, sealed in memory against concurrent
changes, and copied into a read-only sandbox mount.
The smoke suite runs success, expected refusal, and actual namespace isolation
cases. The unit driver additionally executes real failures, signals, descendants,
hangs, excess output, malformed/duplicate/missing records and planted secrets.
All test directories are retained for inspection; there is no implicit cleanup.

For `runner-smoke`, `--case ID` may be repeated for a development subset. It cannot produce a complete
pass: the result is `partial`, skipped cases are counted, and exit status is 3.
Other exits: 0 means complete mechanics success, 1 failed/incomplete evidence,
2 invalid input or runner I/O failure, and 4 unavailable sandbox enforcement.
An existing artifact parent receives a fresh unpredictable `sr-e2e-*` directory
(0700), with `events.jsonl` and `summary.json` (0600). No run overwrites another.
Console output names only the generated directory basename, never supplied paths.

## Isolation and bounds

Linux with `/usr/bin/bwrap`, `/usr/bin/prlimit` and usable network/PID namespaces is required.
Failure to establish the exact execution sandbox yields blocked evidence; the
engine has no unsandboxed fallback. A separate network namespace has no external
routes. Bubblewrap's PID namespace supervisor stays in the process group created
by the runner. Group termination kills that supervisor and tears down its entire
namespace, including descendants that create a new session. A second Bubblewrap
session would escape group cleanup. The fixture keeps ordinary process signal
semantics instead of running as namespace PID 1. The runner observes exit with
`waitid(WNOWAIT)` and retains the leader PID until it has signaled the group, so
cleanup cannot target an unrelated recycled process group. It then reaps the leader,
including deadline, signal and pipe-overflow paths. SIGKILL/crashes may leave an
unfinished event stream; absence of a reconciled summary is incomplete evidence.

The child receives a fixed environment with private HOME, work, tmp and XDG state.
The host checkout, operator home, credentials, proxies and artifact directory are
not mounted. Only read-only system runtime directories, the selected interpreter,
the fixed fixture, private proc/dev mounts and four 1 MiB writable tmpfs mounts
are visible. No raw child stdout or stderr is saved. Only validated assertion IDs,
booleans, bounded outcomes/counts, elapsed milliseconds and exit status survive.
Fixed error codes replace parser, spawn, path and provider exception text.

Each case declares its total stdout-plus-stderr byte cap and deadline. The suite
also has an aggregate case deadline; sandbox probing has its own two-second bound.
Mechanics fixtures allow five seconds per child so namespace/interpreter startup
on a busy fleet can reach the fault being tested. A missing startup witness still
fails certification. These fixture budgets establish no product latency claim;
the CLI's product deadline remains a separate contract.
Address space, CPU, core dumps and individual file writes have OS resource limits.
The interpreter is capped at 128 MiB, source files at 8 MiB, and source inventory at
10,000 entries, depth 32 and 64 MiB total. Events are capped at 1 MiB and expected assertion counts reserve
bounded space before execution. The trusted checked-in fixtures are not a hostile
arbitrary-code execution service or a total-machine resource isolation boundary.

## Evidence contract v2

`evidence.py` is the strict independent parser and reconciler. JSON rejects
duplicate keys, non-finite numbers, oversized records, unexpected fields and
invalid scalar types. Events have a contiguous `seq`, one header, and uniquely
identified case start/result pairs. A blocked case has no start. Each executed
case has exactly one terminal record in a finalized complete report. Missing
records remain incomplete. Retries cannot replace failures; this engine runs
exactly one attempt per case and rejects duplicate child assertions/results.
Every child record, event and summary is bound to an unpredictable invocation ID.
Timeout and descendant-start witnesses additionally bind the expected case and
fault ID. A receipt from another run or case cannot establish that a fault was
reached in this invocation. Version 1 engine reports are rejected by this parser;
their original version 1 validator remains available in Git history. This version
change is independent of the Rust product output schema.

The checked-in suite supplies expected exits, outcomes, assertion IDs and required
faults. Child results cannot choose expectations. Expected refusal requires the
declared exit, refusal outcome, all expected assertions, reached fault and zero
effects. These synthetic effect assertions do not establish product filesystem
or network behavior. The isolation case separately exercises real filesystem and
network boundaries. The validator rechecks observed outcomes against the manifest
and reconciles all summary counts with the event stream, even when a corrupted
summary has been recomputed to match corrupt events.

The header binds the canonical manifest, exact binary, fixture, runner, lockfile,
pinned toolchain file, platform, architecture and host Python version. It also
records Git HEAD and a content digest over Cargo manifests/toolchain plus every
regular file under `src`, `tests`, and `scripts` (including untracked source;
excluding Python bytecode), and `build.rs` when present. Operational Beads state,
documentation, ignored credentials and build outputs are outside this source
scope. A post-run identity mismatch prevents success. The digest is not a claim
that the checkout is clean, nor a Rust build receipt. No Rust features are exercised
by the fixture interpreter; `features` is therefore empty.

Consumers call `evidence.validate(run_directory, checked_in_manifest,
expected_identity, expected_run_id=invocation_id)` and separately require
`runner_status == "passed"`.
Passing the independently expected identity is required when combining evidence
from different revisions; merely accepting a file's own hash proves no provenance.
Artifact integrity is content-bound, not cryptographically authenticated.
There is no provider or actual-harness suite registration yet: such execution
requires separate trusted consent, frozen request/runtime budgets, and its own
accepted implementation/evidence tier.

## Independent engine certification

`runner-contract` executes the fixed oracle in `runner_contract.py`: 19 real
child scenarios and 54 total checks. It exercises nonzero exits, signals,
deadline/output exhaustion, descendants that create a new session, a failing
pipeline producer followed by a successful consumer, invalid records, false
assertions, effects, wrong outcomes, foreign invocation receipts, and planted
private diagnostics. The expected classifications are declared independently of
the engine's classifier. A failed assertion followed by a passing assertion
cannot erase the failed attempt. A forced SIGKILL verifies that an unfinished
run with no summary cannot be accepted.

The adversarial inner report deliberately retains 16 failed and three passed
cases. Separate complete-success and partial runs provide positive twins. The
outer certificate passes only when every expected behavior is observed. Seventeen
mutated reports exercise schema, count, event, identity, assertion, fault-witness,
and failure-history checks; the original reports are retained. No case filtering
is accepted for certification. Platform isolation unavailable means blocked,
not passed. Run acceptance in a frozen checkout: concurrent source edits correctly
invalidate an otherwise successful run.

A private `sr-contract-*` directory holds a version 1 outer certificate declaring
engine schema version 2, ordered assertion records, counts, timings, source and
driver identities, and hashes of retained inner reports and mutated candidates.
`runner_contract.validate_certificate(directory, expected_identity)` checks the
fixed catalog, canonical complete NDJSON, linked report content and classifications,
candidate rejection, and the summary. Consumers must also require its returned
`runner_status == "passed"`. The certificate is integrity evidence from a trusted
runner, not a signature or proof against a malicious artifact author. Runtime
observations such as process absence cannot be reconstructed from hashes alone.
Each engine run, child, diagnostic subprocess and interruption probe is bounded;
the certificate also refuses finalization after 120 seconds. The outer certificate
remains `product_gate: not-applicable`; it does not satisfy Rust, provider, native
harness, ranking-quality, or whole-matrix acceptance gates.
