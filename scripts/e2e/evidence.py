"""Strict, bounded runner evidence. This schema conveys no product quality gate."""

import hashlib
import json
import math
import os
import re
import stat
from pathlib import Path

VERSION = 2
MAX_DOCUMENT = 1024 * 1024
ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}\Z", re.ASCII)
DIGEST = re.compile(r"[0-9a-f]{64}\Z", re.ASCII)
MODES = frozenset({"pass", "refuse", "badexit", "signal", "hang", "flood",
                   "badjson", "duplicate", "noresult", "faultmiss", "secret",
                   "childhang", "failthenpass", "isolation", "pipeline", "truncated",
                   "foreigncase", "effects", "falseassert", "wrongoutcome"})
REASONS = frozenset({"matched", "exit", "protocol", "assertion", "outcome",
                     "effects", "fault", "timeout", "output-limit", "signal",
                     "sandbox-unavailable", "spawn", "aggregate-deadline",
                     "interrupted", "identity-changed"})


class InvalidEvidence(ValueError):
    """Messages are fixed codes; never interpolate an untrusted value."""


def require(condition, code="invalid-evidence"):
    if not condition:
        raise InvalidEvidence(code)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate-key")
        result[key] = value
    return result


def decode(data):
    require(len(data) <= MAX_DOCUMENT, "document-limit")

    def finite_float(value):
        number = float(value)
        require(math.isfinite(number), "nonfinite")
        return number

    try:
        return json.loads(data, object_pairs_hook=unique_object,
                          parse_float=finite_float,
                          parse_constant=lambda _: require(False, "nonfinite"))
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise InvalidEvidence("invalid-json") from exc


def read_bytes(path):
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, "rb") as stream:
            info = os.fstat(stream.fileno())
            require(stat.S_ISREG(info.st_mode) and info.st_size <= MAX_DOCUMENT, "document-file")
            data = stream.read(MAX_DOCUMENT + 1)
            require(len(data) <= MAX_DOCUMENT, "document-limit")
            return data
    except OSError:
        raise InvalidEvidence("document-io") from None


def read_json(path):
    return decode(read_bytes(path))


def encode(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"),
                       ensure_ascii=True, allow_nan=False) + "\n").encode("ascii")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def keys(value, expected):
    require(type(value) is dict and set(value) == set(expected), "fields")


def integer(value, low, high):
    require(type(value) is int and low <= value <= high, "integer")


def identifier(value):
    require(type(value) is str and ID.fullmatch(value) is not None, "identifier")


def identifiers(values):
    require(type(values) is list and 0 < len(values) <= 128, "identifiers")
    for value in values:
        identifier(value)
    require(len(set(values)) == len(values), "duplicate-id")


def manifest(value):
    keys(value, {"schema_version", "suite", "tier", "timeout_ms", "cases"})
    require(value["schema_version"] == VERSION and type(value["schema_version"]) is int,
            "schema")
    identifier(value["suite"])
    require(value["tier"] == "runner-mechanics", "unsupported-evidence-tier")
    integer(value["timeout_ms"], 1, 120_000)
    require(type(value["cases"]) is list and 0 < len(value["cases"]) <= 128, "cases")
    ids = []
    for case in value["cases"]:
        keys(case, {"id", "mode", "timeout_ms", "output_bytes", "expected_exit",
                    "expected_outcome", "assertions", "require_fault"})
        identifier(case["id"])
        require(type(case["mode"]) is str and case["mode"] in MODES, "mode")
        integer(case["timeout_ms"], 1, value["timeout_ms"])
        integer(case["output_bytes"], 256, 1024 * 1024)
        integer(case["expected_exit"], 0, 125)
        require(type(case["expected_outcome"]) is str and case["expected_outcome"] in {"ok", "refused"}, "outcome")
        identifiers(case["assertions"])
        require(type(case["require_fault"]) is bool, "fault")
        ids.append(case["id"])
    identifiers(ids)
    require(sum(len(case["assertions"]) for case in value["cases"]) <= 2048,
            "artifact-reservation")
    return value


