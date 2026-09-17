#!/usr/bin/env python3
"""Adversarial contract tests; fixtures are synthetic and confer no quality proof."""
import copy
import os
import subprocess
import sys
import unittest

import validate_eval_policy as policy


class EvaluationPolicyContract(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
