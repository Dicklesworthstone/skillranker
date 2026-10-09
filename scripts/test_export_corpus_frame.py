#!/usr/bin/env python3
"""Offline handoff controls using constructed evidence, never promotion labels."""
import copy
import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import export_corpus_frame as bridge
from test_validate_corpus import base_case, base_manifest


class ExportTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="sr-corpus-frame-test-"))
        self.output = self.root / "output"
        self.output.mkdir(mode=0o700)
        self.cases = [base_case("positive", "training"),
                      base_case("negative", "validation", "no_match_advisory"),
                      base_case("failed", "holdout"), base_case("unknown", "holdout")]
        self.cases[1]["acceptable_additional_invocations_y"] = []
        self.cases[-1].update(unjudgeable=True, unjudgeable_reason="synthetic missing context")
        self.manifest = base_manifest({"training": ["positive"], "validation": ["negative"],
                                       "holdout": ["failed", "unknown"]})
        self.records = []
        for index, case in enumerate(self.cases):
            decision = ("ranked", "abstain", "unavailable", "ranked")[index]
            record = {
                "schema_version": 1,
                "key": {"frame_id": "synthetic-frame", "family_id": case["family_id"],
                        "case_id": case["case_id"], "replicate": 0, "policy_id": "baseline"},
                "split": bridge.SPLITS[case["split"]],
                "roster_skills": [s["skill_id"] for s in case["roster"]["skills"]],
                "decision": decision, "suggested_skills": ["s_good"] if decision == "ranked" else [],
                "relevance_abstention": decision == "abstain",
                "operational_failure": decision == "unavailable",
            }
            self.records.append({"schema": bridge.WRAPPER_SCHEMA, "case_digest": bridge.canonical_digest(case),
                                 "roster_manifest_digest": case["roster"]["manifest_digest"], "record": record})

    def inputs(self):
        manifest, cases, recorded = (self.root / name for name in ("manifest.json", "cases.jsonl", "recorded.jsonl"))
        manifest.write_text(json.dumps(self.manifest))
        cases.write_bytes(b"".join(bridge.encode(c) for c in self.cases))
        recorded.write_bytes(b"".join(bridge.encode(r) for r in self.records))
        return manifest, cases, recorded

    def run_export(self):
        return bridge.export(*self.inputs(), self.output, 7)

    def reject(self, needle):
        with self.assertRaisesRegex(bridge.Reject, needle):
            self.run_export()
        self.assertEqual(list(self.output.iterdir()), [])

    def test_success_preserves_frozen_order_failures_and_unjudged(self):
        self.records.reverse()
        receipt = self.run_export()
        rows = [json.loads(line) for line in (self.output / "dataset.jsonl").read_text().splitlines()]
        labels = [json.loads(line) for line in (self.output / "labels.jsonl").read_text().splitlines()]
        self.assertEqual([r["key"]["case_id"] for r in rows], ["positive", "negative", "failed", "unknown"])
        self.assertEqual(rows[0]["split"], "train")
        self.assertEqual(rows[2]["decision"], "unavailable")
        self.assertEqual([l["case_id"] for l in labels], ["positive", "negative", "failed"])
        self.assertEqual(labels[1]["acceptable_skills"], [])
        self.assertTrue(labels[1]["no_skill_needed"])
        self.assertEqual((receipt["labels"], receipt["unjudged"], receipt["operational_failures"]), (3, 1, 1))
        self.assertEqual(receipt["quality_gate"], "not-established")
        for name, digest in receipt["files"].items():
            self.assertEqual(hashlib.sha256((self.output / name).read_bytes()).hexdigest(), digest)
            self.assertEqual((self.output / name).stat().st_mode & 0o777, 0o600)

    def test_missing_primary_cannot_turn_into_a_failure(self):
        self.records.pop()
        self.reject("missing recorded primary")

    def test_duplicate_record_is_rejected(self):
        self.records.append(copy.deepcopy(self.records[0]))
        self.reject("duplicate recorded")

    def test_diagnostic_sibling_cannot_enlarge_the_primary_frame(self):
        variant = copy.deepcopy(self.cases[0])
        variant.update(case_id="variant", primary_family_case=False)
        self.cases.append(variant)
        frozen = self.manifest["splits"]["training"]
        frozen["case_ids"].append("variant")
        frozen["digest"] = bridge.canonical_digest({"split": "training", "case_ids": frozen["case_ids"]})
        receipt = self.run_export()
        self.assertEqual(receipt["primary_cases"], 4)
        self.assertEqual(receipt["corpus_validation"]["variant_cases"], 1)

    def test_variant_record_is_refused(self):
        self.records[0]["record"]["key"]["case_id"] = "variant"
        self.reject("not a frozen primary")

    def test_case_content_changes_are_bound_even_if_membership_is_unchanged(self):
        self.cases[0]["consent_reference"] = "consent://changed"
        self.reject("case content digest")

    def test_roster_binding_is_required(self):
        self.records[0]["roster_manifest_digest"] = "1" * 64
        self.reject("roster manifest digest")

    def test_roster_membership_is_exact(self):
        self.records[0]["record"]["roster_skills"].pop()
        self.reject("record roster differs")

    def test_wrong_family_split_and_replicate_are_refused(self):
        original = copy.deepcopy(self.records)
        for field, value, needle in (("family_id", "wrong", "family identity"),
                                     ("replicate", 1, "replicate zero")):
            self.records = copy.deepcopy(original)
            self.records[0]["record"]["key"][field] = value
            self.reject(needle)
        self.records = original
        self.records[0]["record"]["split"] = "holdout"
        self.reject("split mismatch")

    def test_mixed_policies_are_refused(self):
        self.records[0]["record"]["key"]["policy_id"] = "tuned"
        self.reject("mixed policy")

    def test_explicit_and_hidden_failure_are_refused(self):
        self.records[0]["record"]["decision"] = "explicit"
        self.reject("refuses explicit")
        self.records[0]["record"]["decision"] = "ranked"
        self.records[2]["record"]["operational_failure"] = False
        self.reject("failure must retain")

    def test_foreign_suggestions_and_duplicate_roster_are_refused(self):
        self.records[0]["record"]["suggested_skills"] = ["foreign"]
        self.reject("absent from roster")
        self.records[0]["record"]["suggested_skills"] = ["s_good"]
        self.records[0]["record"]["roster_skills"].append("s_good")
        self.reject("duplicate IDs")

    def test_manual_only_selector_mistake_is_not_erased(self):
        self.records[0]["record"]["suggested_skills"] = ["s_manual"]
        self.run_export()
        row = json.loads((self.output / "dataset.jsonl").read_text().splitlines()[0])
        self.assertEqual(row["suggested_skills"], ["s_manual"])

    def test_missing_label_time_does_not_invent_todays_time(self):
        self.cases[0]["adjudications"][0].pop("labeled_at_unix_ms")
        self.records[0]["case_digest"] = bridge.canonical_digest(self.cases[0])
        self.reject("adjudication time")

    def test_validator_independence_requirement_remains_enforced(self):
        self.manifest["selector_identity"] = "adj-1"
        self.reject("selector identity cannot")

    def test_invalid_probabilities_unknown_fields_and_boolean_version_are_refused(self):
        original = copy.deepcopy(self.records)
        for field, value, needle in (("fits", {"s_good": 2}, "finite probability"),
                                    ("path", "/tmp/do-not-resolve", "unknown fields"),
                                    ("schema_version", True, "schema mismatch")):
            self.records = copy.deepcopy(original)
            self.records[0]["record"][field] = value
            self.reject(needle)

    def test_existing_and_raced_in_targets_are_never_replaced(self):
        target = self.output / "labels.jsonl"
        target.write_bytes(b"preserve me")
        with self.assertRaisesRegex(bridge.Reject, "already exists"):
            self.run_export()
        self.assertEqual(target.read_bytes(), b"preserve me")
        other = self.root / "race-output"
        other.mkdir(mode=0o700)
        real_link = os.link

        def race(source, destination, **kwargs):
            if Path(destination).name == "labels.jsonl":
                fd = os.open(destination, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600,
                             dir_fd=kwargs["dst_dir_fd"])
                with os.fdopen(fd, "wb") as handle:
                    handle.write(b"concurrent owner")
            return real_link(source, destination, **kwargs)

        with patch.object(bridge.os, "link", side_effect=race), self.assertRaises(FileExistsError):
            bridge.export(*self.inputs(), other, 7)
        self.assertEqual((other / "labels.jsonl").read_bytes(), b"concurrent owner")
        self.assertFalse((other / "receipt.json").exists())

    def test_verifier_accepts_intact_export_and_refuses_tampering(self):
        self.run_export()
        self.assertEqual(bridge.verify_export(self.output)["primary_cases"], 4)
        with (self.output / "dataset.jsonl").open("ab") as handle:
            handle.write(b"{}\n")
        with self.assertRaisesRegex(bridge.Reject, "digest mismatch"):
            bridge.verify_export(self.output)

    def test_verifier_cannot_follow_receipt_supplied_paths(self):
        self.run_export()
        receipt_path = self.output / "receipt.json"
        receipt = json.loads(receipt_path.read_bytes())
        receipt["files"] = {"../private-input": "0" * 64}
        receipt_path.write_bytes(bridge.encode(receipt))
        with self.assertRaisesRegex(bridge.Reject, "file set mismatch"):
            bridge.verify_export(self.output)

    def test_duplicate_keys_and_oversized_records_are_bounded(self):
        inputs = self.inputs()
        inputs[2].write_bytes(b'{"schema":1,"schema":2}\n')
        with self.assertRaisesRegex(bridge.Reject, "duplicate JSON"):
            bridge.export(*inputs, self.output, 7)
        inputs[2].write_bytes(b" " * (bridge.MAX_RECORD_BYTES + 1))
        with self.assertRaisesRegex(bridge.Reject, "byte bounds"):
            bridge.export(*inputs, self.output, 7)

    def test_recorded_blank_lines_cannot_hide_invalid_json_whitespace(self):
        inputs = self.inputs()
        original = inputs[2].read_bytes()
        for whitespace in (b"\x0b", b"\x0c"):
            with self.subTest(whitespace=whitespace):
                output = self.root / ("invalid-whitespace-" + whitespace.hex())
                output.mkdir(mode=0o700)
                inputs[2].write_bytes(whitespace + b"\n" + original)
                with self.assertRaisesRegex(bridge.Reject, "bounded UTF-8 JSON"):
                    bridge.export(*inputs, output, 7)
                self.assertEqual(list(output.iterdir()), [])
        inputs[2].write_bytes(b" \t\r\n" + original)
        receipt = bridge.export(*inputs, self.output, 7)
        self.assertEqual((receipt["primary_cases"], receipt["labels"]), (4, 3))

    def test_unused_corpus_metadata_still_requires_unicode_scalar_values(self):
        inputs = self.inputs()
        self.cases[0]["consent_reference"] = "private-canary\ud800"
        self.records[0]["case_digest"] = bridge.canonical_digest(self.cases[0])
        # Serialize escaped surrogates like an external producer. The export
        # encoder itself already rejects them and is not this test's boundary.
        inputs[1].write_text("".join(json.dumps(c) + "\n" for c in self.cases))
        inputs[2].write_text("".join(json.dumps(r) + "\n" for r in self.records))
        with self.assertRaisesRegex(bridge.Reject, "non-Unicode string") as rejected:
            bridge.export(*inputs, self.output, 7)
        self.assertNotIn("private-canary", str(rejected.exception))
        self.assertEqual(list(self.output.iterdir()), [])

    def test_unpaired_surrogate_cannot_publish_a_rust_unreadable_record(self):
        self.records[0]["record"]["prompt_summary"] = "\ud800"
        inputs = self.inputs_without_recorded_encoding()
        inputs[2].write_text("".join(json.dumps(r) + "\n" for r in self.records))
        with self.assertRaisesRegex(bridge.Reject, "non-Unicode"):
            bridge.export(*inputs, self.output, 7)
        self.assertEqual(list(self.output.iterdir()), [])

    def inputs_without_recorded_encoding(self):
        records = self.records
        self.records = []
        try:
            return self.inputs()
        finally:
            self.records = records


if __name__ == "__main__":
    unittest.main()