def classify(case, records, exit_code, transport_reason=None, *, run_id):
    """Validate child claims against independent checked-in expectations."""
    if transport_reason:
        return transport_reason, [], None
    assertions = []
    try:
        require(len(records) == len(case["assertions"]) + 1, "protocol")
        for record, expected in zip(records[:-1], case["assertions"], strict=True):
            keys(record, {"schema_version", "run_id", "case", "kind", "id", "passed"})
            require(type(record["schema_version"]) is int and record["schema_version"] == VERSION
                    and record["run_id"] == run_id and record["case"] == case["id"] and record["kind"] == "assertion"
                    and record["id"] == expected and type(record["passed"]) is bool,
                    "protocol")
            assertions.append({"id": expected, "passed": record["passed"]})
        terminal = records[-1]
        keys(terminal, {"schema_version", "run_id", "case", "kind", "outcome", "effects", "fault_reached"})
        require(type(terminal["schema_version"]) is int and terminal["schema_version"] == VERSION
                and terminal["run_id"] == run_id and terminal["case"] == case["id"] and terminal["kind"] == "result"
                and terminal["outcome"] in {"ok", "refused"}
                and type(terminal["effects"]) is int
                and 0 <= terminal["effects"] <= 1_000_000
                and type(terminal["fault_reached"]) is bool, "protocol")
    except (InvalidEvidence, TypeError):
        return "protocol", [], None
    observed = {key: terminal[key] for key in ("outcome", "effects", "fault_reached")}
    if exit_code is None or exit_code < 0:
        return "signal", assertions, observed
    if exit_code != case["expected_exit"]:
        return "exit", assertions, observed
    if not all(item["passed"] for item in assertions):
        return "assertion", assertions, observed
    if terminal["outcome"] != case["expected_outcome"]:
        return "outcome", assertions, observed
    if terminal["effects"] != 0:
        return "effects", assertions, observed
    if case["require_fault"] and not terminal["fault_reached"]:
        return "fault", assertions, observed
    return "matched", assertions, observed


def expectations(case):
    return {key: case[key] for key in ("expected_exit", "expected_outcome", "assertions",
                                     "require_fault", "timeout_ms", "output_bytes")}


def fault_witness(case, records, run_id):
    fault = {"hang": "timeout-entered", "childhang": "descendant-started"}.get(case["mode"])
    matched = (fault is not None and len(records) == 1 and type(records[0]) is dict
            and type(records[0].get("schema_version")) is int
            and records == [{"schema_version": VERSION, "run_id": run_id,
                             "case": case["id"], "kind": "fault", "id": fault}])
    return {"run_id": run_id, "case": case["id"], "id": fault} if matched else None


def summarize(header, events, identity_stable):
    results = [e for e in events if e["kind"] == "case_result"]
    counts = {"planned": len(header["planned"]), "selected": len(header["selected"]),
              "started": sum(e["kind"] == "case_start" for e in events),
              "skipped": len(header["planned"]) - len(header["selected"])}
    for status in ("passed", "failed", "blocked"):
        counts[status] = sum(e["status"] == status for e in results)
    counts["incomplete"] = counts["selected"] - len(results)
    if counts["failed"] or not identity_stable:
        status = "failed"
    elif counts["incomplete"]:
        status = "incomplete"
    elif counts["blocked"]:
        status = "blocked"
    elif counts["skipped"]:
        status = "partial"
    else:
        status = "passed"
    return {"schema_version": VERSION, "run_id": header["run_id"], "suite": header["suite"], "runner_status": status,
            "product_gate": "not-applicable", "identity_stable": identity_stable,
            "counts": counts, "events_sha256": sha(b"".join(encode(e) for e in events))}


