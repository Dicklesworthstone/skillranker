#!/usr/bin/env python3
"""Export validated primary corpus cases and recorded outcomes for offline sr eval.

This prepares evidence; it neither runs the selector nor establishes a quality
gate. Each recorded wrapper must bind the exact adjudicated case and roster.
Only a receipt published last commits the two individually atomic output files.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import secrets
from pathlib import Path

from validate_corpus import (
    MAX_CASES,
    MAX_CASES_BYTES,
    MAX_RECORD_BYTES,
    SPLIT_NAMES,
    Reject,
    canonical_digest,
    decode_bounded_json,
    load_json,
    regular_file,
    require,
    require_str,
    validate,
)

WRAPPER_SCHEMA = "skillranker.corpus_recorded_case.v1"
RECEIPT_SCHEMA = "skillranker.corpus_frame_export.v1"
SPLITS = {"training": "train", "validation": "validation", "holdout": "holdout"}
RECORD_FIELDS = {
    "schema_version", "key", "split", "prompt_summary", "roster_skills", "decision",
    "suggested_skills", "fits", "gate_score", "relevance_abstention", "operational_failure",
}
KEY_FIELDS = {"frame_id", "family_id", "case_id", "replicate", "policy_id"}


def integer(value, label, positive=False):
    require(type(value) is int and (1 if positive else 0) <= value <= 2**64 - 1,
            f"{label} must be a bounded integer")
    return value


def strings(value, label):
    require(isinstance(value, list), f"{label} must be a list")
    require(all(isinstance(v, str) and v.strip() for v in value), f"{label} contains an invalid ID")
    require(len(value) == len(set(value)), f"{label} contains duplicate IDs")
    return value


def probability(value, label):
    require(type(value) in (int, float) and 0 <= value <= 1 and math.isfinite(value),
            f"{label} must be a finite probability")


def check_record(record, case):
    require(isinstance(record, dict) and not record.keys() - RECORD_FIELDS,
            "record has unknown fields")
    require(type(record.get("schema_version")) is int and record["schema_version"] == 1,
            "record schema mismatch")
    key = record.get("key")
    require(isinstance(key, dict) and key.keys() == KEY_FIELDS, "record key fields mismatch")
    for name in KEY_FIELDS - {"replicate"}:
        require(require_str(key[name], name).strip() != "", f"{name} is blank")
    require(type(key["replicate"]) is int and key["replicate"] == 0,
            "primary export requires replicate zero")
    require(key["case_id"] == case["case_id"] and key["family_id"] == case["family_id"],
            "record case/family identity mismatch")
    require(record.get("split") == SPLITS[case["split"]], "record split mismatch")
    roster = strings(record.get("roster_skills"), "roster_skills")
    require(set(roster) == {s["skill_id"] for s in case["roster"]["skills"]},
            "record roster differs from the adjudicated roster")
    suggested = strings(record.get("suggested_skills"), "suggested_skills")
    require(set(suggested) <= set(roster), "suggested ID absent from roster")
    # Keep erroneous manual-only suggestions: the independent labels score
    # them as wrong. Export must not erase a selector's actual mistakes.
    decision = record.get("decision")
    require(decision in ("ranked", "abstain", "unavailable"),
            "advisory export refuses explicit or unknown decisions")
    for name in ("relevance_abstention", "operational_failure"):
        require(type(record.get(name)) is bool, f"{name} must be explicit bool")
    require(record["operational_failure"] == (decision == "unavailable"),
            "operational failure must retain unavailable status")
    require(record["relevance_abstention"] == (decision == "abstain"),
            "relevance abstention status mismatch")
    require(bool(suggested) == (decision == "ranked"), "decision/suggestions mismatch")
    require(record.get("prompt_summary") is None or isinstance(record["prompt_summary"], str),
            "prompt_summary must be string or null")
    fits = record.get("fits", {})
    require(isinstance(fits, dict) and fits.keys() <= set(roster), "fits contain foreign IDs")
    for value in fits.values():
        probability(value, "fit")
    if record.get("gate_score") is not None:
        probability(record["gate_score"], "gate_score")


def load_recorded(path, cases, primary_ids):
    recorded = {}
    with regular_file(path, MAX_CASES_BYTES, "recorded frame") as handle:
        total = 0
        for raw in iter(lambda: handle.readline(MAX_RECORD_BYTES + 1), b""):
            total += len(raw)
            require(total <= MAX_CASES_BYTES and len(raw) <= MAX_RECORD_BYTES,
                    "recorded frame exceeds byte bounds")
            if not raw.strip():
                continue
            require(len(recorded) < MAX_CASES, "recorded frame exceeds record bound")
            wrapper = decode_bounded_json(raw, "recorded frame record")
            require(isinstance(wrapper, dict) and set(wrapper) == {
                "schema", "case_digest", "roster_manifest_digest", "record",
            }, "recorded wrapper fields mismatch")
            require(wrapper["schema"] == WRAPPER_SCHEMA, "recorded wrapper schema mismatch")
            record = wrapper["record"]
            require(isinstance(record, dict) and isinstance(record.get("key"), dict),
                    "recorded frame key missing")
            case_id = record["key"].get("case_id")
            require(isinstance(case_id, str) and case_id in primary_ids,
                    "recorded case is not a frozen primary")
            require(case_id not in recorded, "duplicate recorded case")
            case = cases[case_id]
            require(wrapper["case_digest"] == canonical_digest(case), "case content digest mismatch")
            require(wrapper["roster_manifest_digest"] == case["roster"]["manifest_digest"],
                    "roster manifest digest mismatch")
            check_record(record, case)
            recorded[case_id] = record
    require(recorded.keys() == primary_ids, "missing recorded primary cases; do not invent outcomes")
    require(len({r["key"]["frame_id"] for r in recorded.values()}) == 1,
            "mixed frame identities")
    require(len({r["key"]["policy_id"] for r in recorded.values()}) == 1,
            "mixed policy identities")
    return recorded


def encode(value):
    try:
        return (json.dumps(value, sort_keys=True, separators=(",", ":"),
                           allow_nan=False, ensure_ascii=False) + "\n").encode()
    except UnicodeEncodeError as error:
        raise Reject("export contains a non-Unicode string") from error


def export(manifest_path, cases_path, recorded_path, output, revision):
    integer(revision, "label revision", positive=True)
    manifest, cases, corpus_receipt = validate(manifest_path, cases_path)
    primary = [case_id for split in SPLIT_NAMES
               for case_id in corpus_receipt["primary_case_ids_by_split"][split]]
    require(bool(primary), "cannot export an empty primary cohort")
    recorded = load_recorded(recorded_path, cases, set(primary))
    labels = []
    for case_id in primary:
        case = cases[case_id]
        if case.get("unjudgeable", False):
            continue  # Retain the frame record, with no fabricated relevance label.
        judgments = case["adjudications"]
        times = [integer(j.get("labeled_at_unix_ms"), "adjudication time") for j in judgments]
        labels.append({
            "schema_version": 1, "case_id": case_id, "revision": revision,
            "acceptable_skills": case["acceptable_additional_invocations_y"],
            "no_skill_needed": case["case_kind"] == "no_match_advisory",
            "adjudicator": json.dumps(sorted(j["adjudicator"] for j in judgments)),
            "created_at_unix_ms": max(times),
            "notes": "Corrupted or absent export receipt invalidates this corpus/frame binding.",
        })
    payloads = {
        "dataset.jsonl": b"".join(encode(recorded[c]) for c in primary),
        "labels.jsonl": b"".join(encode(label) for label in labels),
    }
    require(all(len(body) <= MAX_CASES_BYTES for body in payloads.values()), "export exceeds byte bound")
    receipt = {
        "schema": RECEIPT_SCHEMA, "scope": "offline-preparation-only",
        "quality_gate": "not-established", "corpus_validation": corpus_receipt,
        "manifest_content_digest": canonical_digest(manifest),
        "corpus_content_digest": canonical_digest(cases),
        "recorded_content_digest": canonical_digest(recorded),
        "primary_cases": len(primary), "labels": len(labels), "unjudged": len(primary) - len(labels),
        "operational_failures": sum(r["operational_failure"] for r in recorded.values()),
        "frame_id": recorded[primary[0]]["key"]["frame_id"],
        "policy_id": recorded[primary[0]]["key"]["policy_id"], "label_revision": revision,
        "files": {name: hashlib.sha256(body).hexdigest() for name, body in payloads.items()},
    }
    payloads["receipt.json"] = encode(receipt)
    require(len(payloads["receipt.json"]) <= 2 * 1024 * 1024, "receipt exceeds byte bound")
    # Only trusted, owner-only destination directories. A staging directory is
    # deliberately retained on both success and failure; never delete user data.
    directory = os.open(output, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    staging_fd = None
    try:
        info = os.fstat(directory)
        require(info.st_uid == os.getuid() and info.st_mode & 0o077 == 0,
                "output must be an existing owner-only directory")
        for name in payloads:
            try:
                os.stat(name, dir_fd=directory, follow_symlinks=False)
            except FileNotFoundError:
                continue
            raise Reject("export target already exists")
        # Pin both directories by descriptor so concurrent renames cannot send
        # private writes to a replacement destination or swap staged bytes.
        staging_name = ".corpus-frame-" + secrets.token_hex(16)
        os.mkdir(staging_name, 0o700, dir_fd=directory)
        staging_fd = os.open(staging_name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
        for name, body in payloads.items():
            fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=staging_fd)
            with os.fdopen(fd, "wb") as handle:
                handle.write(body)
                handle.flush()
                os.fsync(handle.fileno())
        os.fsync(staging_fd)
        for name in payloads:  # receipt last; link is atomic and refuses raced-in targets.
            os.link(name, name, src_dir_fd=staging_fd, dst_dir_fd=directory, follow_symlinks=False)
            os.fsync(directory)
    finally:
        if staging_fd is not None:
            os.close(staging_fd)
        os.close(directory)
    return receipt


def verify_export(output):
    """Detect incomplete exports and changed bytes, not authenticate evidence."""
    receipt = load_json(output / "receipt.json", 2 * 1024 * 1024, "export receipt")
    require(isinstance(receipt, dict) and receipt.get("schema") == RECEIPT_SCHEMA,
            "unsupported export receipt")
    require(receipt.get("quality_gate") == "not-established", "export is not a quality gate")
    files = receipt.get("files")
    require(isinstance(files, dict) and files.keys() == {"dataset.jsonl", "labels.jsonl"},
            "export receipt file set mismatch")
    for name, expected in files.items():
        digest = hashlib.sha256()
        with regular_file(output / name, MAX_CASES_BYTES, "export file") as handle:
            total = 0
            for chunk in iter(lambda: handle.read(64 * 1024), b""):
                total += len(chunk)
                require(total <= MAX_CASES_BYTES, "export exceeds byte bound")
                digest.update(chunk)
        require(digest.hexdigest() == expected, "export file digest mismatch")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("manifest", "cases", "recorded-frame"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--label-revision", type=int)
    parser.add_argument("--verify", action="store_true")
    args = parser.parse_args()
    inputs = (args.manifest, args.cases, args.recorded_frame, args.label_revision)
    if (args.verify and any(v is not None for v in inputs)) or (not args.verify and any(v is None for v in inputs)):
        parser.error("choose --verify alone or supply all export inputs and --label-revision")
    try:
        receipt = verify_export(args.output_dir) if args.verify else export(
            args.manifest, args.cases, args.recorded_frame, args.output_dir, args.label_revision)
    except (Reject, OSError) as error:
        # OSError paths may contain private data; omit them from diagnostics.
        parser.exit(1, f"corpus frame export failed: {error.strerror if isinstance(error, OSError) else error}\n")
    if args.verify:
        print(json.dumps({"scope": "export-file-integrity-only", "files_verified": 2,
                          "quality_gate": "not-established"}))
        return
    print(json.dumps({"scope": receipt["scope"], "primary_cases": receipt["primary_cases"],
                      "labels": receipt["labels"], "quality_gate": receipt["quality_gate"]}))


if __name__ == "__main__":
    main()
