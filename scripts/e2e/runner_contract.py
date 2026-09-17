"""Independent expected outcomes for the real runner; never a product gate."""

import copy
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import evidence as ev
import runner

HERE = Path(__file__).resolve().parent

# Freeze this oracle independently of evidence.classify. Deliberately failed
# inner runs must remain failed even when this outer certification succeeds.
SCENARIOS = (
    ("success", "pass", "passed", "matched"),
    ("expected-refusal", "refuse", "passed", "matched"),
    ("nonzero", "badexit", "failed", "exit"),
    ("signal", "signal", "failed", "signal"),
    ("timeout", "hang", "failed", "timeout"),
    ("descendant-cleanup", "childhang", "failed", "timeout"),
    ("pipe-budget", "flood", "failed", "output-limit"),
    ("malformed", "badjson", "failed", "protocol"),
    ("truncated", "truncated", "failed", "protocol"),
    ("duplicate-record", "duplicate", "failed", "protocol"),
    ("missing-record", "noresult", "failed", "protocol"),
    ("foreign-case", "foreigncase", "failed", "protocol"),
    ("unreached-fault", "faultmiss", "failed", "fault"),
    ("failed-then-passed", "failthenpass", "failed", "protocol"),
    ("hidden-pipeline-failure", "pipeline", "failed", "assertion"),
    ("unexpected-effects", "effects", "failed", "effects"),
    ("false-assertion", "falseassert", "failed", "assertion"),
    ("wrong-outcome", "wrongoutcome", "failed", "outcome"),
    ("private-diagnostics", "secret", "passed", "matched"),
)


def specification():
    cases = []
    for case_id, mode, _, _ in SCENARIOS:
        cases.append({"id": case_id, "mode": mode, "timeout_ms": 5000,
                      "output_bytes": 512 if mode == "flood" else 8192,
                      "expected_exit": 7 if mode == "refuse" else 0,
                      "expected_outcome": "refused" if mode == "refuse" else "ok",
                      "assertions": ["behavior"], "require_fault": mode in {"faultmiss", "refuse"}})
    return {"schema_version": 2, "suite": "contract-children", "tier": "runner-mechanics",
            "timeout_ms": 60_000, "cases": cases}


MUTATIONS = (
    "schema-mismatch", "duplicate-terminal", "missing-terminal", "missing-summary",
    "truncated-events", "wrong-counts", "unknown-assertion", "wrong-case", "foreign-event",
    "unsafe-extra-field", "false-effects", "false-fault", "retry-erasure",
    "witness-missing", "witness-foreign-run", "witness-foreign-case", "witness-replayed",
)
CHECKS = tuple(case[0] for case in SCENARIOS) + (
    "inner-failure-retained", "honest-complete-twin", "private-artifacts", "sanitized-artifacts",
    "empty-selection", "duplicate-selection", "partial-selection", "artifact-reservation",
    "expected-binary-mismatch", "expected-source-mismatch", "expected-run-mismatch",
    "foreign-child-record", "fault-witness-current", "fault-witness-old", "fault-witness-absent",
    "signal-interruption", "secret-argument", "secret-environment",
) + MUTATIONS
CANARIES = (b"SYNTHETIC_CONTRACT_SECRET", b"RAW_PROVIDER_ERROR", b"credential=value", b"\x1b",
            b"SYNTHETIC_ARGUMENT_SECRET", b"SYNTHETIC_ENV_SECRET")
FIXTURE = HERE / "fixtures/contract_child.py"


def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
                       allow_nan=False) + "\n").encode("ascii")


def write_private(path, data):
    ev.require(len(data) <= ev.MAX_DOCUMENT, "certificate-budget")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def read_events(directory):
    data = ev.read_bytes(directory / "events.jsonl")
    records = [ev.decode(line) for line in data.splitlines()]
    ev.require(data == b"".join(canonical(record) for record in records), "certificate-canonical")
    return records


def rejected(operation, *args, **kwargs):
    try:
        operation(*args, **kwargs)
    except ev.InvalidEvidence:
        return True
    return False


