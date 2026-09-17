"""Real-process and real-file checks; artifacts are intentionally retained."""

import copy
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import evidence as ev
import runner


def case(name, mode="pass", **overrides):
    return {"id": name, "mode": mode, "timeout_ms": 5000, "output_bytes": 8192,
            "expected_exit": 0, "expected_outcome": "ok", "assertions": ["behavior"],
            "require_fault": False, **overrides}


def suite(cases, timeout_ms=60_000):
    return {"schema_version": 2, "suite": "runner-test", "tier": "runner-mechanics",
            "timeout_ms": timeout_ms, "cases": cases}


class RunnerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.artifacts = Path(tempfile.mkdtemp(prefix="sr-runner-tests-"))
        cls.descendant_case = "descendant-" + uuid.uuid4().hex
        print(f"runner test artifacts: {cls.artifacts}", file=sys.stderr)
        cls.spec = suite([
            case("success"), case("refusal", "refuse", expected_exit=7,
                                  expected_outcome="refused", require_fault=True),
            case("isolation", "isolation"), case("secret", "secret"),
            case("badexit", "badexit"), case("signal", "signal"),
            # These are cleanup mechanics, not a product latency benchmark. Leave
            # time for the real child to start before testing its forced timeout.
            case("hang", "hang"), case(cls.descendant_case, "childhang"),
            case("flood", "flood", output_bytes=512), case("badjson", "badjson"),
            case("duplicate", "duplicate"), case("noresult", "noresult"),
            case("faultmiss", "faultmiss", require_fault=True), case("retry", "failthenpass")])
        cls.directory, cls.summary = runner.run(cls.spec, sys.executable, cls.artifacts)
        cls.events = [ev.decode(line) for line in (cls.directory / "events.jsonl").read_bytes().splitlines()]
        cls.results = {event["case"]: event for event in cls.events if event["kind"] == "case_result"}

    def mutated_report(self, change):
        directory = Path(tempfile.mkdtemp(prefix="mutated-", dir=self.artifacts))
        events = copy.deepcopy(self.events)
        change(events)
        for index, event in enumerate(events):
            event["seq"] = index
        (directory / "events.jsonl").write_bytes(b"".join(ev.encode(e) for e in events))
        (directory / "summary.json").write_bytes(ev.encode(ev.summarize(events[0], events, True)))
        return directory

    def test_positive_and_refusal_twins(self):
        self.assertEqual(self.events[0]["network"], "isolated", "Sandbox unavailable: host cannot certify this test")
        for name in ("success", "refusal", "isolation", "secret"):
            with self.subTest(case=name):
                self.assertEqual(self.results[name]["status"], "passed")
        self.assertEqual(self.results["refusal"]["observed"],
                         {"effects": 0, "fault_reached": True, "outcome": "refused"})
        self.assertEqual(self.summary["runner_status"], "failed")
        self.assertTrue(self.summary["identity_stable"])

    def test_failure_classifications(self):
        expected = {"badexit": "exit", "signal": "signal", "hang": "timeout", self.descendant_case: "timeout",
                    "flood": "output-limit", "badjson": "protocol", "duplicate": "protocol",
                    "noresult": "protocol", "faultmiss": "fault", "retry": "protocol"}
        for name, reason in expected.items():
            with self.subTest(case=name):
                self.assertEqual(self.results[name]["status"], "failed")
                self.assertEqual(self.results[name]["reason"], reason)
        self.assertEqual(self.summary["counts"]["started"], len(self.spec["cases"]))
        self.assertEqual(self.summary["counts"]["failed"], len(expected))
        self.assertEqual(self.summary["counts"]["incomplete"], 0)

    def test_private_sanitized_artifacts(self):
        self.assertEqual(self.directory.stat().st_mode & 0o777, 0o700)
        for name in ("events.jsonl", "summary.json"):
            path = self.directory / name
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            content = path.read_bytes()
            for secret in (b"SYNTHETIC_SECRET_DO_NOT_LOG", b"credential=value", b"\x1b"):
                self.assertNotIn(secret, content)
            self.assertLessEqual(len(content), ev.MAX_DOCUMENT)
        self.assertGreater(self.results["secret"]["stderr_bytes"], 0)

    def test_report_reconciliation(self):
        self.assertEqual(ev.validate(self.directory, self.spec), self.summary)
        incompatible = copy.deepcopy(self.events[0]["identity"])
        incompatible["binary_sha256"] = "0" * 64
        with self.assertRaises(ev.InvalidEvidence):
            ev.validate(self.directory, self.spec, incompatible)

    def test_missing_terminal_is_incomplete(self):
        directory = self.mutated_report(lambda events: events.pop())
        self.assertEqual(ev.validate(directory, self.spec)["counts"]["incomplete"], 1)
        # Other deliberate failures still dominate; absence cannot create a pass.
        self.assertNotEqual(ev.validate(directory, self.spec)["runner_status"], "passed")

    def test_duplicate_terminal_rejected(self):
        directory = self.mutated_report(lambda events: events.append(copy.deepcopy(events[-1])))
        with self.assertRaises(ev.InvalidEvidence):
            ev.validate(directory, self.spec)

    def test_recomputed_summary_cannot_hide_wrong_expectations(self):
        def corrupt(events):
            result = next(event for event in events if event.get("case") == "faultmiss"
                          and event["kind"] == "case_result")
            result.update(status="passed", reason="matched")
        with self.assertRaises(ev.InvalidEvidence):
            ev.validate(self.mutated_report(corrupt), self.spec)

    def test_recomputed_summary_cannot_hide_effects(self):
        def corrupt(events):
            result = next(event for event in events if event.get("case") == "refusal"
                          and event["kind"] == "case_result")
            result["observed"]["effects"] = 1
        with self.assertRaises(ev.InvalidEvidence):
            ev.validate(self.mutated_report(corrupt), self.spec)

    def test_truncation_counts_schema_and_secret_fields_rejected(self):
        for field, value in (("counts", {}), ("schema_version", 99), ("unexpected", "private")):
            directory = self.mutated_report(lambda _: None)
            summary = ev.read_json(directory / "summary.json")
            summary[field] = value
            (directory / "summary.json").write_bytes(ev.encode(summary))
            with self.subTest(field=field), self.assertRaises(ev.InvalidEvidence):
                ev.validate(directory, self.spec)
        directory = self.mutated_report(lambda _: None)
        path = directory / "events.jsonl"
        path.write_bytes(path.read_bytes()[:-1])
        with self.assertRaises(ev.InvalidEvidence):
            ev.validate(directory, self.spec)

    def test_partial_and_empty_selection(self):
        spec = suite([case("one"), case("two")])
        _, summary = runner.run(spec, sys.executable, self.artifacts, ["one"])
        self.assertEqual(summary["runner_status"], "partial")
        self.assertEqual(summary["counts"]["skipped"], 1)
        for selection in ([], ["one", "one"], ["missing"]):
            with self.subTest(selection=selection), self.assertRaises(ev.InvalidEvidence):
                runner.run(spec, sys.executable, self.artifacts, selection)

    def test_aggregate_deadline(self):
        spec = suite([case("hang", "hang", timeout_ms=250), case("after", timeout_ms=250)], 250)
        directory, summary = runner.run(spec, sys.executable, self.artifacts)
        self.assertEqual(summary["counts"]["failed"], 1)
        self.assertEqual(summary["counts"]["blocked"], 1)
        self.assertEqual(summary["counts"]["started"], 1)
        self.assertEqual(ev.validate(directory, spec), summary)

    def test_descendants_are_gone_after_timeout(self):
        # Absence alone would pass if slow sandbox setup never reached the fork.
        self.assertTrue(self.results[self.descendant_case]["fault_observed"])
        self.assertTrue(self.results["hang"]["fault_observed"])
        for path in Path("/proc").glob("[0-9]*/cmdline"):
            try:
                content = path.read_bytes()
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
            self.assertNotIn(("sr-runner-owned-descendant-" + self.descendant_case).encode("ascii"), content)

    def test_term_preserves_failed_terminal_and_blocks_remaining(self):
        parent = Path(tempfile.mkdtemp(prefix="interrupt-", dir=self.artifacts))
        spec = suite([case("waiting", "hang", timeout_ms=15_000), case("after")])
        code = ("import sys; sys.path.insert(0, sys.argv[1]); import runner, json; "
                "runner.run(json.loads(sys.argv[2]), sys.executable, sys.argv[3])")
        process = subprocess.Popen([sys.executable, "-I", "-B", "-c", code,
                                    str(runner.HERE), json.dumps(spec), str(parent)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            found = None
            while time.monotonic() < deadline and process.poll() is None:
                for path in parent.glob("sr-e2e-*/events.jsonl"):
                    if b'"kind":"case_start"' in path.read_bytes():
                        found = path.parent
                        break
                if found:
                    break
                time.sleep(0.02)
            self.assertIsNotNone(found, "child never reached its declared case")
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=5)
            self.assertEqual(process.returncode, 0, stderr)
            self.assertEqual(stdout, b"")
            summary = ev.validate(found, spec)
            self.assertEqual(summary["runner_status"], "failed")
            self.assertEqual(summary["counts"]["failed"], 1)
            self.assertEqual(summary["counts"]["blocked"], 1)
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)

    def test_unavailable_interpreter_cannot_pass(self):
        # A real binary that cannot execute the fixture protocol fails the sandbox
        # capability probe. This tests fail-closed behavior without a fake sandbox.
        spec = suite([case("one")])
        directory, summary = runner.run(spec, "/usr/bin/false", self.artifacts)
        self.assertEqual(summary["runner_status"], "blocked")
        self.assertEqual(summary["counts"]["started"], 0)
        self.assertEqual(ev.validate(directory, spec), summary)

    def test_cleanup_retains_leader_identity_until_group_signal(self):
        process = subprocess.Popen(["/usr/bin/true"], start_new_session=True, env=runner.SAFE_ENV)
        try:
            deadline = time.monotonic() + 2
            while not runner.exited_without_reaping(process) and time.monotonic() < deadline:
                time.sleep(0.005)
            self.assertTrue(runner.exited_without_reaping(process))
            self.assertIsNone(process.returncode)
            os.kill(process.pid, 0)  # The unreaped PID remains reserved to our child.
            runner.kill_owned(process)
            self.assertEqual(process.returncode, 0)
            runner.kill_owned(process)  # Idempotent; never signal a reused PGID.
        finally:
            if process.returncode is None:
                runner.kill_owned(process)

    def test_inherited_secrets_and_cli_errors_not_echoed(self):
        # Synthetic canaries only; no operator environment or actual credential is read.
        env = {**runner.SAFE_ENV, "TYPESAFE_API_KEY": "SYNTHETIC_INHERITED_SECRET",
               "HTTPS_PROXY": "https://SYNTHETIC_PROXY_SECRET", "PYTHONPATH": "/secret/imports"}
        command = [str(runner.HERE / "run.sh"), "--suite", "runner-smoke", "--artifacts", str(self.artifacts)]
        result = subprocess.run(command, env=env, capture_output=True, timeout=30, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        directory = self.artifacts / json.loads(result.stdout)["run"]
        for content in (result.stdout, result.stderr, (directory / "events.jsonl").read_bytes(),
                        (directory / "summary.json").read_bytes()):
            self.assertNotIn(b"SYNTHETIC_INHERITED_SECRET", content)
            self.assertNotIn(b"SYNTHETIC_PROXY_SECRET", content)
        result = subprocess.run(command + ["--SYNTHETIC_ARGUMENT_SECRET"], capture_output=True,
                                timeout=10, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn(b"SYNTHETIC_ARGUMENT_SECRET", result.stdout + result.stderr)

    def test_source_digest_includes_untracked_code(self):
        root = Path(tempfile.mkdtemp(prefix="source-", dir=self.artifacts))
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
            (root / name).write_text("synthetic")
        for name in ("src", "tests", "scripts"):
            (root / name).mkdir()
        value = runner.source_digest(root)
        (root / ".env").write_text("SYNTHETIC_SECRET")
        self.assertEqual(value, runner.source_digest(root))
        (root / "src/untracked.rs").write_text("// synthetic input\n")
        self.assertNotEqual(value, runner.source_digest(root))
        (root / "src/link.rs").symlink_to(root / ".env")
        with self.assertRaises(ev.InvalidEvidence):
            runner.source_digest(root)

    def test_input_bytes_sealed_and_identity_checked(self):
        path = self.artifacts / "sealed-source"
        path.write_bytes(b"original")
        descriptor = runner.open_input(path, ev.sha(b"original"))
        try:
            path.write_bytes(b"replacement")
            self.assertEqual(os.read(descriptor, 100), b"original")
            with self.assertRaises(PermissionError):
                os.write(descriptor, b"changed")
        finally:
            os.close(descriptor)
        with self.assertRaises(ev.InvalidEvidence):
            runner.open_input(path, ev.sha(b"original"))
        fifo = self.artifacts / "fifo"
        os.mkfifo(fifo)
        with self.assertRaises(ev.InvalidEvidence):
            runner.open_input(fifo, ev.sha(b""))
        with self.assertRaises(ev.InvalidEvidence):
            ev.read_json(fifo)

    def test_scalar_confusion_and_missing_event_fields_rejected(self):
        for field in ("seq", "kind", "case"):
            events = copy.deepcopy(self.events)
            events[1].pop(field)
            directory = Path(tempfile.mkdtemp(prefix="missing-", dir=self.artifacts))
            (directory / "events.jsonl").write_bytes(b"".join(ev.encode(e) for e in events))
            (directory / "summary.json").write_bytes(ev.encode(self.summary))
            with self.subTest(field=field), self.assertRaises(ev.InvalidEvidence):
                ev.validate(directory, self.spec)
        directory = self.mutated_report(lambda _: None)
        summary = ev.read_json(directory / "summary.json")
        summary["counts"]["incomplete"] = False
        (directory / "summary.json").write_bytes(ev.encode(summary))
        with self.assertRaises(ev.InvalidEvidence):
            ev.validate(directory, self.spec)

    def test_strict_json_and_manifest(self):
        for data in (b'{"x":1,"x":2}', b'{"x":NaN}', b"[" * 2000, b"\xff"):
            with self.subTest(data=data[:20]), self.assertRaises(ev.InvalidEvidence):
                ev.decode(data)
        for change in (lambda spec: spec.update(cases=[]),
                       lambda spec: spec.update(schema_version=True),
                       lambda spec: spec.update(tier="live"),
                       lambda spec: spec["cases"][0].update(timeout_ms=True),
                       lambda spec: spec["cases"].append(copy.deepcopy(spec["cases"][0]))):
            spec = suite([case("one")])
            change(spec)
            with self.assertRaises(ev.InvalidEvidence):
                ev.manifest(spec)


class EvidenceBoundaryTests(unittest.TestCase):
    def test_exponent_overflow_is_rejected_with_finite_twins(self):
        for data in (b"1e999", b"-1e999", b'{"nested":[1e999]}'):
            with self.subTest(data=data), self.assertRaises(ev.InvalidEvidence):
                ev.decode(data)
        self.assertEqual(ev.decode(b'{"values":[1e308,1e-308,0.0]}'),
                         {"values": [1e308, 1e-308, 0.0]})

    def test_cleanup_completion_obeys_deadline(self):
        command = ["/usr/bin/printf", "{}\n"]
        records, code, reason, _, _ = runner.invoke(command, (), time.monotonic() + 5, 8192, 5000)
        self.assertEqual((records, code, reason), ([{}], 0, None))
        cleanup = runner.kill_owned
        entered_in_time = []
        deadline = time.monotonic() + 1

        def delayed_cleanup(process):
            cleanup(process)
            entered_in_time.append(time.monotonic() < deadline)
            time.sleep(max(0, deadline - time.monotonic()) + 0.02)

        with patch.object(runner, "kill_owned", side_effect=delayed_cleanup):
            records, code, reason, elapsed, _ = runner.invoke(command, (), deadline, 8192, 1000)
        self.assertEqual(entered_in_time, [True], "process must finish before injecting late cleanup")
        self.assertEqual((records, code, reason), ([{}], 0, "timeout"))
        self.assertGreaterEqual(elapsed, 1000)

    def test_cancellation_during_cleanup_cannot_return_success(self):
        cleanup = runner.kill_owned
        old_stop = runner.STOP

        def cancel_during_cleanup(process):
            cleanup(process)
            runner.STOP = True

        try:
            runner.STOP = False
            with patch.object(runner, "kill_owned", side_effect=cancel_during_cleanup):
                records, code, reason, _, _ = runner.invoke(
                    ["/usr/bin/printf", "{}\n"], (), time.monotonic() + 5, 8192, 5000)
            self.assertEqual((records, code, reason), ([{}], 0, "interrupted"))
        finally:
            runner.STOP = old_stop

    def test_decoding_completion_obeys_deadline(self):
        decode = ev.decode
        entered_in_time = []
        deadline = time.monotonic() + 1

        def delayed_decode(data):
            result = decode(data)
            entered_in_time.append(time.monotonic() < deadline)
            time.sleep(max(0, deadline - time.monotonic()) + 0.02)
            return result

        with patch.object(ev, "decode", side_effect=delayed_decode):
            records, code, reason, elapsed, _ = runner.invoke(
                ["/usr/bin/printf", "{}\n"], (), deadline, 8192, 1000)
        self.assertEqual(entered_in_time, [True], "child must finish before injecting slow decode")
        self.assertEqual((records, code, reason), ([{}], 0, "timeout"))
        self.assertGreaterEqual(elapsed, 1000)

    def test_retry_fixture_really_emits_failure_before_success(self):
        result = subprocess.run([sys.executable, "-I", "-B", str(runner.FIXTURE),
                                 "failthenpass", "retry", "run-fixture"],
                                env=runner.SAFE_ENV, capture_output=True, timeout=5, check=True)
        records = [ev.decode(line) for line in result.stdout.splitlines()]
        self.assertEqual([record["passed"] for record in records if record["kind"] == "assertion"], [False, True])
        self.assertEqual(ev.classify(case("retry", "failthenpass"), records, 0, run_id="run-fixture")[0], "protocol")


if __name__ == "__main__":
    unittest.main(verbosity=2)
