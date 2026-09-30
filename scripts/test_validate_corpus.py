#!/usr/bin/env python3
"""Regression tests for scripts/validate_corpus.py (sr-uv2v).

Each adversarial case targets one way a corpus could look complete and be
worthless; each has an honest counterpart that must pass. Synthetic fixtures
are retained in a reported temporary directory for diagnosis. No real
sessions, no provider calls.
"""
from __future__ import annotations

import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VALIDATOR = ROOT / "scripts" / "validate_corpus.py"

RUBRIC = "relevance-corpus-contract.v1"
SELECTOR = "sr@a82abbb"


def digest(value) -> str:
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def base_manifest(case_ids_by_split, minimums=None, fraction=0.0):
    splits = {}
    for name, ids in case_ids_by_split.items():
        splits[name] = {"case_ids": ids, "digest": digest({"split": name, "case_ids": ids})}
    for name in ("training", "validation", "holdout"):
        splits.setdefault(name, {"case_ids": [], "digest": digest({"split": name, "case_ids": []})})
    return {
        "schema": "skillranker.corpus_manifest.v1",
        "rubric_version": RUBRIC,
        "selector_identity": SELECTOR,
        "declared_population": {
            "description": "pilot frame: synthetic unit-test cases",
            "narrowed": True,
            "justification": "validator regression fixture, not a promotion cohort",
        },
        "strata_minimums": minimums
        or {"positive": 0, "no_match": 0, "near_miss": 0, "overflow_positive": 0},
        "adjudicators": [
            {"id": "adj-1", "relationship": "reviewer", "forbidden_sessions": []},
            {"id": "adj-2", "relationship": "reviewer", "forbidden_sessions": ["case-pos"]},
        ],
        "double_judgment_fraction": fraction,
        "splits": splits,
    }


def base_case(case_id, split, kind="positive_advisory", family=None):
    return {
        "schema_version": "skillranker.adjudicated_case.v1",
        "case_id": case_id,
        "family_id": family or f"fam-{case_id}",
        "split": split,
        "primary_family_case": True,
        "case_kind": kind,
        "constructed": True,
        "overflow": False,
        "consent_reference": "consent://synthetic",
        "roster": {
            "manifest_digest": "0" * 64,
            "skills": [
                {"skill_id": "s_good", "invocation_name": "good", "usage_kind": "workflow"},
                {"skill_id": "s_tempt", "invocation_name": "tempt", "usage_kind": "workflow"},
                {
                    "skill_id": "s_manual",
                    "invocation_name": "manual",
                    "usage_kind": "reference",
                    "manual_only": True,
                },
            ],
        },
        "acceptable_additional_invocations_y": ["s_good"],
        "near_miss_skill_ids": [],
        "adjudications": [
            {
                "adjudicator": "adj-1",
                "blindness_attested": True,
                "rubric_version": RUBRIC,
                "labeled_at_unix_ms": 1,
            }
        ],
    }


def valid_corpus():
    cases = [
        base_case("case-pos", "training"),
        base_case("case-nomatch", "training", "no_match_advisory"),
        base_case("case-nearmiss", "validation", "near_miss_advisory"),
    ]
    cases[1]["acceptable_additional_invocations_y"] = []
    cases[2]["near_miss_skill_ids"] = ["s_tempt"]
    manifest = base_manifest(
        {"training": ["case-pos", "case-nomatch"], "validation": ["case-nearmiss"]},
        minimums={"positive": 1, "no_match": 1, "near_miss": 1, "overflow_positive": 0},
    )
    return manifest, cases


class CorpusValidatorTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="sr-corpus-validator-"))
        print(f"synthetic corpus fixtures retained at {cls.tmp}")

    def run_validator(self, manifest, cases):
        return self.run_raw_validator(
            json.dumps(manifest).encode(),
            "".join(json.dumps(c) + "\n" for c in cases).encode(),
        )

    def run_raw_validator(self, manifest, cases):
        manifest_path = self.tmp / "manifest.json"
        cases_path = self.tmp / "cases.jsonl"
        manifest_path.write_bytes(manifest)
        cases_path.write_bytes(cases)
        return subprocess.run(
            [sys.executable, str(VALIDATOR), "--manifest", str(manifest_path), "--cases", str(cases_path)],
            capture_output=True,
            text=True,
            timeout=10,
        )

    def assert_fails_with(self, manifest, cases, needle):
        result = self.run_validator(manifest, cases)
        self.assertNotEqual(result.returncode, 0, f"expected failure for {needle!r}")
        self.assertIn(
            needle,
            result.stderr + result.stdout,
            f"failure for {needle!r} must name its cause: {result.stderr}",
        )

    def append_frozen_case(self, manifest, cases, case):
        cases.append(case)
        split = case["split"]
        frozen = manifest["splits"][split]
        frozen["case_ids"].append(case["case_id"])
        frozen["digest"] = digest({"split": split, "case_ids": frozen["case_ids"]})

    def assert_quota_requires_independent_primary(self, stratum, kind, overflow=False):
        manifest, cases = valid_corpus()
        manifest["strata_minimums"][stratum] = 1 if overflow else 2
        for index in range(12):
            sibling = base_case(f"variant-{index}", "training", kind, cases[0]["family_id"])
            sibling["primary_family_case"] = False
            sibling["overflow"] = overflow
            if kind == "no_match_advisory":
                sibling["acceptable_additional_invocations_y"] = []
            if kind == "near_miss_advisory":
                sibling["near_miss_skill_ids"] = ["s_tempt"]
            self.append_frozen_case(manifest, cases, sibling)
        self.assert_fails_with(manifest, cases, f"stratum {stratum}")

        independent = base_case("independent-primary", "holdout", kind)
        independent["overflow"] = overflow
        if kind == "no_match_advisory":
            independent["acceptable_additional_invocations_y"] = []
        if kind == "near_miss_advisory":
            independent["near_miss_skill_ids"] = ["s_tempt"]
        self.append_frozen_case(manifest, cases, independent)
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["strata"][stratum], 1 if overflow else 2)
        self.assertEqual(report["variant_strata"][stratum], 12)
        self.assertEqual(report["primary_cases"], 4)
        self.assertEqual(report["variant_cases"], 12)
        self.assertEqual(report["primary_case_ids_by_split"]["holdout"], ["independent-primary"])

    def test_positive_siblings_cannot_supply_primary_quota(self):
        self.assert_quota_requires_independent_primary("positive", "positive_advisory")

    def test_no_match_siblings_cannot_supply_primary_quota(self):
        self.assert_quota_requires_independent_primary("no_match", "no_match_advisory")

    def test_near_miss_siblings_cannot_supply_primary_quota(self):
        self.assert_quota_requires_independent_primary("near_miss", "near_miss_advisory")

    def test_overflow_siblings_cannot_supply_primary_quota(self):
        self.assert_quota_requires_independent_primary("overflow_positive", "positive_advisory", True)

    def test_mixed_variants_preserve_frozen_primary_report_cohort(self):
        manifest, cases = valid_corpus()
        for index in range(300):
            kind = ("positive_advisory", "no_match_advisory", "near_miss_advisory")[index % 3]
            sibling = base_case(f"mixed-{index}", "training", kind, cases[0]["family_id"])
            sibling["primary_family_case"] = False
            sibling["overflow"] = True
            if kind == "no_match_advisory":
                sibling["acceptable_additional_invocations_y"] = []
            if kind == "near_miss_advisory":
                sibling["near_miss_skill_ids"] = ["s_tempt"]
            self.append_frozen_case(manifest, cases, sibling)
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["cases"], 303)
        self.assertEqual(report["primary_cases"], 3)
        self.assertEqual(report["families"], 3)
        self.assertEqual(report["variant_cases"], 300)
        self.assertEqual(report["strata"], {"positive": 1, "no_match": 1, "near_miss": 1, "overflow_positive": 0})
        self.assertEqual(report["variant_strata"], {"positive": 100, "no_match": 100, "near_miss": 100, "overflow_positive": 200})
        expected_ids = {
            "training": ["case-pos", "case-nomatch"],
            "validation": ["case-nearmiss"],
            "holdout": [],
        }
        self.assertEqual(report["primary_case_ids_by_split"], expected_ids)
        self.assertEqual(report["primary_dataset_digest"], digest(expected_ids))
        # Report consumers select this frozen cohort, never all retained records.
        primary_ids = {case_id for ids in report["primary_case_ids_by_split"].values() for case_id in ids}
        cohort = [case for case in cases if case["case_id"] in primary_ids]
        self.assertEqual(len(cohort), 3)
        self.assertTrue(all(case["primary_family_case"] for case in cohort))
        self.assertEqual(report["scope"], "diagnostic")
        manifest["strata_minimums"]["positive"] = 150
        self.assert_fails_with(manifest, cases, "stratum positive has 1 primary cases")

    def test_variants_retain_all_trust_and_label_validation(self):
        for field, value, message in (
            ("consent_reference", "", "consent_reference"),
            ("acceptable_additional_invocations_y", ["s_ghost"], "not in the roster"),
            ("adjudications", [], "adjudication record"),
            ("near_miss_skill_ids", ["s_good"], "both acceptable and near-miss"),
        ):
            with self.subTest(field=field):
                manifest, cases = valid_corpus()
                sibling = base_case("bad-variant", "training", family=cases[0]["family_id"])
                sibling["primary_family_case"] = False
                sibling[field] = value
                self.append_frozen_case(manifest, cases, sibling)
                self.assert_fails_with(manifest, cases, message)

    def test_family_without_primary_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["primary_family_case"] = False
        self.assert_fails_with(manifest, cases, "has no primary case")

    def test_review_fraction_keeps_all_record_population_explicit(self):
        manifest, cases = valid_corpus()
        sibling = base_case("reviewed-variant", "validation", family=cases[2]["family_id"])
        sibling["primary_family_case"] = False
        second = {"adjudicator": "adj-2", "blindness_attested": True,
                  "rubric_version": RUBRIC, "labeled_at_unix_ms": 3}
        cases[2]["adjudications"].append(copy.deepcopy(second))
        self.append_frozen_case(manifest, cases, sibling)
        manifest["double_judgment_fraction"] = 0.5
        self.assert_fails_with(manifest, cases, "only 1 of 4 cases are double-judged")
        sibling["adjudications"].append(second)
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["double_judged"], 2)
        self.assertEqual(report["primary_double_judged"], 1)
        self.assertEqual(report["double_judgment_population"], "all-records")
        self.assertEqual(report["double_judgment_denominator"], 4)

    def test_broad_validation_receipt_does_not_claim_promotion(self):
        manifest, cases = valid_corpus()
        manifest["declared_population"]["narrowed"] = False
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["scope"], "corpus-validation-only")

    def test_deep_json_is_rejected_before_decoding(self):
        for target in ("manifest", "case"):
            for nesting in (64, 65):
                with self.subTest(target=target, nesting=nesting):
                    manifest, cases = valid_corpus()
                    value = 0
                    # The surrounding manifest/case object contributes one level.
                    for _ in range(nesting - 1):
                        value = [value]
                    (manifest if target == "manifest" else cases[0])["extra"] = value
                    result = self.run_validator(manifest, cases)
                    if nesting == 64:
                        self.assertEqual(result.returncode, 0, result.stderr)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("nesting bound", result.stderr)
                        self.assertNotIn("Traceback", result.stderr)

    def test_exponent_overflow_and_huge_integer_refuse_without_traceback(self):
        manifest, cases = valid_corpus()
        for number in (b"1e9999", b"-1e9999", b"9" * 5000):
            with self.subTest(number=number[:20]):
                raw = json.dumps(cases[0]).encode()[:-1] + b',"extra":' + number + b'}\n'
                result = self.run_raw_validator(json.dumps(manifest).encode(), raw)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("corpus validation failed", result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    def test_invalid_utf8_refuses_without_echoing_input(self):
        manifest, _ = valid_corpus()
        result = self.run_raw_validator(
            json.dumps(manifest).encode(), b'{"extra":"private\xff"}\n'
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("UTF-8", result.stderr)
        self.assertNotIn("private", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_record_byte_limit_counts_blank_lines_and_accepts_exact_limit(self):
        manifest, cases = valid_corpus()
        rest = "".join(json.dumps(c) + "\n" for c in cases[1:]).encode()
        first = json.dumps(cases[0]).encode()
        limit = 1024 * 1024
        exact = first + b" " * (limit - len(first) - 1) + b"\n"
        result = self.run_raw_validator(json.dumps(manifest).encode(), exact + rest)
        self.assertEqual(result.returncode, 0, result.stderr)
        for oversized in (exact[:-1] + b" \n", b" " * limit + b"\n"):
            result = self.run_raw_validator(json.dumps(manifest).encode(), oversized + rest)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("byte bound", result.stderr)
            self.assertNotIn("Traceback", result.stderr)

    def test_file_growth_after_metadata_check_stays_bounded(self):
        # Deterministically append to actual regular files after fstat, at the
        # context-manager boundary. The original open/read operations still run.
        manifest, cases = valid_corpus()
        self.assertEqual(self.run_validator(manifest, cases).returncode, 0)
        script = '''
import importlib.util, sys
from contextlib import contextmanager
from pathlib import Path
spec = importlib.util.spec_from_file_location("validator", sys.argv[1])
v = importlib.util.module_from_spec(spec)
spec.loader.exec_module(v)
manifest, cases, target = Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4]
original = v.regular_file
bound = (manifest if target == "manifest" else cases).stat().st_size
if target == "cases":
    v.MAX_CASES_BYTES = bound
@contextmanager
def grow(path, max_bytes, label):
    with original(path, max_bytes, label) as handle:
        if label == target:
            with path.open("ab") as writer:
                writer.write(b" " * (max_bytes + 1) if target == "manifest" else b"\\n")
        yield handle
v.regular_file = grow
try:
    if target == "manifest":
        v.load_json(manifest, bound, "manifest")
    else:
        v.run(manifest, cases)
except v.Reject as error:
    assert "byte bound" in str(error), str(error)
else:
    raise AssertionError("growing file escaped its byte bound")
'''
        for target in ("manifest", "cases"):
            self.assertEqual(self.run_validator(manifest, cases).returncode, 0)
            result = subprocess.run(
                [sys.executable, "-c", script, str(VALIDATOR),
                 str(self.tmp / "manifest.json"), str(self.tmp / "cases.jsonl"), target],
                capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_nonfinite_constants_are_not_json_evidence(self):
        for value in (float("nan"), float("inf"), -float("inf")):
            manifest, cases = valid_corpus()
            cases[0]["extra"] = value
            self.assert_fails_with(manifest, cases, "non-finite")

    def test_malformed_identifier_lists_refuse_without_traceback(self):
        for field in ("acceptable_additional_invocations_y", "near_miss_skill_ids"):
            manifest, cases = valid_corpus()
            cases[0][field] = [{}]
            result = self.run_validator(manifest, cases)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("non-empty strings", result.stderr)
            self.assertNotIn("Traceback", result.stderr)
        manifest, cases = valid_corpus()
        manifest["adjudicators"][0]["forbidden_sessions"] = [{}]
        result = self.run_validator(manifest, cases)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-empty strings", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_string_delimiters_do_not_count_as_nesting(self):
        manifest, cases = valid_corpus()
        cases[0]["extra"] = ('[{' + chr(34) + chr(92)) * 100
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_valid_corpus_passes(self):
        manifest, cases = valid_corpus()
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["cases"], 3)
        self.assertEqual(report["strata"]["positive"], 1)
        self.assertEqual(report["strata"]["near_miss"], 1)

    def test_case_in_two_splits_fails(self):
        manifest, cases = valid_corpus()
        manifest["splits"]["validation"]["case_ids"].append("case-pos")
        manifest["splits"]["validation"]["digest"] = digest(
            {"split": "validation", "case_ids": manifest["splits"]["validation"]["case_ids"]}
        )
        self.assert_fails_with(manifest, cases, "more than one split")

    def test_family_spanning_splits_fails(self):
        manifest, cases = valid_corpus()
        cases[2]["family_id"] = cases[0]["family_id"]
        cases[2]["primary_family_case"] = False
        self.assert_fails_with(manifest, cases, "spans splits")

    def test_stratum_below_declared_minimum_fails(self):
        manifest, cases = valid_corpus()
        manifest["strata_minimums"]["positive"] = 5
        self.assert_fails_with(manifest, cases, "below the declared minimum")

    def test_missing_adjudication_record_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["adjudications"] = []
        self.assert_fails_with(manifest, cases, "missing its adjudication record")

    def test_selector_as_adjudicator_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["adjudications"][0]["adjudicator"] = "adj-2"
        manifest["adjudicators"][1]["id"] = SELECTOR
        self.assert_fails_with(manifest, cases, "selector")

    def test_forbidden_session_adjudicator_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["adjudications"][0]["adjudicator"] = "adj-2"
        self.assert_fails_with(manifest, cases, "forbidden")

    def test_missing_blindness_attestation_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["adjudications"][0]["blindness_attested"] = False
        self.assert_fails_with(manifest, cases, "blindness")

    def test_near_miss_with_empty_acceptable_set_fails(self):
        manifest, cases = valid_corpus()
        cases[2]["acceptable_additional_invocations_y"] = []
        self.assert_fails_with(manifest, cases, "no-match")

    def test_label_absent_from_roster_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["acceptable_additional_invocations_y"] = ["s_ghost"]
        self.assert_fails_with(manifest, cases, "not in the roster")

    def test_manual_only_label_fails(self):
        manifest, cases = valid_corpus()
        cases[0]["acceptable_additional_invocations_y"] = ["s_manual"]
        self.assert_fails_with(manifest, cases, "manual-only")

    def test_two_primary_cases_in_family_fails(self):
        manifest, cases = valid_corpus()
        cases[1]["family_id"] = cases[0]["family_id"]
        self.assert_fails_with(manifest, cases, "more than one primary")

    def test_case_added_after_freeze_fails(self):
        manifest, cases = valid_corpus()
        extra = base_case("case-late", "training")
        cases.append(extra)
        self.assert_fails_with(manifest, cases, "not in any frozen split")

    def test_frozen_case_missing_from_corpus_fails(self):
        manifest, cases = valid_corpus()
        cases.pop(2)
        self.assert_fails_with(manifest, cases, "missing from the corpus")

    def test_tampered_split_digest_fails(self):
        manifest, cases = valid_corpus()
        manifest["splits"]["training"]["digest"] = "f" * 64
        self.assert_fails_with(manifest, cases, "digest")

    def test_double_judgment_fraction_enforced(self):
        manifest, cases = valid_corpus()
        manifest["double_judgment_fraction"] = 0.5
        self.assert_fails_with(manifest, cases, "double-judged")
        cases[1]["adjudications"].append(
            {
                "adjudicator": "adj-2",
                "blindness_attested": True,
                "rubric_version": RUBRIC,
                "labeled_at_unix_ms": 2,
            }
        )
        cases[2]["adjudications"].append(
            {
                "adjudicator": "adj-2",
                "blindness_attested": True,
                "rubric_version": RUBRIC,
                "labeled_at_unix_ms": 3,
            }
        )
        result = self.run_validator(manifest, cases)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_consent_reference_fails(self):
        manifest, cases = valid_corpus()
        del cases[0]["consent_reference"]
        self.assert_fails_with(manifest, cases, "consent_reference")

    def test_duplicate_json_key_fails(self):
        manifest, cases = valid_corpus()
        manifest_path = self.tmp / "manifest.json"
        cases_path = self.tmp / "cases.jsonl"
        manifest_path.write_text(json.dumps(manifest))
        record = json.dumps(cases[0])[:-1] + ',"case_id":"dup"}'
        cases_path.write_text(record + "\n")
        result = subprocess.run(
            [sys.executable, str(VALIDATOR), "--manifest", str(manifest_path), "--cases", str(cases_path)],
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate", result.stderr + result.stdout)


if __name__ == "__main__":
    unittest.main()