def validate_identity(identity):
    keys(identity, {"source_sha256", "binary_sha256", "runner_sha256", "fixture_sha256",
                    "lock_sha256", "toolchain_sha256", "git_commit", "platform", "architecture",
                    "python", "features", "binary_role"})
    for key in ("source_sha256", "binary_sha256", "runner_sha256", "fixture_sha256",
                "lock_sha256", "toolchain_sha256"):
        require(type(identity[key]) is str and DIGEST.fullmatch(identity[key]), "digest")
    require(type(identity["git_commit"]) is str
            and re.fullmatch(r"[0-9a-f]{40,64}", identity["git_commit"]), "commit")
    for key in ("platform", "architecture", "python"):
        require(type(identity[key]) is str and re.fullmatch(r"[A-Za-z0-9_.-]{1,64}", identity[key]), "platform")
    require(identity["features"] == [] and identity["binary_role"] == "fixture-interpreter", "features")


def validate(directory, expected_manifest, expected_identity=None, *, expected_run_id=None):
    """Reconcile all evidence, independently of runner exit status. Raises on refusal.

    A valid failed/blocked/partial report remains a failed/blocked/partial report.
    Consumers MUST check runner_status == passed to accept mechanics completion.
    """
    spec = manifest(expected_manifest)
    raw = read_bytes(Path(directory) / "events.jsonl")
    require(0 < len(raw) <= MAX_DOCUMENT and raw.endswith(b"\n"), "truncated-events")
    lines = raw.splitlines(keepends=True)
    events = [decode(line) for line in lines]
    require(events and all(type(e) is dict for e in events), "events")
    require(all(encode(e) == line for e, line in zip(events, lines, strict=True)), "canonical")
    header = events[0]
    keys(header, {"schema_version", "run_id", "seq", "kind", "suite", "tier", "planned", "selected",
                  "manifest_sha256", "identity", "network", "artifact_limit", "seed"})
    require(type(header["schema_version"]) is int and header["schema_version"] == VERSION
            and header["kind"] == "header" and header["suite"] == spec["suite"]
            and header["tier"] == spec["tier"] and type(header["seed"]) is int and header["seed"] == 0
            and type(header["artifact_limit"]) is int and header["artifact_limit"] == MAX_DOCUMENT, "header")
    identifiers(header["planned"])
    identifier(header["run_id"])
    if expected_run_id is not None:
        require(header["run_id"] == expected_run_id, "expected-run")
    identifiers(header["selected"])
    planned = [case["id"] for case in spec["cases"]]
    require(header["planned"] == planned and set(header["selected"]) <= set(planned), "selection")
    require(header["selected"] == [case for case in planned if case in header["selected"]], "selection-order")
    require(header["manifest_sha256"] == sha(encode(spec)), "manifest-identity")
    require(type(header["network"]) is str and header["network"] in {"isolated", "unavailable"}, "network")
    identity = header["identity"]
    validate_identity(identity)
    if expected_identity is not None:
        require(identity == expected_identity, "expected-identity")
    cases = {case["id"]: case for case in spec["cases"]}
    started, finished, active = set(), set(), None
    for index, event in enumerate(events):
        require(type(event.get("seq")) is int and event["seq"] == index, "sequence")
        require(event.get("run_id") == header["run_id"], "event-run")
        if index == 0:
            continue
        require(type(event.get("case")) is str and event["case"] in header["selected"], "case")
        require(type(event.get("kind")) is str and event["kind"] in {"case_start", "case_result"}, "kind")
        case_id = event["case"]
        if event["kind"] == "case_start":
            keys(event, {"run_id", "seq", "kind", "case", "step", "expected"})
            require(event["step"] == "child-execution"
                    and encode(event["expected"]) == encode(expectations(cases[case_id])), "expectations")
            require(active is None and case_id not in started and case_id not in finished, "duplicate-start")
            started.add(case_id)
            active = case_id
        else:
            keys(event, {"run_id", "seq", "kind", "case", "status", "reason", "exit_code", "elapsed_ms",
                         "stdout_bytes", "stderr_bytes", "assertions", "attempt", "observed", "fault_observed",
                         "fault_witness"})
            require(type(event["fault_observed"]) is bool, "fault-observed")
            require(event["fault_observed"] == (event["fault_witness"] is not None), "fault-witness")
            if event["fault_observed"]:
                witness = event["fault_witness"]
                keys(witness, {"run_id", "case", "id"})
                fault_id = {"hang": "timeout-entered", "childhang": "descendant-started"}.get(cases[case_id]["mode"])
                require(cases[case_id]["mode"] in {"hang", "childhang"}
                        and event["status"] == "failed" and witness["run_id"] == header["run_id"]
                        and witness["case"] == case_id and witness["id"] == fault_id, "fault-observed")
            require(event["kind"] == "case_result" and case_id not in finished, "terminal")
            require(type(event["status"]) is str and event["status"] in {"passed", "failed", "blocked"}
                    and type(event["reason"]) is str and event["reason"] in REASONS and type(event["attempt"]) is int
                    and event["attempt"] == 1, "status")
            for key in ("elapsed_ms", "stdout_bytes", "stderr_bytes"):
                integer(event[key], 0, 2**63 - 1)
            require(event["exit_code"] is None or (type(event["exit_code"]) is int
                    and -255 <= event["exit_code"] <= 255), "exit")
            if event["status"] == "blocked":
                require(case_id not in started and active is None and event["exit_code"] is None
                        and event["reason"] in {"sandbox-unavailable", "aggregate-deadline", "interrupted"}
                        and event["assertions"] == [] and event["observed"] is None, "blocked")
            else:
                require(active == case_id, "missing-start")
                active = None
                require(type(event["assertions"]) is list, "assertions")
                if event["assertions"]:
                    require(len(event["assertions"]) == len(cases[case_id]["assertions"]), "assertions")
                    for assertion, aid in zip(event["assertions"], cases[case_id]["assertions"], strict=True):
                        keys(assertion, {"id", "passed"})
                        require(assertion["id"] == aid and type(assertion["passed"]) is bool, "assertions")
                if event["observed"] is not None:
                    keys(event["observed"], {"outcome", "effects", "fault_reached"})
                    records = [{"schema_version": VERSION, "run_id": header["run_id"],
                                "case": case_id, "kind": "assertion", **a}
                               for a in event["assertions"]]
                    records.append({"schema_version": VERSION, "run_id": header["run_id"],
                                    "case": case_id, "kind": "result",
                                    **event["observed"]})
                    reason, _, observed = classify(cases[case_id], records, event["exit_code"], run_id=header["run_id"])
                    require(observed is not None and reason == event["reason"], "observed-mismatch")
                else:
                    require(event["reason"] in {"protocol", "timeout", "output-limit", "signal",
                                                "spawn", "interrupted"}, "missing-observed")
                if event["status"] == "passed":
                    require(event["reason"] == "matched" and header["network"] == "isolated"
                            and event["exit_code"] == cases[case_id]["expected_exit"]
                            and event["stdout_bytes"] + event["stderr_bytes"] <= cases[case_id]["output_bytes"]
                            and event["elapsed_ms"] <= cases[case_id]["timeout_ms"]
                            and len(event["assertions"]) == len(cases[case_id]["assertions"])
                            and all(a["passed"] for a in event["assertions"]), "false-pass")
                else:
                    require(event["reason"] != "matched", "false-failure")
            finished.add(case_id)
    summary = read_json(Path(directory) / "summary.json")
    keys(summary, {"schema_version", "run_id", "suite", "runner_status", "product_gate", "identity_stable",
                   "counts", "events_sha256"})
    require(type(summary["identity_stable"]) is bool and type(summary["schema_version"]) is int, "summary")
    keys(summary["counts"], {"planned", "selected", "started", "skipped", "passed", "failed", "blocked", "incomplete"})
    for count in summary["counts"].values():
        integer(count, 0, 128)
    require(summary == summarize(header, events, summary["identity_stable"]), "reconciliation")
    return summary
