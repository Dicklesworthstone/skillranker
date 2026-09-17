# First P0 foundation batch

Implemented beads: `sr-roadmap-l1i.1.1`, `.1.2`, `.1.4`, `.1.7`, and `.1.8`.
The remaining P0 contracts and its phase acceptance gate are still open.

The single Rust 2024 package now has a pinned toolchain and lockfile, a pure
library, and a bootstrap `sr` supporting help/version. It includes local identity,
provenance, normalized records, roster records, checked resource budgets, separate
clock types, and request counters that preserve spent usage after refusals.
The evaluation artifacts freeze representative cases and policy semantics before
measured tuning; they are not a benchmark dataset or evaluator implementation.

## Executed checks

| Check | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| RCH `cargo check --locked --all-targets` | Passed, default features |
| RCH `cargo check --locked --all-targets --all-features -j 6` | Passed, including the reserved empty `tui` boundary |
| RCH `cargo clippy --locked --all-targets -j 6 -- -D warnings` | Passed |
| RCH `cargo clippy --locked --all-targets --all-features -j 6 -- -D warnings` | Passed after the test correction below |
| RCH `cargo test --locked -j 5` | 29 passed: 3 real CLI-process tests, 14 identity tests, 12 limits tests |
| `python3 scripts/validate_eval_policy.py` | 12 representative synthetic cases validated |
| `python3 scripts/test_eval_policy.py` | 19 passed, including a real validator CLI refusal |
| `python3 scripts/check_dependency_graph.py` | One Asupersync source, one Quill source; no prohibited packages across normal/build/dev edges with all features |
| `UBS_SKIP_RUST_BUILD=1 ubs --staged --ci --no-color --format=json` | Completed; zero critical findings; warnings reviewed |
| `git diff --check`, JSON parsing, Beads cycle check | Passed |

RCH ran in required-remote mode with source-content receipts. Python/format/static
checks ran locally. The local pinned compiler reports Rust 1.100.0-nightly,
commit `90850177249efe0321573c569aec5d12b257f8d6`, dated 2026-08-30, for channel
`nightly-2026-08-31`. Checks ran on Linux x86-64; no cross-platform release claim
is made. The [machine-readable record](verification-p0.json) binds command results
to source hashes and RCH receipts. Full logs remain at
`/data/tmp/skillranker-execution-m0ip1wu3/`.

The initial test run exposed a faulty assertion: it forbade a diagnostic from
containing a rejected input consisting of one space. The corrected test requires
the exact fixed diagnostic for every invalid input, retaining the separate
private-text checks. Production Rust and Cargo inputs did not change. Final tests
and all-feature Clippy cover the corrected test source; the earlier check receipts
retain its earlier hash. Failed and refused runs remain in the local evidence.

UBS warnings concern test assertions/unwraps, constant-validated arithmetic and
constructors, and deliberate exact Python JSON type checks that reject booleans
as numbers. Its file-open heuristic was reviewed: the descriptor is immediately
owned by a context-managed binary file. UBS compilation was disabled because the
named RCH jobs supply that evidence separately; scanner success is not build proof.

There are no live Jev calls, native-harness smoke results, ranking-quality results,
latency measurements, or release artifacts in this batch. Adapters, configuration,
decision/output schemas, ranking and the later phase gates remain tracked work.
