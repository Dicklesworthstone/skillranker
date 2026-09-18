# Product e2e case evidence

A product e2e suite runs whole Rust integration targets against real files,
processes and the real `sr` binary. The runner-mechanics tier
(`scripts/e2e/runner.py` with `runner_child.py`) is different. It proves only
the runner itself, reports `product_gate: not-applicable`, and never counts as
product evidence.

Each contract-matrix e2e case of a product suite maps to the real tests that
establish it in a catalog, `scripts/e2e/product/<suite>.json`:

```json
{"schema_version": 1, "suite": "roster", "tier": "rust-product-integration",
 "targets": ["roster_cli", "..."],
 "cases": [{"id": "roster-json-pagination", "assertions": ["pagination_stable"],
            "tests": ["roster_cli::concatenated_pages_equal_the_whole_snapshot", "..."]},
           {"id": "full-roster-suite", "assertions": ["p2_gate_passed"], "tests": "all"}]}
```

`scripts/e2e/product_cases.py` has two modes:
- `check CATALOG`, a static check. It fails unless:
  - every mapped test is a `#[test]` function in exactly one of the suite's
    targets;
  - every matrix case of the suite has a catalog entry carrying the row's
    assertion IDs;
  - no catalog case is missing from the matrix.
- `evaluate CATALOG LOG`, which reads the actual cargo test log and reports
  each case as `passed`, `failed` or `missing`. A case passes only if every
  mapped test appears exactly once as `... ok`, and the run is complete: one
  `test result: ok` per target, with none failed, ignored or filtered, and at
  least one passed. An `all` case needs only the complete run.

Cargo's stderr and the test binaries' stdout interleave unpredictably, so
results are attributed by test name. The static check is what makes names
unambiguous.

`scripts/e2e/run.sh --suite roster --artifacts DIR` runs the roster suite
(`scripts/e2e/roster.sh`). It runs every roster, Quill, authorized-read,
redaction, local-inspection and P2 gate target remotely through RCH, then
writes per-case records to `cases.jsonl` in its artifact directory. The suite
passes only if every case passes.
`scripts/e2e/test_product_cases.py` shows that missing, failed, ignored,
duplicated, incomplete, filtered and empty runs never pass a case. It also
shows that the roster catalog matches its sources and the matrix.

A passed case is local product evidence. It is not live provider, native
harness, quality or latency evidence.
