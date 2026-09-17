"""Certification tests invoke the real entrypoint; no product/live claims."""

import copy
import hashlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import evidence as ev
import runner
import runner_contract as contract


class ContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.parent = Path(tempfile.mkdtemp(prefix="sr-contract-tests-"))
        print(f"contract artifacts: {cls.parent}", file=sys.stderr)
        command = [str(runner.HERE / "run.sh"), "--suite", "runner-contract", "--artifacts", str(cls.parent)]
        cls.process = subprocess.run(command, env=runner.SAFE_ENV, capture_output=True,
                                     timeout=150, check=False)
        cls.console = ev.decode(cls.process.stdout)
        cls.directory = cls.parent / cls.console["run"]
        cls.events = contract.read_events(cls.directory)
        cls.summary = ev.read_json(cls.directory / "summary.json")

    def mutated(self, change):
        directory = Path(tempfile.mkdtemp(prefix="certificate-negative-", dir=self.parent))
        shutil.copytree(self.directory, directory, dirs_exist_ok=True)
        events, summary = copy.deepcopy(self.events), copy.deepcopy(self.summary)
        change(events, summary)
        for index, event in enumerate(events):
            event["seq"] = index
        data = b"".join(contract.canonical(event) for event in events)
        summary["events_sha256"] = hashlib.sha256(data).hexdigest()
        (directory / "events.jsonl").write_bytes(data)
        (directory / "summary.json").write_bytes(contract.canonical(summary))
        return directory

    def test_real_entrypoint_and_frozen_oracles_pass(self):
        self.assertEqual(self.process.returncode, 0, self.process.stderr)
        self.assertEqual(self.console["runner_status"], "passed")
        self.assertEqual(self.summary["counts"], {"planned": 54, "passed": 54, "failed": 0, "blocked": 0})
        self.assertEqual(self.summary["product_gate"], "not-applicable")
        self.assertTrue(self.summary["identity_stable"])
        self.assertEqual(contract.validate_certificate(self.directory, self.events[0]["identity"]), self.summary)

    def test_inner_failures_are_retained(self):
        reports = [ev.read_json(path) for path in self.directory.glob("sr-e2e-*/summary.json")]
        self.assertTrue(any(report["runner_status"] == "failed" and report["counts"]["failed"] == 16 for report in reports))
        self.assertTrue(any(report["runner_status"] == "passed" and report["counts"]["passed"] == 3 for report in reports))
        self.assertTrue(any(report["runner_status"] == "partial" for report in reports))

    def test_unsafe_values_never_reach_console_or_artifacts(self):
        for channel in (self.process.stdout, self.process.stderr):
            for canary in contract.CANARIES:
                self.assertNotIn(canary, channel)
        for path in self.directory.rglob("*"):
            if path.is_file():
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                data = ev.read_bytes(path)
                for canary in contract.CANARIES:
                    self.assertNotIn(canary, data)
            else:
                self.assertEqual(path.stat().st_mode & 0o777, 0o700)

    def test_duplicate_missing_unknown_or_reordered_checks_rejected(self):
        mutations = (
            lambda events, _: events.append(copy.deepcopy(events[-1])),
            lambda events, _: events.pop(),
            lambda events, _: events[1].update(case="invented-check"),
            lambda events, _: events.reverse(),
        )
        for change in mutations:
            with self.assertRaises(ev.InvalidEvidence):
                contract.validate_certificate(self.mutated(change))

    def test_failed_assertion_cannot_be_relabelled_complete(self):
        def change(events, summary):
            events[1].update(status="failed", actual="violated")
            summary["counts"].update(passed=53, failed=1)
            # Retain passed top-level status despite internally consistent counts.
        with self.assertRaises(ev.InvalidEvidence):
            contract.validate_certificate(self.mutated(change))

    def test_copied_matching_certificate_still_passes(self):
        directory = self.mutated(lambda _events, _summary: None)
        self.assertEqual(contract.validate_certificate(directory)["runner_status"], "passed")

    def test_truncated_or_noncanonical_certificate_is_rejected(self):
        for transform in (lambda data: data[:-1], lambda data: data.replace(b'"kind":', b'"kind": ', 1)):
            directory = self.mutated(lambda _events, _summary: None)
            path = directory / "events.jsonl"
            path.write_bytes(transform(path.read_bytes()))
            with self.assertRaises(ev.InvalidEvidence):
                contract.validate_certificate(directory)

    def test_outer_claim_requires_matching_inner_classification(self):
        directory = self.mutated(lambda _events, _summary: None)
        events = contract.read_events(directory)
        reference = events[0]["inner_runs"][0]
        inner = directory / reference["directory"]
        inner_events = contract.read_events(inner)
        result = next(event for event in inner_events if event.get("case") == "foreign-case"
                      and event["kind"] == "case_result")
        result.update(reason="signal", exit_code=-9)
        data = b"".join(contract.canonical(event) for event in inner_events)
        (inner / "events.jsonl").write_bytes(data)
        report = ev.read_json(inner / "summary.json")
        report["events_sha256"] = hashlib.sha256(data).hexdigest()
        (inner / "summary.json").write_bytes(contract.canonical(report))
        # This remains an honest failed engine report, but is the wrong reason
        # for the outer suite's independent foreign-case protocol oracle.
        self.assertEqual(ev.validate(inner, contract.specification())["runner_status"], "failed")
        reference.update(contract.evidence_reference("adversarial", inner, reference["run_id"]))
        data = b"".join(contract.canonical(event) for event in events)
        (directory / "events.jsonl").write_bytes(data)
        summary = ev.read_json(directory / "summary.json")
        summary["events_sha256"] = hashlib.sha256(data).hexdigest()
        (directory / "summary.json").write_bytes(contract.canonical(summary))
        with self.assertRaises(ev.InvalidEvidence):
            contract.validate_certificate(directory)

    def test_malformed_certificate_fields_fail_with_typed_error(self):
        changes = (
            lambda events, _: events[0].update(inner_runs={}),
            lambda events, _: events[0].update(candidates=[None] * 17),
            lambda events, _: events[0]["inner_runs"][0].update(directory=[]),
            lambda events, _: events[1].update(actual=[]),
            lambda events, _: events[1].update(elapsed_ms=True),
            lambda _events, summary: summary["counts"].update(passed=True),
        )
        for change in changes:
            with self.assertRaises(ev.InvalidEvidence):
                contract.validate_certificate(self.mutated(change))

    def test_unstable_expected_failure_cannot_certify_stable_source(self):
        directory = self.mutated(lambda _events, _summary: None)
        events = contract.read_events(directory)
        reference = events[0]["inner_runs"][0]
        inner = directory / reference["directory"]
        report = ev.read_json(inner / "summary.json")
        report["identity_stable"] = False
        (inner / "summary.json").write_bytes(contract.canonical(report))
        self.assertEqual(ev.validate(inner, contract.specification())["runner_status"], "failed")
        reference.update(contract.evidence_reference("adversarial", inner, reference["run_id"]))
        data = b"".join(contract.canonical(event) for event in events)
        (directory / "events.jsonl").write_bytes(data)
        summary = ev.read_json(directory / "summary.json")
        summary["events_sha256"] = hashlib.sha256(data).hexdigest()
        (directory / "summary.json").write_bytes(contract.canonical(summary))
        with self.assertRaisesRegex(ev.InvalidEvidence, "certificate-inner-stability"):
            contract.validate_certificate(directory)
        # Preserve an honest failed report instead of hiding the changed source.
        summary.update(identity_stable=False, runner_status="failed")
        (directory / "summary.json").write_bytes(contract.canonical(summary))
        self.assertEqual(contract.validate_certificate(directory)["runner_status"], "failed")

    def test_missing_or_replaced_inner_receipts_reject_certificate(self):
        mutations = (
            lambda events, _: events[0]["inner_runs"][0].update(directory="sr-e2e-missing0"),
            lambda events, _: events[0]["inner_runs"][0].update(events_sha256="0" * 64),
            lambda events, _: events[0]["inner_runs"][0].update(summary_sha256="0" * 64),
            lambda events, _: events[0]["candidates"][0].update(events_sha256="0" * 64),
        )
        for change in mutations:
            with self.assertRaises(ev.InvalidEvidence):
                contract.validate_certificate(self.mutated(change))

    def test_fail_closed_with_unavailable_or_changed_identity(self):
        for field, value in (("driver_sha256", "0" * 64), ("specification_sha256", "0" * 64)):
            with self.subTest(field=field), self.assertRaises(ev.InvalidEvidence):
                contract.validate_certificate(self.mutated(lambda events, _, field=field, value=value: events[0].update({field: value})))
        directory = self.mutated(lambda _, summary: summary.update(identity_stable=False))
        with self.assertRaises(ev.InvalidEvidence):
            contract.validate_certificate(directory)

    def test_child_records_require_current_run_identity(self):
        case = contract.specification()["cases"][0]
        records = [
            {"schema_version": 2, "run_id": "run-current", "case": "success", "kind": "assertion", "id": "behavior", "passed": True},
            {"schema_version": 2, "run_id": "run-current", "case": "success", "kind": "result", "outcome": "ok", "effects": 0, "fault_reached": False},
        ]
        self.assertEqual(ev.classify(case, records, 0, run_id="run-current")[0], "matched")
        self.assertEqual(ev.classify(case, records, 0, run_id="run-next")[0], "protocol")
        records[0]["schema_version"] = True
        self.assertEqual(ev.classify(case, records, 0, run_id="run-current")[0], "protocol")

    def test_fork_witness_replay_and_foreign_case_rejected(self):
        case = {**contract.specification()["cases"][4], "mode": "childhang"}
        witness = {"schema_version": 2, "run_id": "run-owned", "case": "timeout", "kind": "fault", "id": "descendant-started"}
        self.assertIsNotNone(ev.fault_witness(case, [witness], "run-owned"))
        self.assertIsNone(ev.fault_witness(case, [witness], "run-new"))
        self.assertIsNone(ev.fault_witness(case, [{**witness, "case": "foreign"}], "run-owned"))
        self.assertIsNone(ev.fault_witness(case, [], "run-owned"))

    def test_contract_does_not_allow_partial_cli_certification(self):
        command = [str(runner.HERE / "run.sh"), "--suite", "runner-contract", "--artifacts", str(self.parent),
                   "--case", "success"]
        result = subprocess.run(command, env=runner.SAFE_ENV, capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(ev.decode(result.stderr)["runner_status"], "incomplete")


if __name__ == "__main__":
    unittest.main(verbosity=2)
