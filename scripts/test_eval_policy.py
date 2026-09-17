#!/usr/bin/env python3
"""Adversarial contract tests; fixtures are synthetic and confer no quality proof."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import validate_eval_policy as policy


class EvaluationPolicyContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Keep synthetic artifacts for diagnosis rather than deleting evidence.
        cls.artifacts = Path(tempfile.mkdtemp(prefix="skillranker-policy-tests-"))
        print(f"synthetic test artifacts: {cls.artifacts}", file=sys.stderr)

    def setUp(self):
        self.policy = policy.load_json(policy.DEFAULT_POLICY)
        self.expected = policy.load_json(policy.DEFAULT_EXPECTED)
        self.cases = policy.load_jsonl(policy.DEFAULT_CASES)

    def test_valid_contracts_are_accepted(self):
        policy.validate_policy(self.policy)
        policy.validate_cases(self.cases, self.policy)
        policy.validate_expected(self.expected)

    @unittest.skipUnless(os.name == "posix", "uses POSIX null device")
    def test_cli_rejects_nonregular_inputs_with_sanitized_diagnostic(self):
        result = subprocess.run([sys.executable, str(policy.ROOT / "scripts/validate_eval_policy.py"),
                                 "--policy", "/dev/null"], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr, b"validation failed: artifact must be a regular file\n")

    def test_false_abstention_cannot_be_scored_as_correct(self):
        self.expected["loss_examples"][1].update(loss=0, normalized_y=0)
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_candidate_coverage_is_not_set_recall(self):
        self.expected["coverage_examples"][0]["set_recall"] = 1
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_candidate_average_cannot_hide_its_loss(self):
        self.expected["always_abstain_counterexample"]["expected"]["candidate_policy_mean_loss"] = 0
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_unstarted_cases_cannot_disappear(self):
        self.expected["missing_failed_unstarted_examples"][2]["unstarted_cases"] = 0
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_family_independence_does_not_replace_model_assumptions(self):
        self.policy["uncertainty_and_sampling"]["model_assumptions_required"] = False
        with self.assertRaises(SystemExit):
            policy.validate_policy(self.policy)

    def test_ambiguous_and_nonfinite_json_are_rejected(self):
        for raw in [b'{"schema":1,"schema":2}', b'{"x":NaN}', b'{"x":Infinity}', b'{"x":1e999}', b'\xff']:
            with self.subTest(raw_kind=raw[:12]), self.assertRaises(SystemExit):
                policy.strict_json(raw)

    def test_duplicate_case_definitions_cannot_inflate_counts(self):
        self.cases.append(copy.deepcopy(self.cases[0]))
        with self.assertRaises(SystemExit):
            policy.validate_cases(self.cases, self.policy)

    def test_nesting_is_bounded_independently_of_bytes(self):
        self.assertEqual(policy.strict_json(b'[[0]]'), [[0]])
        with self.assertRaises(SystemExit):
            policy.strict_json(b'[' * 65 + b'0' + b']' * 65)

    def test_harm_gate_flags_must_be_booleans(self):
        self.expected["harm_interval_examples"][1]["passes_2_percent_gate"] = "false"
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_empty_case_collection_does_not_pass(self):
        with self.assertRaises(SystemExit):
            policy.validate_cases([], self.policy)

    def test_boolean_losses_are_not_numeric_labels(self):
        self.expected["loss_examples"][0]["loss"] = False
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_synthetic_cases_cannot_be_relabelled_as_holdout_evidence(self):
        self.cases[0]["split"] = "held_out"
        with self.assertRaises(SystemExit):
            policy.validate_cases(self.cases, self.policy)

    def test_negative_loss_cannot_improve_a_candidate(self):
        counter = self.expected["always_abstain_counterexample"]
        counter["cohort"][0]["candidate_policy_loss"] = -99
        counter["expected"].update(candidate_policy_total_loss=-97,
                                   candidate_policy_mean_loss=-97/6,
                                   candidate_policy_mean_normalized_loss=-97/12)
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_available_reference_cannot_require_an_additional_invocation(self):
        case = next(x for x in self.cases if x["case_kind"] == "loaded_reference_empty_y")
        case["acceptable_additional_invocations_y"] = ["unexpected-skill"]
        with self.assertRaises(SystemExit):
            policy.validate_cases(self.cases, self.policy)

    def test_positive_cases_cannot_opt_out_of_metrics(self):
        self.cases[0]["oracle"]["advisory_metrics_eligible"] = False
        with self.assertRaises(SystemExit):
            policy.validate_cases(self.cases, self.policy)

    def test_thresholds_require_json_numbers(self):
        for group, key in [("controlled_harm", "maximum_one_sided_95_upper_bound"),
                           ("operational_fallback", "maximum_fallback_rate")]:
            with self.subTest(group=group):
                value = copy.deepcopy(self.policy)
                threshold = value["promotion_requirements"][group]
                threshold[key] = str(threshold[key])
                with self.assertRaises(SystemExit):
                    policy.validate_policy(value)

    def test_duplicate_expected_definitions_are_not_last_wins(self):
        for collection in ("loss_examples", "coverage_examples", "missing_failed_unstarted_examples", "harm_interval_examples"):
            with self.subTest(collection=collection):
                value = copy.deepcopy(self.expected)
                value[collection].append(copy.deepcopy(value[collection][0]))
                with self.assertRaises(SystemExit):
                    policy.validate_expected(value)

    def test_expected_numbers_do_not_coerce_strings_booleans_or_fractional_counts(self):
        fields = [
            (("loss_examples", 0, "normalized_y"), ["0", False]),
            (("loss_examples", 0, "y_nonempty"), [1, 1.0, "true"]),
            (("always_abstain_counterexample", "expected", "candidate_policy_mean_loss"), ["0.3333333333"]),
            (("always_abstain_counterexample", "expected", "candidate_policy_total_loss"), [2.0, "2"]),
            (("coverage_examples", 0, "set_recall"), ["0.05"]),
            (("harm_interval_examples", 1, "flagged"), [False, "0", 0.9]),
            (("harm_interval_examples", 1, "n"), ["150", 150.9]),
            (("cohort_separation_examples", "operational_fallback_minimum_invocations"), [500.0, "500"]),
        ]
        for path, bad_values in fields:
            for bad in bad_values:
                with self.subTest(path=path, invalid_type=type(bad).__name__):
                    value = copy.deepcopy(self.expected)
                    target = value
                    for key in path[:-1]:
                        target = target[key]
                    target[path[-1]] = bad
                    with self.assertRaises(SystemExit):
                        policy.validate_expected(value)

    def test_negative_counts_cannot_balance_an_equation(self):
        self.expected["missing_failed_unstarted_examples"][0].update(
            available_complete_cases=11, missing_replay_responses=-1)
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_timeout_replacement_preserves_the_original_denominator(self):
        self.expected["missing_failed_unstarted_examples"][1].update(
            attempted_timeouts_without_response=0, expected_total_loss=0)
        with self.assertRaises(SystemExit):
            policy.validate_expected(self.expected)

    def test_coverage_collections_are_unique_arrays_not_silently_coerced_sets(self):
        for field in ("acceptable_y", "retrieved"):
            for replacement in ("skill-a", ["skill-a", "skill-a"]):
                with self.subTest(field=field, replacement_type=type(replacement).__name__):
                    value = copy.deepcopy(self.expected)
                    value["coverage_examples"][0][field] = replacement
                    with self.assertRaises(SystemExit):
                        policy.validate_expected(value)

    def test_oracles_need_a_visible_acceptable_skill_and_correct_loss(self):
        for field, value in (("expected_correct_top_one", "release-preparations"),
                             ("expected_abstain_loss", 0)):
            with self.subTest(field=field):
                cases = copy.deepcopy(self.cases)
                cases[0]["oracle"][field] = value
                with self.assertRaises(SystemExit):
                    policy.validate_cases(cases, self.policy)
        self.cases[0]["acceptable_additional_invocations_y"] = ["absent-skill"]
        self.cases[0]["oracle"]["expected_correct_top_one"] = "absent-skill"
        with self.assertRaises(SystemExit):
            policy.validate_cases(self.cases, self.policy)

    def test_missing_oracle_assertions_cannot_silently_remove_contract_coverage(self):
        for index, original in enumerate(self.cases):
            for field in original["oracle"]:
                cases = copy.deepcopy(self.cases)
                del cases[index]["oracle"][field]
                with self.subTest(kind=original["case_kind"], field=field), self.assertRaises(SystemExit):
                    policy.validate_cases(cases, self.policy)

    def test_oracle_safety_flags_cannot_be_reversed(self):
        for index, original in enumerate(self.cases):
            for field, value in original["oracle"].items():
                if type(value) is bool:
                    cases = copy.deepcopy(self.cases)
                    cases[index]["oracle"][field] = not value
                    with self.subTest(kind=original["case_kind"], field=field), self.assertRaises(SystemExit):
                        policy.validate_cases(cases, self.policy)

    def test_case_roster_definitions_and_near_misses_are_consistent(self):
        cases = copy.deepcopy(self.cases)
        cases[0]["visible_roster"].append(copy.deepcopy(cases[0]["visible_roster"][0]))
        with self.assertRaises(SystemExit):
            policy.validate_cases(cases, self.policy)
        self.cases[0]["near_miss_skill_ids"] = ["rust-test-triage"]
        with self.assertRaises(SystemExit):
            policy.validate_cases(self.cases, self.policy)

    def test_namespaced_skill_ids_remain_valid_references(self):
        self.cases[0]["visible_roster"][0]["skill_id"] = "plugin:rust-test-triage"
        self.cases[0]["acceptable_additional_invocations_y"] = ["plugin:rust-test-triage"]
        self.cases[0]["oracle"]["expected_correct_top_one"] = "plugin:rust-test-triage"
        policy.validate_cases(self.cases, self.policy)

    def test_frozen_examples_cannot_drop_the_failure_half(self):
        value = copy.deepcopy(self.expected)
        value["loss_examples"].pop(1)
        with self.assertRaises(SystemExit):
            policy.validate_expected(value)
        value = copy.deepcopy(self.expected)
        value["harm_interval_examples"][0] = copy.deepcopy(value["harm_interval_examples"][1])
        value["harm_interval_examples"][0]["example_id"] = "renamed-success"
        with self.assertRaises(SystemExit):
            policy.validate_expected(value)

    def test_expected_artifact_identity_and_non_evidence_status_are_required(self):
        for field, bad in (("policy_id", None), ("status", "measured_results")):
            with self.subTest(field=field):
                value = copy.deepcopy(self.expected)
                value[field] = bad
                with self.assertRaises(SystemExit):
                    policy.validate_expected(value)

    def test_policy_cannot_relax_failure_semantics_or_use_fractional_cohort_counts(self):
        for path, bad in [
            (("promotion_requirements", "relevance_cohort", "minimum_primary_cases"), 300.0),
            (("promotion_requirements", "operational_fallback", "include_provider_outages"), False),
            (("missing_failed_unstarted_semantics", "operational_failure_after_case_attempted", "loss"), 0),
        ]:
            with self.subTest(path=path):
                value = copy.deepcopy(self.policy)
                value[path[0]][path[1]][path[2]] = bad
                with self.assertRaises(SystemExit):
                    policy.validate_policy(value)

    def test_escaped_surrogates_are_not_unicode_scalar_values(self):
        self.assertEqual(policy.strict_json(b'"\\ud83e\\udd80"'), "🦀")
        for raw in [b'"\\ud800"', b'"\\udfff"', b'{"\\ud800":0}']:
            with self.subTest(raw=raw), self.assertRaises(SystemExit):
                policy.strict_json(raw)

    def test_jsonl_preserves_strict_whitespace_and_record_byte_limits(self):
        path = self.artifacts / "record-boundaries.jsonl"
        for prefix in (b"\v", b"\f"):
            path.write_bytes(prefix + b'{"ok":true}\n')
            with self.subTest(prefix=prefix), self.assertRaises(SystemExit):
                policy.load_jsonl(path)
        path.write_bytes(b' \t{"ok":true}\r\n')
        self.assertEqual(policy.load_jsonl(path), [{"ok": True}])
        raw = b'{"value":"' + b'x' * (policy.MAX_RECORD_BYTES - 13) + b'"}\n'
        self.assertEqual(len(raw), policy.MAX_RECORD_BYTES)
        path.write_bytes(raw)
        self.assertEqual(len(policy.load_jsonl(path)), 1)
        path.write_bytes(b' ' + raw)
        with self.assertRaises(SystemExit):
            policy.load_jsonl(path)

    def test_cli_reports_malformed_shapes_without_private_text_or_tracebacks(self):
        path = self.artifacts / "invalid-expected.json"
        for invalid in ([], {"schema_version": policy.EXPECTED_SCHEMA, "policy_id": policy.POLICY_ID,
                             "status": "PRIVATE-CANARY"}):
            path.write_text(json.dumps(invalid), encoding="utf-8")
            result = subprocess.run([sys.executable, str(policy.ROOT / "scripts/validate_eval_policy.py"),
                                     "--expected", str(path)], capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, b"")
            self.assertTrue(result.stderr.startswith(b"validation failed: "))
            self.assertNotIn(b"PRIVATE-CANARY", result.stderr)
            self.assertNotIn(b"Traceback", result.stderr)

    def test_case_and_family_ids_are_not_echoed_in_cli_diagnostics(self):
        path = self.artifacts / "private-case-ids.jsonl"
        for field in ("case_id", "family_id"):
            cases = copy.deepcopy(self.cases)
            cases[0][field] = cases[1][field] = "PRIVATE-CANARY"
            path.write_text("\n".join(json.dumps(x) for x in cases) + "\n", encoding="utf-8")
            result = subprocess.run([sys.executable, str(policy.ROOT / "scripts/validate_eval_policy.py"),
                                     "--cases", str(path)], capture_output=True, timeout=5)
            with self.subTest(field=field):
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, b"")
                self.assertTrue(result.stderr.startswith(b"validation failed: "))
                self.assertNotIn(b"PRIVATE-CANARY", result.stderr)
                self.assertNotIn(b"Traceback", result.stderr)

    def test_numbers_outside_finite_float_range_fail_without_overflowing(self):
        self.assertFalse(policy.json_number(10 ** 400))
        self.assertTrue(policy.json_number(300))


if __name__ == "__main__":
    unittest.main(verbosity=2)
