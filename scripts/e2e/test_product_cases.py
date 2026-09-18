#!/usr/bin/env python3
"""Checks that product case evaluation cannot pass on names, gaps or partial runs."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import product_cases  # noqa: E402

CATALOG = {
    "schema_version": 1,
    "suite": "demo",
    "tier": "rust-product-integration",
    "targets": ["alpha", "beta"],
    "cases": [
        {"id": "first", "assertions": ["a"], "tests": ["alpha::one", "beta::two"]},
        {"id": "whole", "assertions": ["b"], "tests": "all"},
    ],
}
SUMMARY = "test result: ok. {n} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"


def log(*lines, summaries=2):
    return "\n".join(list(lines) + [SUMMARY.format(n=1)] * summaries) + "\n"


def statuses(text):
    return {r["case"]: r["status"] for r in product_cases.evaluate(CATALOG, text)}


class EvaluateTests(unittest.TestCase):
    def test_every_mapped_test_passing_in_a_complete_run_passes(self):
        text = log("test one ... ok", "     Running tests/beta.rs (x)", "test two ... ok")
        self.assertEqual(statuses(text), {"first": "passed", "whole": "passed"})

    def test_a_missing_mapped_test_is_missing_not_passed(self):
        self.assertEqual(statuses(log("test one ... ok"))["first"], "missing")

    def test_failed_or_ignored_mapped_tests_fail_the_case(self):
        for status in ("FAILED", "ignored"):
            text = log("test one ... ok", f"test two ... {status}")
            self.assertEqual(statuses(text)["first"], "failed", status)

    def test_a_tests_own_exact_child_run_is_not_a_target(self):
        # A test re-runs its binary with --exact; the child's lines join the log.
        child = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 14 filtered out"
        text = log("test one ... ok", child, "test one ... ok", "test two ... ok")
        records = {r["case"]: r for r in product_cases.evaluate(CATALOG, text)}
        self.assertEqual(records["first"]["status"], "passed")
        self.assertEqual(records["whole"]["status"], "passed")
        self.assertEqual(records["whole"]["nested_child_runs"], 1)
        # The worst status wins: a failing parent is not rescued by its child.
        text = log("test one ... ok", child, "test one ... FAILED", "test two ... ok")
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})

    def test_a_filtered_target_run_leaves_the_target_count_short(self):
        filtered = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out"
        text = "test one ... ok\ntest two ... ok\n" + SUMMARY.format(n=1) + "\n" + filtered + "\n"
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})

    def test_ignore_reasons_are_parsed(self):
        results, _, _ = product_cases.parse_log("test live ... ignored, needs a live binary\n")
        self.assertEqual(results, {"live": "ignored"})

    def test_an_incomplete_run_fails_every_case(self):
        # One target never reported, even though the mapped tests passed.
        text = log("test one ... ok", "test two ... ok", summaries=1)
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})
        filtered = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out"
        text = "test one ... ok\ntest two ... ok\n" + SUMMARY.format(n=1) + "\n" + filtered + "\n"
        self.assertEqual(statuses(text)["whole"], "failed")

    def test_only_declared_opt_in_tests_may_be_ignored(self):
        ignored = "test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out"
        text = "test one ... ok\ntest two ... ok\ntest live ... ignored\n" + SUMMARY.format(n=1) + "\n" + ignored + "\n"
        self.assertEqual(statuses(text)["whole"], "failed")
        declared = dict(CATALOG, allowed_ignored=[{"test": "beta::live", "reason": "needs a live binary"}])
        records = {r["case"]: r for r in product_cases.evaluate(declared, text)}
        self.assertEqual(records["whole"]["status"], "passed")
        self.assertEqual(records["whole"]["ignored"], ["live"])
        # A count without a matching ignored line is not complete.
        hidden = "test one ... ok\ntest two ... ok\n" + SUMMARY.format(n=1) + "\n" + ignored + "\n"
        self.assertEqual(product_cases.evaluate(declared, hidden)[1]["status"], "failed")

    def test_an_empty_run_does_not_pass(self):
        text = "\n".join(["test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"] * 2)
        self.assertEqual(statuses(text), {"first": "missing", "whole": "failed"})


class CatalogTests(unittest.TestCase):
    def test_every_product_catalog_matches_sources_and_the_matrix(self):
<<<<<<< HEAD
        for path in sorted((Path(__file__).parent / "product").glob("*.json")):
            catalog = product_cases.load_catalog(path)
            self.assertEqual(product_cases.check(catalog), [], path.name)

    def test_a_declared_ignored_test_must_really_be_ignored_and_cannot_back_a_case(self):
        catalog = product_cases.load_catalog(Path(__file__).parent / "product/roster.json")
        live = "cass_adapter::actual_installed_cass_exports_synthetic_session"
        declared = dict(catalog, targets=catalog["targets"] + ["cass_adapter"],
                        allowed_ignored=[{"test": live, "reason": "needs an installed cass"}])
        self.assertEqual(product_cases.check(declared), [])
        wrong = dict(declared, allowed_ignored=[{"test": "roster_cli::concatenated_pages_equal_the_whole_snapshot",
                                                 "reason": "x"}])
        self.assertTrue(any("is not an #[ignore] test" in p for p in product_cases.check(wrong)))
        unexplained = dict(declared, allowed_ignored=[{"test": live, "reason": ""}])
        self.assertTrue(any("needs a reason" in p for p in product_cases.check(unexplained)))
        cases = [dict(case) for case in declared["cases"]]
        cases[0]["tests"] = cases[0]["tests"] + [live]
        problems = product_cases.check(dict(declared, cases=cases))
=======
        for name in ("roster", "context"):
            catalog = product_cases.load_catalog(Path(__file__).parent / f"product/{name}.json")
            self.assertEqual(product_cases.check(catalog), [], name)

    def test_a_declared_ignored_test_must_really_be_ignored_and_cannot_back_a_case(self):
        catalog = product_cases.load_catalog(Path(__file__).parent / "product/context.json")
        live = catalog["allowed_ignored"][0]["test"]
        broken = dict(catalog, allowed_ignored=[{"test": "context_contract::task_anchors", "reason": "x"}])
        self.assertTrue(any("is not an #[ignore] test" in p for p in product_cases.check(broken)))
        cases = [dict(case) for case in catalog["cases"]]
        cases[0]["tests"] = cases[0]["tests"] + [live]
        problems = product_cases.check(dict(catalog, cases=cases))
>>>>>>> bfab6db (wipbase)
        self.assertTrue(any("cannot establish a case" in p for p in problems))

    def test_stale_names_unknown_targets_and_uncatalogued_cases_are_reported(self):
        catalog = product_cases.load_catalog(Path(__file__).parent / "product/roster.json")
        broken = dict(catalog, cases=[dict(case) for case in catalog["cases"]])
        broken["cases"][0]["tests"] = ["roster_cli::roster_inspection", "nowhere::thing"]
        dropped = broken["cases"].pop()
        problems = product_cases.check(broken)
        self.assertTrue(any("roster_inspection is not a #[test] fn" in p for p in problems))
        self.assertTrue(any("nowhere is not a suite target" in p for p in problems))
        self.assertTrue(any(f"case {dropped['id']} has no catalog entry" in p for p in problems))


if __name__ == "__main__":
    unittest.main()
