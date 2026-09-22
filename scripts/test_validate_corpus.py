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
        manifest_path = self.tmp / "manifest.json"
        cases_path = self.tmp / "cases.jsonl"
        manifest_path.write_text(json.dumps(manifest))
        cases_path.write_text("".join(json.dumps(c) + "\n" for c in cases))
        return subprocess.run(
            [sys.executable, str(VALIDATOR), "--manifest", str(manifest_path), "--cases", str(cases_path)],
            capture_output=True,
            text=True,
        )

    def assert_fails_with(self, manifest, cases, needle):
        result = self.run_validator(manifest, cases)
        self.assertNotEqual(result.returncode, 0, f"expected failure for {needle!r}")
        self.assertIn(
            needle,
            result.stderr + result.stdout,
            f"failure for {needle!r} must name its cause: {result.stderr}",
        )

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
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate", result.stderr + result.stdout)


if __name__ == "__main__":
    unittest.main()
