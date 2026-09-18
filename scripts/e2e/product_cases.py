#!/usr/bin/env python3
"""Map product e2e case IDs to the real Rust tests that establish them.

A product suite runs whole Rust integration targets. Its catalog names, for
each contract-matrix case, the tests whose passing establishes that case.
`check` verifies the catalog statically against the sources and the matrix.
`evaluate` reads an actual cargo test log and reports each case passed,
failed or missing. A case never passes by name alone: every mapped test must
appear exactly once as `... ok`, and the whole run must be complete.

Cargo's stderr and the test binaries' stdout interleave unpredictably, so
results are attributed by test name. `check` guarantees that every mapped name
is a test in exactly one of the suite's targets.

A test may re-run its own binary with `--exact NAME`, and with `--nocapture`
that child's output joins the log. A product run never filters, so only
unfiltered summaries count as target summaries. A repeated result line for one
name merges, and the worst status wins.
"""
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MATRIX = ROOT / "tests/contract_matrix.toml"
# libtest appends a reason to ignored results: `... ignored, <reason>`.
RESULT = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)(?:, .*)?$")
# The worst status wins when one name reports more than once.
RANK = {"ok": 0, "ignored": 1, "FAILED": 2}
SUMMARY = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out"
)
# A #[test] attribute, optional further attributes, then the function name.
TEST_FN = re.compile(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*fn\s+([a-z0-9_]+)\s*\(")
IGNORED_FN = re.compile(r"#\[ignore[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*fn\s+([a-z0-9_]+)\s*\(")
ALL = "all"


def load_catalog(path):
    catalog = json.loads(Path(path).read_text())
    if catalog.get("schema_version") != 1 or catalog.get("tier") != "rust-product-integration":
        raise ValueError("unsupported catalog")
    ids = [case["id"] for case in catalog["cases"]]
    if len(ids) != len(set(ids)) or not catalog["targets"]:
        raise ValueError("duplicate case IDs or no targets")
    return catalog


def defined_tests(target):
    source = ROOT / "tests" / f"{target}.rs"
    return set(TEST_FN.findall(source.read_text())) if source.is_file() else None


def ignored_tests(target):
    source = ROOT / "tests" / f"{target}.rs"
    return set(IGNORED_FN.findall(source.read_text())) if source.is_file() else set()


def allowed_ignored(catalog):
    """Declared opt-in tests that a complete run may report as ignored."""
    return {entry["test"].partition("::")[2] for entry in catalog.get("allowed_ignored", [])}


def check(catalog, matrix_path=MATRIX):
    """Return a list of problems; empty means the catalog is consistent."""
    problems = []
    defined = {}
    for target in catalog["targets"]:
        tests = defined_tests(target)
        if tests is None:
            problems.append(f"missing target source tests/{target}.rs")
            tests = set()
        defined[target] = tests
    for case in catalog["cases"]:
        if case["tests"] == ALL:
            continue
        if not case["tests"]:
            problems.append(f"{case['id']}: no tests")
        for reference in case["tests"]:
            target, _, name = reference.partition("::")
            if target not in defined:
                problems.append(f"{case['id']}: {target} is not a suite target")
            elif name not in defined[target]:
                problems.append(f"{case['id']}: {reference} is not a #[test] fn")
            owners = [t for t, tests in defined.items() if name in tests]
            if len(owners) > 1:
                problems.append(f"{case['id']}: {name} is ambiguous across {owners}")
    for entry in catalog.get("allowed_ignored", []):
        target, _, name = entry["test"].partition("::")
        if not entry.get("reason"):
            problems.append(f"allowed ignored {entry['test']} needs a reason")
        if target not in defined or name not in ignored_tests(target):
            problems.append(f"allowed ignored {entry['test']} is not an #[ignore] test")
        if any(entry["test"] in c["tests"] for c in catalog["cases"] if c["tests"] != ALL):
            problems.append(f"allowed ignored {entry['test']} cannot establish a case")
    matrix = tomllib.loads(Path(matrix_path).read_text())
    cases = {case["id"]: set(case["assertions"]) for case in catalog["cases"]}
    required = set()
    for row in matrix["boundaries"]:
        if row["e2e_suite"] != catalog["suite"]:
            continue
        for case in row["e2e_cases"]:
            required.add(case)
            if case not in cases:
                problems.append(f"{row['id']}: case {case} has no catalog entry")
            elif not set(row["assertion_ids"]) <= cases[case]:
                problems.append(f"{row['id']}: case {case} lacks its assertions")
    for case in cases.keys() - required:
        problems.append(f"catalog case {case} is not in the matrix")
    return problems


def parse_log(text):
    results = {}
    summaries = []
    nested = 0
    for line in text.splitlines():
        line = line.rstrip()
        match = RESULT.match(line)
        if match:
            name, status = match.groups()
            if RANK[status] >= RANK.get(results.get(name, "ok"), 0):
                results[name] = status
            continue
        match = SUMMARY.match(line)
        if match:
            summary = tuple([match[1]] + [int(x) for x in match.groups()[1:]])
            # A filtered summary comes from a test's own `--exact` child run.
            if summary[5] > 0:
                nested += 1
            else:
                summaries.append(summary)
    return results, summaries, nested


def evaluate(catalog, text):
<<<<<<< HEAD
    results, summaries, nested = parse_log(text)
    ignored = {name for name, status in results.items() if status == "ignored"}
    complete = (
        len(summaries) == len(catalog["targets"])
        and all(s[0] == "ok" and s[2] == 0 for s in summaries)
=======
    results, duplicates, summaries = parse_log(text)
    ignored = {name for name, status in results.items() if status == "ignored"}
    complete = (
        len(summaries) == len(catalog["targets"])
        and all(s[0] == "ok" and s[2] == 0 and s[5] == 0 for s in summaries)
>>>>>>> bfab6db (wipbase)
        # Only declared opt-in tests may be ignored, and the counts must agree.
        and ignored <= allowed_ignored(catalog)
        and sum(s[3] for s in summaries) == len(ignored)
        and sum(s[1] for s in summaries) > 0
        and not any(status == "FAILED" for status in results.values())
    )
    records = []
    for case in catalog["cases"]:
        if case["tests"] == ALL:
            status = "passed" if complete else "failed"
            detail = {
                "targets": len(summaries),
                "passed": sum(s[1] for s in summaries),
                "failed": sum(s[2] for s in summaries),
                "ignored": sorted(ignored),
<<<<<<< HEAD
                "nested_child_runs": nested,
=======
                "filtered": sum(s[5] for s in summaries),
>>>>>>> bfab6db (wipbase)
            }
        else:
            names = [reference.partition("::")[2] for reference in case["tests"]]
            missing = [n for n in names if n not in results]
            failed = [n for n in names if results.get(n) in ("FAILED", "ignored")]
            status = "failed" if failed else "missing" if missing else "passed"
            if status == "passed" and not complete:
                status = "failed"
            detail = {"tests": len(names), "missing": missing, "failed": failed}
        records.append({
            "schema_version": 1,
            "suite": catalog["suite"],
            "case": case["id"],
            "assertions": case["assertions"],
            "status": status,
            **detail,
        })
    return records


def main(argv):
    if len(argv) == 3 and argv[1] == "check":
        problems = check(load_catalog(argv[2]))
        for problem in problems:
            print(problem, file=sys.stderr)
        return 1 if problems else 0
    if len(argv) == 3 and argv[1] == "targets":
        for target in load_catalog(argv[2])["targets"]:
            print(target)
        return 0
    if len(argv) == 4 and argv[1] == "evaluate":
        catalog = load_catalog(argv[2])
        records = evaluate(catalog, Path(argv[3]).read_text(errors="replace"))
        for record in records:
            print(json.dumps(record, sort_keys=True))
        passed = sum(r["status"] == "passed" for r in records)
        print(json.dumps({"schema_version": 1, "suite": catalog["suite"],
                          "cases": len(records), "passed": passed,
                          "status": "passed" if passed == len(records) else "failed"}))
        return 0 if passed == len(records) else 1
    print("usage: product_cases.py check CATALOG | targets CATALOG | evaluate CATALOG LOG",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
