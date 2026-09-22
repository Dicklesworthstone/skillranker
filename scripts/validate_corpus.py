#!/usr/bin/env python3
"""Validate an adjudicated SkillRanker relevance corpus (sr-uv2v).

Mechanical exit gate for docs/relevance-corpus-contract.md. It checks the
corpus manifest and case records against the pre-registered contract: split
disjointness, family atomicity, declared stratum minimums, adjudication
records, adjudicator independence, near-miss class integrity, label/roster
consistency, primary-case uniqueness, and frozen split digests.

This script does not evaluate SkillRanker, call Jev, or claim any promotion
gate has passed. A passing corpus still needs its declared population to
support whatever claim a report makes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Any

MANIFEST_SCHEMA = "skillranker.corpus_manifest.v1"
CASE_SCHEMA = "skillranker.adjudicated_case.v1"
MAX_MANIFEST_BYTES = 1024 * 1024
MAX_CASES_BYTES = 256 * 1024 * 1024
MAX_RECORD_BYTES = 1024 * 1024
MAX_CASES = 10_000
SPLIT_NAMES = ("training", "validation", "holdout")
CASE_KINDS = ("positive_advisory", "no_match_advisory", "near_miss_advisory")

sys.setrecursionlimit(200)


class Reject(Exception):
    pass


def fail(message: str) -> None:
    raise SystemExit(f"corpus validation failed: {message}")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise Reject(message)


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON object key")
        result[key] = value
    return result


@contextmanager
def regular_file(path: Path, max_bytes: int, label: str):
    # O_NONBLOCK ensures opening a supplied FIFO cannot hang before fstat.
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(fd, "rb") as handle:
        mode = os.fstat(handle.fileno()).st_mode
        require(stat.S_ISREG(mode), f"{label} must be a regular file")
        require(os.fstat(handle.fileno()).st_size <= max_bytes, f"{label} exceeds its byte bound")
        yield handle


def load_json(path: Path, max_bytes: int, label: str) -> Any:
    with regular_file(path, max_bytes, label) as handle:
        try:
            return json.load(handle, object_pairs_hook=unique_object)
        except RecursionError:
            fail(f"{label} exceeds the nesting bound")
        except Reject as error:
            fail(f"{label}: {error}")
        except json.JSONDecodeError as error:
            fail(f"{label} is not valid JSON: {error}")


def canonical_digest(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def require_str(value: Any, field: str) -> str:
    require(isinstance(value, str) and value != "", f"{field} must be a non-empty string")
    return value


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--cases", required=True, type=Path)
    args = parser.parse_args()

    try:
        run(args.manifest, args.cases)
    except Reject as error:
        fail(str(error))


def run(manifest_path: Path, cases_path: Path) -> None:
    manifest = load_json(manifest_path, MAX_MANIFEST_BYTES, "manifest")
    require(isinstance(manifest, dict), "manifest must be a JSON object")
    require(manifest.get("schema") == MANIFEST_SCHEMA, "manifest schema mismatch")

    rubric_version = require_str(manifest.get("rubric_version"), "rubric_version")
    selector_identity = require_str(manifest.get("selector_identity"), "selector_identity")
    declared = manifest.get("declared_population")
    require(isinstance(declared, dict), "declared_population must be an object")
    require_str(declared.get("description"), "declared_population.description")
    require(isinstance(declared.get("narrowed"), bool), "declared_population.narrowed must be bool")
    require_str(declared.get("justification"), "declared_population.justification")

    minimums = manifest.get("strata_minimums")
    require(isinstance(minimums, dict), "strata_minimums must be an object")
    strata_keys = ("positive", "no_match", "near_miss", "overflow_positive")
    for key in strata_keys:
        value = minimums.get(key, 0)
        require(
            isinstance(value, int) and not isinstance(value, bool) and value >= 0,
            f"strata_minimums.{key} must be a non-negative integer",
        )

    adjudicators = manifest.get("adjudicators")
    require(isinstance(adjudicators, list) and adjudicators, "adjudicators must be a non-empty list")
    adjudicator_ids: dict[str, set[str]] = {}
    for entry in adjudicators:
        require(isinstance(entry, dict), "adjudicator entry must be an object")
        adj_id = require_str(entry.get("id"), "adjudicator.id")
        require(adj_id not in adjudicator_ids, f"duplicate adjudicator {adj_id}")
        forbidden = entry.get("forbidden_sessions", [])
        require(isinstance(forbidden, list), "adjudicator.forbidden_sessions must be a list")
        adjudicator_ids[adj_id] = set(forbidden)
    require(
        selector_identity not in adjudicator_ids,
        "the selector identity cannot be registered as an adjudicator",
    )

    splits = manifest.get("splits")
    require(isinstance(splits, dict), "splits must be an object")
    frozen_lists: dict[str, list[str]] = {}
    for name in SPLIT_NAMES:
        split = splits.get(name)
        require(isinstance(split, dict), f"splits.{name} must be an object")
        case_ids = split.get("case_ids")
        require(isinstance(case_ids, list), f"splits.{name}.case_ids must be a list")
        require(all(isinstance(c, str) for c in case_ids), f"splits.{name}.case_ids must be strings")
        require(
            len(case_ids) == len(set(case_ids)),
            f"splits.{name}.case_ids contains duplicates",
        )
        digest = require_str(split.get("digest"), f"splits.{name}.digest")
        expected = canonical_digest({"split": name, "case_ids": case_ids})
        require(digest == expected, f"splits.{name}.digest does not match its frozen case list")
        frozen_lists[name] = case_ids

    all_listed: dict[str, str] = {}
    for name, ids in frozen_lists.items():
        for case_id in ids:
            require(
                case_id not in all_listed,
                f"case {case_id} appears in more than one split ({all_listed.get(case_id)} and {name})",
            )
            all_listed[case_id] = name

    # --- case records ---
    cases: dict[str, dict[str, Any]] = {}
    family_split: dict[str, str] = {}
    family_primary: dict[str, str] = {}
    counts = {key: 0 for key in strata_keys}
    double_judged = 0

    with regular_file(cases_path, MAX_CASES_BYTES, "cases") as handle:
        for line_number, raw in enumerate(handle, start=1):
            if len(raw) > MAX_RECORD_BYTES:
                fail(f"case record at line {line_number} exceeds its byte bound")
            line = raw.strip()
            if not line:
                continue
            if len(cases) >= MAX_CASES:
                fail("case count exceeds its bound")
            try:
                case = json.loads(line, object_pairs_hook=unique_object)
            except Reject as error:
                fail(f"case at line {line_number}: {error}")
            except json.JSONDecodeError as error:
                fail(f"case at line {line_number} is not valid JSON: {error}")
            require(isinstance(case, dict), f"case at line {line_number} must be an object")
            require(
                case.get("schema_version") == CASE_SCHEMA,
                f"case at line {line_number} has a schema mismatch",
            )
            case_id = require_str(case.get("case_id"), f"line {line_number} case_id")
            require(case_id not in cases, f"duplicate case {case_id}")
            cases[case_id] = case
            check_case(case, case_id, rubric_version, selector_identity, adjudicator_ids)

            family_id = require_str(case.get("family_id"), f"{case_id}.family_id")
            split = require_str(case.get("split"), f"{case_id}.split")
            require(split in SPLIT_NAMES, f"{case_id}.split is not a declared split")
            require(
                family_split.setdefault(family_id, split) == split,
                f"family {family_id} spans splits ({family_split[family_id]} and {split})",
            )
            require(
                isinstance(case.get("primary_family_case"), bool),
                f"{case_id}.primary_family_case must be bool",
            )
            if case["primary_family_case"]:
                require(
                    family_id not in family_primary,
                    f"family {family_id} has more than one primary case",
                )
                family_primary[family_id] = case_id

            kind = require_str(case.get("case_kind"), f"{case_id}.case_kind")
            acceptable = case.get("acceptable_additional_invocations_y")
            near_miss = case.get("near_miss_skill_ids")
            if kind == "positive_advisory":
                counts["positive"] += 1
            elif kind == "no_match_advisory":
                require(acceptable == [], f"{case_id} is no-match but names acceptable skills")
                counts["no_match"] += 1
            elif kind == "near_miss_advisory":
                counts["near_miss"] += 1
            if case.get("overflow") is True and acceptable:
                counts["overflow_positive"] += 1
            if len(case.get("adjudications")) > 1:
                double_judged += 1
            require(isinstance(near_miss, list), f"{case_id}.near_miss_skill_ids must be a list")

    for family_id in family_split:
        require(
            family_id in family_primary,
            f"family {family_id} has no primary case",
        )

    # Frozen split membership matches the corpus exactly, in both directions.
    for case_id, case in cases.items():
        require(
            case_id in all_listed,
            f"case {case_id} is not in any frozen split case list",
        )
        require(
            all_listed[case_id] == case["split"],
            f"case {case_id} declares split {case['split']} but the manifest froze it in "
            f"{all_listed[case_id]}",
        )
    for case_id in all_listed:
        require(case_id in cases, f"frozen case {case_id} is missing from the corpus")

    for key in strata_keys:
        require(
            counts[key] >= minimums.get(key, 0),
            f"stratum {key} has {counts[key]} cases, below the declared minimum "
            f"{minimums.get(key, 0)}",
        )

    fraction = manifest.get("double_judgment_fraction", 0)
    require(
        isinstance(fraction, (int, float)) and not isinstance(fraction, bool) and 0 <= fraction <= 1,
        "double_judgment_fraction must be in [0, 1]",
    )
    if cases and fraction > 0:
        require(
            double_judged >= fraction * len(cases),
            f"only {double_judged} of {len(cases)} cases are double-judged, below the declared "
            f"fraction {fraction}",
        )

    dataset_digest = canonical_digest(
        {name: frozen_lists[name] for name in SPLIT_NAMES}
    )
    recorded = manifest.get("dataset_digest")
    if recorded is not None:
        require(recorded == dataset_digest, "dataset_digest does not match the frozen split lists")

    print(
        json.dumps(
            {
                "status": "passed",
                "cases": len(cases),
                "families": len(family_split),
                "strata": counts,
                "double_judged": double_judged,
                "dataset_digest": dataset_digest,
                "declared_population": declared["description"],
                "narrowed": declared["narrowed"],
            },
            indent=2,
        )
    )


def check_case(
    case: dict[str, Any],
    case_id: str,
    rubric_version: str,
    selector_identity: str,
    adjudicators: dict[str, set[str]],
) -> None:
    kind = require_str(case.get("case_kind"), f"{case_id}.case_kind")
    require(kind in CASE_KINDS, f"{case_id}.case_kind is not a declared class")
    require(isinstance(case.get("constructed"), bool), f"{case_id}.constructed must be bool")
    require(isinstance(case.get("overflow"), bool), f"{case_id}.overflow must be bool")
    require_str(case.get("consent_reference"), f"{case_id}.consent_reference")

    roster = case.get("roster")
    require(isinstance(roster, dict), f"{case_id} is missing its roster manifest")
    require_str(roster.get("manifest_digest"), f"{case_id}.roster.manifest_digest")
    skills = roster.get("skills")
    require(isinstance(skills, list), f"{case_id}.roster.skills must be a list")
    roster_ids: set[str] = set()
    manual_only: set[str] = set()
    for skill in skills:
        require(isinstance(skill, dict), f"{case_id} roster skill must be an object")
        skill_id = require_str(skill.get("skill_id"), f"{case_id} roster skill_id")
        require(skill_id not in roster_ids, f"{case_id} roster repeats {skill_id}")
        roster_ids.add(skill_id)
        if skill.get("manual_only") is True:
            manual_only.add(skill_id)

    acceptable = case.get("acceptable_additional_invocations_y")
    require(isinstance(acceptable, list), f"{case_id}.acceptable_additional_invocations_y")
    require(
        len(acceptable) == len(set(acceptable)),
        f"{case_id} names an acceptable skill twice",
    )
    near_miss = case.get("near_miss_skill_ids")
    require(isinstance(near_miss, list), f"{case_id}.near_miss_skill_ids")
    require(len(near_miss) == len(set(near_miss)), f"{case_id} names a near-miss skill twice")

    unjudgeable = case.get("unjudgeable", False)
    require(isinstance(unjudgeable, bool), f"{case_id}.unjudgeable must be bool")
    if unjudgeable:
        require_str(case.get("unjudgeable_reason"), f"{case_id}.unjudgeable_reason")
    for skill_id in acceptable:
        require(skill_id in roster_ids, f"{case_id} label {skill_id} is not in the roster")
        require(
            skill_id not in manual_only,
            f"{case_id} names manual-only skill {skill_id} as acceptable",
        )
    for skill_id in near_miss:
        require(skill_id in roster_ids, f"{case_id} near-miss {skill_id} is not in the roster")
        require(
            skill_id not in acceptable,
            f"{case_id} names {skill_id} as both acceptable and near-miss",
        )

    if kind == "near_miss_advisory":
        require(
            acceptable != [],
            f"{case_id} is near-miss but has an empty acceptable set; that is a no-match",
        )
        require(
            near_miss != [],
            f"{case_id} is near-miss but names no near-miss skill",
        )
    if kind == "positive_advisory":
        require(acceptable != [], f"{case_id} is positive but has an empty acceptable set")

    adjudications = case.get("adjudications")
    require(
        isinstance(adjudications, list) and adjudications,
        f"{case_id} is missing its adjudication record",
    )
    seen: set[str] = set()
    for record in adjudications:
        require(isinstance(record, dict), f"{case_id} adjudication record must be an object")
        adjudicator = require_str(record.get("adjudicator"), f"{case_id} adjudication.adjudicator")
        require(
            adjudicator in adjudicators,
            f"{case_id} adjudicator {adjudicator} is not in the manifest registry",
        )
        require(adjudicator not in seen, f"{case_id} is adjudicated twice by {adjudicator}")
        seen.add(adjudicator)
        require(
            adjudicator != selector_identity,
            f"{case_id} was adjudicated by the selector itself",
        )
        require(
            case_id not in adjudicators[adjudicator],
            f"{case_id} was adjudicated by {adjudicator}, who is forbidden from judging it",
        )
        require(
            record.get("blindness_attested") is True,
            f"{case_id} adjudication by {adjudicator} lacks a blindness attestation",
        )
        require(
            record.get("rubric_version") == rubric_version,
            f"{case_id} was adjudicated under a different rubric version",
        )


if __name__ == "__main__":
    main()