def owned_descendant_gone(run_id):
    """Return a boolean only; never retain or display arbitrary host argv."""
    marker = ("sr-contract-descendant-" + run_id).encode("ascii")
    deadline = time.monotonic() + 2
    for index, path in enumerate(Path("/proc").glob("[0-9]*/cmdline")):
        if index >= 10_000 or time.monotonic() >= deadline:
            return False
        try:
            with path.open("rb") as stream:
                data = stream.read(8192)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        if marker in data:
            return False
    return True


def candidate(parent, original, mutation):
    """Make a bounded invalid candidate without altering the retained original."""
    events = copy.deepcopy(read_events(original))
    summary = ev.read_json(original / "summary.json")
    results = [event for event in events if event["kind"] == "case_result"]
    if mutation == "schema-mismatch":
        events[0]["schema_version"] = 1
    elif mutation == "duplicate-terminal":
        events.append(copy.deepcopy(results[-1]))
    elif mutation == "missing-terminal":
        events.remove(results[-1])
    elif mutation == "wrong-counts":
        summary["counts"]["passed"] += 1
    elif mutation == "unknown-assertion":
        results[0]["assertions"][0]["id"] = "unknown-assertion"
    elif mutation == "wrong-case":
        results[0]["case"] = "unplanned-case"
    elif mutation == "foreign-event":
        results[0]["run_id"] = "run-foreign"
    elif mutation == "unsafe-extra-field":
        results[0]["untrusted-payload"] = 1
    elif mutation == "false-effects":
        results[0]["observed"]["effects"] = 1
    elif mutation == "false-fault":
        results[1]["observed"]["fault_reached"] = False
    elif mutation == "retry-erasure":
        results[0]["attempt"] = 2
    elif mutation.startswith("witness-"):
        event = next(item for item in results if item["case"] == "timeout")
        if event["fault_witness"] is None:
            event["fault_witness"] = {"run_id": events[0]["run_id"], "case": "timeout", "id": "timeout-entered"}
        if mutation == "witness-missing":
            event["fault_witness"] = None
        elif mutation in {"witness-foreign-run", "witness-replayed"}:
            event["fault_witness"]["run_id"] = "run-previous" if mutation == "witness-replayed" else "run-foreign"
        else:
            event["fault_witness"]["case"] = "foreign-case"
    for index, event in enumerate(events):
        event["seq"] = index
    data = b"".join(canonical(event) for event in events)
    summary["events_sha256"] = hashlib.sha256(data).hexdigest()
    if mutation == "truncated-events":
        data = data[:-1]
    directory = Path(tempfile.mkdtemp(prefix="candidate-", dir=parent))
    write_private(directory / "events.jsonl", data)
    if mutation != "missing-summary":
        write_private(directory / "summary.json", canonical(summary))
    return directory


