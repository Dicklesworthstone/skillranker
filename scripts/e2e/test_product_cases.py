#!/usr/bin/env python3
"""Checks that product case evaluation cannot pass on names, gaps or partial runs."""
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

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

    def test_summaries_without_result_lines_cannot_establish_the_whole_suite(self):
        self.assertEqual(statuses(log())["whole"], "failed")

    def test_a_missing_unmapped_result_fails_every_otherwise_passing_case(self):
        # Mapped results survive, but one result elsewhere in the suite is lost.
        text = "\n".join(["test one ... ok", "test two ... ok",
                          SUMMARY.format(n=1), SUMMARY.format(n=2)])
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})
        self.assertEqual(statuses("test three ... ok\n" + text),
                         {"first": "passed", "whole": "passed"})

    def test_one_empty_target_does_not_hide_behind_another_passing_target(self):
        text = "\n".join(["test one ... ok", "test two ... ok",
                          SUMMARY.format(n=2), SUMMARY.format(n=0)])
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})

    def test_extra_result_lines_do_not_match_a_complete_target_accounting(self):
        text = log("test one ... ok", "test two ... ok", "test foreign ... ok")
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})

    def test_measurements_cannot_substitute_for_test_execution(self):
        measured = SUMMARY.format(n=1).replace("0 measured", "1 measured")
        text = "\n".join(["test one ... ok", "test two ... ok",
                          SUMMARY.format(n=1), measured])
        self.assertEqual(statuses(text), {"first": "failed", "whole": "failed"})


class CatalogTests(unittest.TestCase):
    def test_duplicate_unmapped_names_are_ambiguous_for_whole_suite_accounting(self):
        catalog = dict(CATALOG, cases=[CATALOG["cases"][1]])
        with patch.object(product_cases, "defined_tests", return_value={"same"}), \
                patch.object(product_cases, "ignored_tests", return_value=set()):
            problems = product_cases.check(catalog)
        self.assertTrue(any("same is ambiguous across" in p for p in problems))

    def test_every_product_catalog_matches_sources_and_the_matrix(self):
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

    def test_a_new_target_cannot_hide_an_undeclared_ignored_test(self):
        catalog = product_cases.load_catalog(Path(__file__).parent / "product/roster.json")
        added = dict(catalog, targets=catalog["targets"] + ["cass_adapter"])
        self.assertTrue(any("actual_installed_cass_exports_synthetic_session needs an allowed_ignored"
                            in p for p in product_cases.check(added)))


