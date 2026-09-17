# Foundation audit after 26ad983

Audit bead: `sr-roadmap-l1i.1.13`. This review covered every Rust module and test,
the evaluation artifacts and Python checkers, the bootstrap CLI, dependency
boundaries, and the documentation changed in the first foundation batch.

## Defects corrected

- Expected-value checks accepted fractional harm counts, numeric strings and
  integers in boolean fields. They also accepted negative missing counts and
  timeout replacements that silently dropped the original attempted cohort.
  Counts, labels, losses and calculated values now retain their declared types
  and denominators.
- Coverage sets silently deduplicated IDs. Synthetic cases could name absent
  skills, contradict their oracles, remove oracle assertions or reverse safety
  flags. Validation now checks definitions, membership and the complete oracle
  contract for each case kind, including failure and success examples.
- Expected artifacts could claim measured-result status. The checker now binds
  them to the frozen policy and their explicit synthetic/non-evidence status.
- Python accepted escaped lone surrogates; JSONL preprocessing removed illegal
  vertical-tab/form-feed whitespace. Both now fail without printing input text.
- Case/family IDs appeared in validation errors, while option IDs and roster
  names appeared in Debug output. These accidental diagnostic paths now hide
  the values. Explicit serialization remains unchanged.
- Wall-clock expiry accepted times before creation. Remaining-time helpers
  could increase budgets for samples before invocation start. Both now fail
  closed. Timestamp addition checks the final signed result instead of wrongly
  rejecting every duration larger than `i64::MAX`.
- Dependency checks inspected only the host target and omitted the package name
  `ms`. They now inspect all target configurations and reject that name too.
- A synthetic prompt incorrectly suggested changing tests when implementation
  was wrong. It now calls for preserving valid assertions and fixing the code.

## Verification

The final staged build/test inputs matched all three accepted RCH source-content
receipts. Rust source was frozen throughout these accepted runs. The commands ran
with `RCH_REQUIRE_REMOTE=1` and `--source-content-receipt`:

| Command | Result | Worker / build ID |
| --- | --- | --- |
| `cargo test --locked -j 4` | 32 passed: 3 CLI, 15 identity, 14 limits | hz3 / 30024414133223608 |
| `cargo clippy --locked --all-targets --all-features -j 6 -- -D warnings` | Passed | ovh-b / 30024414133223610 |
| `cargo check --locked --all-targets -j 4` | Passed | vmi1152480 / 30024414133223609 |

Receipt roots, in the same order:

```text
130e94f0aa481751fb328e65a4b5d880cbf1c61b5f460cbcbdd8118b83f9c2a5
39d4042c0235336b509e6ba0cb7f140e7eb965920cb9eacd1ae2a3e8908c2a00
b02e3909304ef3f8803d50c48d9ee792d6051670d98bc91f4746f0b081563d1c
```

Local checks passed: `cargo fmt --check`, all 36 Python regression tests, validation
of 12 synthetic cases, the all-target dependency check, whitespace checks and the
Beads cycle check. Seven deliberately corrupted fixtures were accepted by the
committed baseline and rejected by the corrected checker. A real Cargo metadata
fixture showed a Windows-only prohibited dependency omitted by the host graph
and rejected by the corrected checker; no fixture crate was compiled.

`UBS_SKIP_RUST_BUILD=1 ubs --staged --ci --no-color --format=json` completed with
zero critical findings. Its 19 Python and 330 Rust warnings were reviewed: strict
type checks intentionally distinguish booleans from numbers, the raw file
descriptor is context-managed, and Rust warnings concern test assertions and
validated constructors/arithmetic. This static scan does not replace compilation.

An earlier remote run printed successful Rust tests but its source receipt was
refused because two Python files changed during review. Those runs are not
accepted verification. The final runs above used the frozen complete input set.
Logs, per-file hashes, receipt JSON, before/after reproductions and retained
synthetic artifacts are recorded under `/data/tmp/skillranker-audit-v_wmreld/`.

Concurrent work on decision/output schemas was excluded from this audit commit
and its verification. There were no live provider calls. Wall-clock checks alone
cannot detect rollback within a validity window; future cache code still needs
monotonic elapsed-time enforcement and rollback invalidation.