def interrupted_run(parent, binary):
    directory = Path(tempfile.mkdtemp(prefix="interrupted-", dir=parent))
    process = subprocess.Popen([sys.executable, "-I", "-B", str(__file__), "interrupt-child",
                                str(directory), str(binary)], env=runner.SAFE_ENV,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    found = None
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not runner.exited_without_reaping(process):
            for path in directory.glob("sr-e2e-*/events.jsonl"):
                if b'"kind":"case_start"' in ev.read_bytes(path):
                    found = path.parent
                    break
            if found is not None:
                break
            time.sleep(0.01)
        if found is None:
            return False
        # Force death before finalization, not a graceful exception in a fake driver.
        runner.kill_owned(process)
        stdout, stderr = process.communicate(timeout=5)
        spec = interruption_specification()
        return (process.returncode == -signal.SIGKILL and not (found / "summary.json").exists()
                and rejected(lambda: ev.validate(found, spec))
                and not any(canary in stdout + stderr for canary in CANARIES))
    finally:
        if process.returncode is None:
            runner.kill_owned(process)
        process.communicate(timeout=5)


def interruption_specification():
    spec = specification()
    spec["cases"] = [dict(spec["cases"][4], timeout_ms=15_000)]
    return spec


def positive_specification():
    spec = specification()
    return dict(spec, cases=[spec["cases"][0], spec["cases"][1], spec["cases"][-1]])


def evidence_reference(role, directory, run_id):
    return {"role": role, "directory": directory.name, "run_id": run_id,
            "events_sha256": hashlib.sha256(ev.read_bytes(directory / "events.jsonl")).hexdigest(),
            "summary_sha256": hashlib.sha256(ev.read_bytes(directory / "summary.json")).hexdigest()}


def referenced_directory(parent, reference, pattern):
    ev.require(type(reference["directory"]) is str and re.fullmatch(pattern, reference["directory"]), "certificate-path")
    path = Path(parent) / reference["directory"]
    ev.require(path.is_dir() and not path.is_symlink(), "certificate-directory")
    return path


def validate_certificate(directory, expected_identity=None):
    events = read_events(Path(directory))
    ev.require(len(events) == len(CHECKS) + 1, "certificate-cardinality")
    ev.require(all(type(event) is dict for event in events), "certificate-events")
    header = events[0]
    ev.keys(header, {"schema_version", "engine_schema_version", "kind", "seq", "run_id", "suite",
                     "tier", "checks", "identity", "driver_sha256", "specification_sha256", "inner_runs", "candidates"})
    ev.require(header["schema_version"] == 1 and type(header["schema_version"]) is int
               and header["engine_schema_version"] == 2 and type(header["engine_schema_version"]) is int
               and header["kind"] == "header" and header["suite"] == "runner-contract"
               and header["tier"] == "runner-mechanics" and header["checks"] == list(CHECKS), "certificate-header")
    ev.identifier(header["run_id"])
    ev.validate_identity(header["identity"])
    ev.require(header["driver_sha256"] == runner.digest_file(__file__)
               and header["specification_sha256"] == hashlib.sha256(canonical(specification())).hexdigest(),
               "certificate-identity")
    if expected_identity is not None:
        ev.require(header["identity"] == expected_identity, "certificate-source")
    ev.require(type(header["inner_runs"]) is list and len(header["inner_runs"]) in {1, 3}, "certificate-references")
    actual_runs = {}
    inner_events = {}
    for reference, role in zip(header["inner_runs"], ("adversarial", "positive", "partial"), strict=False):
        ev.keys(reference, {"role", "directory", "run_id", "events_sha256", "summary_sha256"})
        ev.require(reference["role"] == role, "certificate-role")
        path = referenced_directory(directory, reference, r"sr-e2e-[a-z0-9_]{8}")
        ev.require(hashlib.sha256(ev.read_bytes(path / "events.jsonl")).hexdigest() == reference["events_sha256"], "certificate-inner-hash")
        ev.require(hashlib.sha256(ev.read_bytes(path / "summary.json")).hexdigest() == reference["summary_sha256"], "certificate-inner-summary")
        spec = specification() if role == "adversarial" else positive_specification()
        report = ev.validate(path, spec, header["identity"], expected_run_id=reference["run_id"])
        actual_runs[role] = (reference, report)
        inner_events[role] = read_events(path)
    unavailable = actual_runs["adversarial"][1]["runner_status"] == "blocked"
    ev.require(len(actual_runs) == (1 if unavailable else 3), "certificate-incomplete-runs")
    receipts = {}
    if not unavailable:
        results = {event["case"]: event for event in inner_events["adversarial"] if event["kind"] == "case_result"}
        for name, mode, expected_status, expected_reason in SCENARIOS:
            result = results.get(name)
            receipts[name] = (result is not None and result["status"] == expected_status
                              and result["reason"] == expected_reason)
            if mode == "childhang":
                # The live process-absence check is not replayable. Its prerequisite
                # receipt must still prove this invocation actually started a child.
                receipts[name] = receipts[name] and result["fault_observed"]
        adversarial = actual_runs["adversarial"][1]
        receipts["inner-failure-retained"] = (adversarial["runner_status"] == "failed"
            and adversarial["counts"]["failed"] == 16 and adversarial["counts"]["passed"] == 3)
        positive = actual_runs["positive"][1]
        receipts["honest-complete-twin"] = positive["runner_status"] == "passed" and positive["counts"]["passed"] == 3
        partial = actual_runs["partial"][1]
        receipts["partial-selection"] = (partial["runner_status"] == "partial" and partial["counts"]["skipped"] == 2
            and inner_events["partial"][0]["selected"] == ["success"])
        receipts["fault-witness-current"] = results.get("timeout", {}).get("fault_observed", False)
    ev.require(type(header["candidates"]) is list and len(header["candidates"]) == (0 if unavailable else len(MUTATIONS)), "certificate-candidates")
    candidate_rejections = {}
    for reference, mutation in zip(header["candidates"], MUTATIONS, strict=False):
        ev.keys(reference, {"mutation", "directory", "events_sha256", "summary_sha256"})
        ev.require(reference["mutation"] == mutation, "certificate-mutation")
        path = referenced_directory(directory, reference, r"candidate-[a-z0-9_]{8}")
        ev.require(hashlib.sha256(ev.read_bytes(path / "events.jsonl")).hexdigest() == reference["events_sha256"], "certificate-candidate-hash")
        if mutation == "missing-summary":
            ev.require(reference["summary_sha256"] is None and not (path / "summary.json").exists(), "certificate-missing-summary")
        else:
            ev.require(hashlib.sha256(ev.read_bytes(path / "summary.json")).hexdigest() == reference["summary_sha256"], "certificate-candidate-summary")
        original = actual_runs["adversarial" if mutation.startswith("witness-") else "positive"][0]
        spec = specification() if mutation.startswith("witness-") else positive_specification()
        candidate_rejections[mutation] = rejected(ev.validate, path, spec, header["identity"], expected_run_id=original["run_id"])
    for index, event in enumerate(events):
        ev.require(type(event.get("seq")) is int and event["seq"] == index, "certificate-sequence")
        if index == 0:
            continue
        ev.keys(event, {"seq", "kind", "run_id", "case", "status", "expected", "actual", "elapsed_ms"})
        ev.require(event["kind"] == "assertion" and event["run_id"] == header["run_id"]
                   and event["case"] == CHECKS[index - 1] and event["expected"] == "satisfied"
                   and event["actual"] in ("satisfied", "violated", "unavailable")
                   and event["status"] == {"satisfied": "passed", "violated": "failed", "unavailable": "blocked"}[event["actual"]],
                   "certificate-case")
        ev.integer(event["elapsed_ms"], 0, 120_000)
        if event["case"] in candidate_rejections:
            ev.require((event["status"] == "passed") == candidate_rejections[event["case"]], "false-negative-certificate")
        if event["status"] == "passed" and event["case"] in receipts:
            ev.require(receipts[event["case"]], "certificate-inner-assertion")
        ev.require((event["status"] == "blocked") == unavailable, "certificate-availability")
    summary = ev.read_json(Path(directory) / "summary.json")
    ev.keys(summary, {"schema_version", "run_id", "suite", "runner_status", "product_gate", "identity_stable",
                      "counts", "events_sha256"})
    ev.require(type(summary["identity_stable"]) is bool and type(summary["schema_version"]) is int, "certificate-stability")
    counts = {"planned": len(CHECKS), "passed": 0, "failed": 0, "blocked": 0}
    for event in events[1:]:
        counts[event["status"]] += 1
    ev.keys(summary["counts"], counts)
    for value in summary["counts"].values():
        ev.integer(value, 0, len(CHECKS))
    status = ("failed" if counts["failed"] or not summary["identity_stable"]
              else "blocked" if counts["blocked"] else "passed")
    expected = {"schema_version": 1, "run_id": header["run_id"], "suite": "runner-contract",
                "runner_status": status, "product_gate": "not-applicable",
                "identity_stable": summary["identity_stable"], "counts": counts,
                "events_sha256": hashlib.sha256(b"".join(canonical(e) for e in events)).hexdigest()}
    ev.require(summary == expected, "certificate-reconciliation")
    if summary["runner_status"] == "passed":
        ev.require(not unavailable and actual_runs["adversarial"][1]["runner_status"] == "failed"
                   and actual_runs["positive"][1]["runner_status"] == "passed"
                   and actual_runs["partial"][1]["runner_status"] == "partial", "certificate-inner-outcomes")
    return summary


def run_contract(binary, artifacts):
    binary = Path(binary).resolve(strict=True)
    parent = Path(tempfile.mkdtemp(prefix="sr-contract-", dir=artifacts))
    before = runner.identity(binary, FIXTURE)
    driver_hash = runner.digest_file(__file__)
    run_id = "contract-" + uuid.uuid4().hex
    outcomes = {}
    observed_times = {}
    checkpoint = time.monotonic()

    def check(name, condition, elapsed=None):
        nonlocal checkpoint
        now = time.monotonic()
        ev.require(name in CHECKS and name not in outcomes, "duplicate-contract-check")
        outcomes[name] = "satisfied" if condition else "violated"
        observed_times[name] = max(0, int((now - checkpoint) * 1000)) if elapsed is None else elapsed
        checkpoint = now

    spec = specification()
    inner_id = "run-" + uuid.uuid4().hex
    started = time.monotonic()
    inner, inner_summary = runner.run(spec, binary, parent, fixture=FIXTURE, run_id=inner_id)
    events = read_events(inner)
    references = [evidence_reference("adversarial", inner, inner_id)]
    candidates = []
    available = events[0]["network"] == "isolated"
    if available:
        results = {event["case"]: event for event in events if event["kind"] == "case_result"}
        for name, mode, status, reason in SCENARIOS:
            result = results.get(name)
            matched = result is not None and result["status"] == status and result["reason"] == reason
            if mode == "childhang":
                matched = matched and result["fault_observed"] and owned_descendant_gone(inner_id)
            check(name, matched,
                  0 if result is None else result["elapsed_ms"])
        check("inner-failure-retained", inner_summary["runner_status"] == "failed"
              and inner_summary["counts"]["failed"] == 16 and inner_summary["counts"]["passed"] == 3)
        positive_spec = positive_specification()
        positive_id = "run-" + uuid.uuid4().hex
        positive, positive_summary = runner.run(positive_spec, binary, parent, fixture=FIXTURE, run_id=positive_id)
        references.append(evidence_reference("positive", positive, positive_id))
        check("honest-complete-twin", positive_summary["runner_status"] == "passed"
              and ev.validate(positive, positive_spec, before, expected_run_id=positive_id)["counts"]["passed"] == 3)
        check("empty-selection", rejected(lambda: runner.run(spec, binary, parent, [], fixture=FIXTURE)))
        check("duplicate-selection", rejected(lambda: runner.run(spec, binary, parent, ["success", "success"], fixture=FIXTURE)))
        partial_id = "run-" + uuid.uuid4().hex
        partial_directory, partial = runner.run(positive_spec, binary, parent, ["success"], fixture=FIXTURE, run_id=partial_id)
        references.append(evidence_reference("partial", partial_directory, partial_id))
        check("partial-selection", partial["runner_status"] == "partial" and partial["counts"]["skipped"] == 2)
        oversized = copy.deepcopy(spec)
        oversized["cases"] = [dict(spec["cases"][0], id=f"case-{i}", assertions=[f"assert-{j}" for j in range(128)])
                              for i in range(17)]
        check("artifact-reservation", rejected(lambda: runner.run(oversized, binary, parent, fixture=FIXTURE)))
        for field, name in (("binary_sha256", "expected-binary-mismatch"), ("source_sha256", "expected-source-mismatch")):
            mismatched = {**before, field: "0" * 64}
            check(name, rejected(ev.validate, positive, positive_spec, mismatched, expected_run_id=positive_id))
        check("expected-run-mismatch", rejected(lambda: ev.validate(positive, positive_spec, before, expected_run_id="run-previous")))
        records = [{"schema_version": 2, "run_id": "run-previous", "case": "success", "kind": "assertion", "id": "behavior", "passed": True},
                   {"schema_version": 2, "run_id": "run-previous", "case": "success", "kind": "result", "outcome": "ok", "effects": 0, "fault_reached": False}]
        check("foreign-child-record", ev.classify(spec["cases"][0], records, 0, run_id=inner_id)[0] == "protocol")
        witness = {"schema_version": 2, "run_id": inner_id, "case": "timeout", "kind": "fault", "id": "timeout-entered"}
        check("fault-witness-current", ev.fault_witness(spec["cases"][4], [witness], inner_id) is not None
              and results["timeout"]["fault_observed"])
        check("fault-witness-old", ev.fault_witness(spec["cases"][4], [witness], "run-next") is None)
        check("fault-witness-absent", ev.fault_witness(spec["cases"][4], [], inner_id) is None)
        check("signal-interruption", interrupted_run(parent, binary))
        cli = [str(HERE / "run.sh"), "--suite", "runner-smoke", "--artifacts", str(parent)]
        failure = subprocess.run([*cli, "--SYNTHETIC_ARGUMENT_SECRET"], env=runner.SAFE_ENV,
                                 capture_output=True, timeout=10, check=False)
        check("secret-argument", failure.returncode == 2 and not any(c in failure.stdout + failure.stderr for c in CANARIES))
        env = {**runner.SAFE_ENV, "TYPESAFE_API_KEY": "SYNTHETIC_ENV_SECRET", "HTTPS_PROXY": "SYNTHETIC_ENV_SECRET"}
        clean = subprocess.run(cli, env=env, capture_output=True, timeout=30, check=False)
        check("secret-environment", clean.returncode == 0 and not any(c in clean.stdout + clean.stderr for c in CANARIES))
        for mutation in MUTATIONS:
            witness_case = mutation.startswith("witness-")
            original, expected_spec, expected_id = ((inner, spec, inner_id) if witness_case
                                                    else (positive, positive_spec, positive_id))
            mutated = candidate(parent, original, mutation)
            candidates.append({"mutation": mutation, "directory": mutated.name,
                               "events_sha256": hashlib.sha256(ev.read_bytes(mutated / "events.jsonl")).hexdigest(),
                               "summary_sha256": None if mutation == "missing-summary" else
                               hashlib.sha256(ev.read_bytes(mutated / "summary.json")).hexdigest()})
            check(mutation, rejected(ev.validate, mutated, expected_spec, before, expected_run_id=expected_id))
        files = list(parent.rglob("*.json")) + list(parent.rglob("*.jsonl"))
        check("private-artifacts", parent.stat().st_mode & 0o777 == 0o700
              and all(p.stat().st_mode & 0o777 == 0o600 for p in files)
              and all(p.stat().st_mode & 0o777 == 0o700 for p in parent.rglob("*") if p.is_dir()))
        check("sanitized-artifacts", all(not any(c in ev.read_bytes(p) for c in CANARIES) for p in files))
    else:
        outcomes = dict.fromkeys(CHECKS, "unavailable")
    ev.require(set(outcomes) == set(CHECKS), "missing-contract-check")
    stable = before == runner.identity(binary, FIXTURE) and driver_hash == runner.digest_file(__file__)
    header = {"schema_version": 1, "engine_schema_version": 2, "kind": "header", "seq": 0, "run_id": run_id,
              "suite": "runner-contract", "tier": "runner-mechanics", "checks": list(CHECKS), "identity": before,
              "driver_sha256": driver_hash, "specification_sha256": hashlib.sha256(canonical(spec)).hexdigest(),
              "inner_runs": references, "candidates": candidates}
    records = [header]
    counts = {"planned": len(CHECKS), "passed": 0, "failed": 0, "blocked": 0}
    for index, name in enumerate(CHECKS, 1):
        actual = outcomes[name]
        status = {"satisfied": "passed", "violated": "failed", "unavailable": "blocked"}[actual]
        counts[status] += 1
        records.append({"seq": index, "kind": "assertion", "run_id": run_id, "case": name,
                        "status": status, "expected": "satisfied", "actual": actual,
                        "elapsed_ms": observed_times.get(name, 0)})
    ev.require(time.monotonic() - started < 120, "contract-deadline")
    data = b"".join(canonical(record) for record in records)
    status = "failed" if counts["failed"] or not stable else "blocked" if counts["blocked"] else "passed"
    summary = {"schema_version": 1, "run_id": run_id, "suite": "runner-contract", "runner_status": status,
               "product_gate": "not-applicable", "identity_stable": stable, "counts": counts,
               "events_sha256": hashlib.sha256(data).hexdigest()}
    write_private(parent / "events.jsonl", data)
    write_private(parent / "summary.json", canonical(summary))
    validate_certificate(parent, before)
    return parent, summary


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "interrupt-child":
        runner.run(interruption_specification(), sys.argv[3], sys.argv[2], fixture=FIXTURE)
    else:
        raise SystemExit(2)