class ProductArtifactTests(unittest.TestCase):
    """Mechanics only: synthetic libtest/RCH receipts never qualify a product."""
    def files(self, text=None, catalog=None, exit_code=0):
        import json
        import tempfile
        directory = Path(tempfile.mkdtemp(prefix='sr-product-artifact-test-'))
        catalog_path = directory / 'catalog.json'
        catalog_path.write_text(json.dumps(catalog or CATALOG))
        product_cases.product_artifact_start(catalog_path, directory)
        (directory / 'tests.log').write_text(text or log('test one ... ok', 'test two ... ok'))
        (directory / 'remote-result.json').write_text(json.dumps({
            'location': 'remote', 'outcome': 'completed', 'remote_exit_code': exit_code,
            'admission': {'strict_remote': True, 'terminal_acknowledged': True}}))
        return directory, catalog_path

    def read(self, directory):
        import json
        return json.loads((directory / 'summary.json').read_text())

    def test_complete_case_logs_are_private_and_bound_to_actual_local_inputs(self):
        import hashlib
        import json
        directory, catalog = self.files()
        self.assertEqual(product_cases.product_artifact_record(catalog, directory, 0), 0)
        summary = self.read(directory)
        self.assertEqual(summary['cases_passed'], 2)
        self.assertEqual(summary['assertions'], 2)
        self.assertEqual(summary['quality_gate'], 'not-established')
        self.assertFalse(summary['identity']['remote_inputs_verified'])
        self.assertEqual(summary['events_sha256'], hashlib.sha256((directory / 'events.jsonl').read_bytes()).hexdigest())
        events = [json.loads(line) for line in (directory / 'events.jsonl').read_text().splitlines()]
        self.assertEqual(events[1]['actual']['tests'], {'alpha::one': 'ok', 'beta::two': 'ok'})
        self.assertIsNone(events[1]['elapsed_ns'])
        self.assertIsNone(events[1]['fault_reached'])
        self.assertGreater(summary['elapsed_ns'], 0)
        for name in ('started.json', 'events.jsonl', 'summary.json'):
            self.assertEqual((directory / name).stat().st_mode & 0o077, 0)

    def test_failed_or_unstarted_run_retains_missing_case_evidence(self):
        for text in ('', 'test one ... FAILED\n'):
            directory, catalog = self.files(text='placeholder' if not text else text, exit_code=101)
            self.assertEqual(product_cases.product_artifact_record(catalog, directory, 101), 1)
            summary = self.read(directory)
            self.assertEqual(summary['cases_passed'], 0)
            self.assertEqual(summary['exit'], 101)
            self.assertEqual(summary['status'], 'failed')
            self.assertTrue((directory / 'events.jsonl').is_file())

    def test_remote_acknowledgment_and_unchanged_identity_are_required(self):
        import json
        directory, catalog = self.files()
        (directory / 'remote-result.json').write_text(json.dumps({'location': 'local', 'outcome': 'completed', 'remote_exit_code': 0}))
        self.assertEqual(product_cases.product_artifact_record(catalog, directory, 0), 1)
        directory, catalog = self.files()
        with catalog.open('a') as stream:
            stream.write('\n')
        self.assertEqual(product_cases.product_artifact_record(catalog, directory, 0), 1)
        self.assertFalse(self.read(directory)['identity_stable'])

    def test_planted_leak_detector_has_success_and_violation_controls(self):
        directory, catalog = self.files()
        with (directory / 'tests.log').open('ab') as stream:
            stream.write(product_cases.CANARY + b'\n')
        self.assertEqual(product_cases.product_artifact_record(catalog, directory, 0), 1)
        self.assertFalse(self.read(directory)['leak_free'])
        self.assertNotIn(product_cases.CANARY, (directory / 'events.jsonl').read_bytes())
        self.assertNotIn(product_cases.CANARY, (directory / 'summary.json').read_bytes())
        directory, catalog = self.files()
        self.assertEqual(product_cases.product_artifact_record(catalog, directory, 0), 0)
        self.assertTrue(self.read(directory)['leak_free'])

    def test_p5_needs_real_binary_control_receipts_not_only_case_names(self):
        import json
        catalog = dict(CATALOG, suite='replay')
        directory, catalog_path = self.files(catalog=catalog)
        self.assertEqual(product_cases.product_artifact_record(catalog_path, directory, 0), 1)
        self.assertFalse(self.read(directory)['controls_complete'])
        directory, catalog_path = self.files(catalog=catalog)
        commit = json.loads((directory / 'started.json').read_text())['identity']['git_commit']
        receipts = []
        for scenario, cap, calls, unknown, fault in [('useful', '16', 12, 0, False), ('stale-renamed-option', '16', 11, 1, True), ('useful', '12', 10, 0, False), ('useful', '4', 2, 0, False)]:
            receipts.append({'case': 'prespecified-wire-variants', 'scenario': scenario, 'attempt_cap': cap,
                             'compiled_source': commit, 'binary_blake3': 'a' * 64, 'platform': 'linux-x86_64',
                             'computation_digest': 'b' * 64, 'numeric_build_digest': 'c' * 64,
                             'elapsed_ns': 1, 'http_attempts': calls, 'unknown_usage_attempts': unknown,
                             'fault_reached': fault, 'leak_detector_checked': True})
        with (directory / 'tests.log').open('a') as stream:
            for receipt in receipts:
                stream.write(json.dumps(receipt, separators=(',', ':')) + '\n')
        self.assertEqual(product_cases.product_artifact_record(catalog_path, directory, 0), 0)
        self.assertEqual(self.read(directory)['binary']['blake3'], 'a' * 64)
        # A malicious diagnostic resembling a receipt cannot copy its text to
        # private structured artifacts or rescue the missing real controls.
        directory, catalog_path = self.files(catalog=catalog)
        receipts[0]['compiled_source'] = 'untrusted-secret-text'
        with (directory / 'tests.log').open('a') as stream:
            stream.write(json.dumps(receipts[0], separators=(',', ':')) + '\n')
        self.assertEqual(product_cases.product_artifact_record(catalog_path, directory, 0), 1)
        self.assertNotIn(b'untrusted-secret-text', (directory / 'events.jsonl').read_bytes())

    def test_artifact_input_bounds_and_duplicate_unicode_definitions_are_strict(self):
        for data in (b'{"same":1,"s\\u0061me":2}', b'{"x":NaN}', b'{"x":1e999}',
                     b'{"x":"\\ud800"}', b'[' * 66 + b'0' + b']' * 66,
                     b' ' * (product_cases.MAX_ARTIFACT + 1)):
            with self.assertRaises((ValueError, UnicodeError, RecursionError)):
                product_cases.artifact_json(data)
        self.assertEqual(product_cases.artifact_json(b'{"same":1}'), {'same': 1})

    def test_p5_foreign_source_cannot_pass_by_sharing_the_head_prefix(self):
        import json
        directory, catalog = self.files(catalog=dict(CATALOG, suite='replay'))
        commit = json.loads((directory / 'started.json').read_text())['identity']['git_commit']
        # A valid-looking 64-hex marker, not this checkout's 40-hex commit.
        self.assertEqual(len(commit), 40)
        foreign = commit + '0' * 24
        with (directory / 'tests.log').open('a') as stream:
            for scenario, cap, calls, unknown, fault in [('useful', '16', 12, 0, False), ('stale-renamed-option', '16', 11, 1, True), ('useful', '12', 10, 0, False), ('useful', '4', 2, 0, False)]:
                stream.write(json.dumps({'case': 'prespecified-wire-variants',
                    'scenario': scenario, 'attempt_cap': cap, 'compiled_source': foreign,
                    'binary_blake3': 'a' * 64, 'platform': 'linux-x86_64',
                    'computation_digest': 'b' * 64, 'numeric_build_digest': 'c' * 64,
                    'elapsed_ns': 1, 'http_attempts': calls, 'unknown_usage_attempts': unknown,
                    'fault_reached': fault, 'leak_detector_checked': True}, separators=(',', ':')) + '\n')
        self.assertEqual(product_cases.product_artifact_record(catalog, directory, 0), 1)
        self.assertFalse(self.read(directory)['controls_complete'])

    def test_changed_artifact_directory_privacy_fails_before_new_logs(self):
        directory, catalog = self.files()
        directory.chmod(0o755)
        try:
            with self.assertRaises(ValueError):
                product_cases.product_artifact_record(catalog, directory, 0)
            self.assertFalse((directory / 'events.jsonl').exists())
        finally:
            directory.chmod(0o700)

    def test_artifact_reads_reject_fifo_symlink_and_oversize_with_regular_success(self):
        import os
        import tempfile
        directory = Path(tempfile.mkdtemp(prefix='sr-artifact-read-test-'))
        regular = directory / 'regular'
        regular.write_bytes(b'OK')
        self.assertEqual(product_cases.bounded_bytes(regular, 2), b'OK')
        with self.assertRaises(ValueError):
            product_cases.bounded_bytes(regular, 1)
        link = directory / 'link'
        link.symlink_to(regular)
        with self.assertRaises(OSError):
            product_cases.bounded_bytes(link, 2)
        fifo = directory / 'fifo'
        os.mkfifo(fifo, 0o600)
        with self.assertRaises(ValueError):
            product_cases.bounded_bytes(fifo, 2)


if __name__ == "__main__":
    unittest.main()
