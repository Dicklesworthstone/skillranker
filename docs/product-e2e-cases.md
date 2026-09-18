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
  `test result: ok` per target, with none failed or filtered, and at least one
  passed. An `all` case needs only the complete run.
- `targets CATALOG`, which prints the suite's targets.

A run may not skip tests silently. It may report as ignored only the opt-in
tests the catalog lists under `allowed_ignored`, each with a reason. Such a
test must carry `#[ignore]` in its source, must not back any case, and is named
in the `all` case's output. An example is
`cass_adapter::actual_installed_cass_exports_synthetic_session`, which needs an
installed cass binary; build workers have none.

A test may re-run its own binary with `--exact NAME`. With `--nocapture`, that
child's output joins the log. Such a child always filters, so filtered
summaries never count as target summaries. Repeated result lines for one name
merge, and the worst status wins.

Cargo's stderr and the test binaries' stdout interleave unpredictably, so
results are attributed by test name. The static check is what makes names
unambiguous.

`scripts/e2e/product.sh --suite NAME --artifacts DIR` runs any product suite:
- it checks the catalog first;
- it runs every catalog target remotely through RCH, with no test-name filter;
- it writes per-case records to `cases.jsonl` in its artifact directory.

The suite passes only if every case passes. Catalogs live in
`scripts/e2e/product/`. `scripts/e2e/run.sh --suite roster` delegates to this
runner for the P2 catalog.

`scripts/e2e/test_product_cases.py` shows that missing, failed, undeclared
ignored, duplicated, incomplete, filtered and empty runs never pass a case. It
also shows that:
- every catalog matches its sources and the matrix;
- a declared ignored test must really be ignored, needs a reason and cannot
  back a case;
- a test's own `--exact` child run neither counts as a target nor rescues a
  failure.

A passed case is local product evidence. It is not live provider, native
harness, quality or latency evidence.
