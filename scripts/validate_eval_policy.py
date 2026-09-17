#!/usr/bin/env python3
"""Validate SkillRanker's frozen v1 evaluation policy fixtures.

This is a lightweight stdlib contract checker for sr-roadmap-l1i.1.8. It does
not evaluate SkillRanker, call Jev, inspect harnesses, or claim benchmark pass.
"""
from __future__ import annotations

import argparse
import json
import math
import os
import re
import stat
from contextlib import contextmanager
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_POLICY = ROOT / "tests" / "eval" / "evaluation_policy.v1.json"
DEFAULT_CASES = ROOT / "tests" / "eval" / "synthetic_cases.v1.jsonl"
DEFAULT_EXPECTED = ROOT / "tests" / "eval" / "expected_values.v1.json"

POLICY_SCHEMA = "skillranker.evaluation_policy.v1"
CASE_SCHEMA = "skillranker.synthetic_case.v1"
EXPECTED_SCHEMA = "skillranker.expected_values.v1"
MAX_ARTIFACT_BYTES = 1024 * 1024
MAX_RECORD_BYTES = 64 * 1024


@contextmanager
def regular_file(path: Path):
    # O_NONBLOCK ensures opening a supplied FIFO cannot hang before fstat.
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(fd, "rb") as handle:
        require(stat.S_ISREG(os.fstat(handle.fileno()).st_mode), "artifact must be a regular file")
        yield handle


def fail(message: str) -> None:
    raise SystemExit(f"validation failed: {message}")


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON object key")
        result[key] = value
    return result


def finite_float(text: str) -> float:
    value = float(text)
    require(math.isfinite(value), "non-finite JSON number")
    return value


def strict_json(raw: bytes) -> Any:
    require(len(raw) <= MAX_ARTIFACT_BYTES, "contract artifact exceeds byte cap")
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object,
                           parse_float=finite_float,
                           parse_constant=lambda _: fail("non-finite JSON constant"))
    except (ValueError, RecursionError):
        fail("invalid or excessively nested JSON artifact")
    stack = [(value, 0)]
    while stack:
        node, depth = stack.pop()
        if isinstance(node, (list, dict)):
            require(depth < 64, "contract artifact exceeds nesting cap")
            children = node.values() if isinstance(node, dict) else node
            stack.extend((child, depth + 1) for child in children)
    return value


def load_json(path: Path) -> Any:
    with regular_file(path) as f:
        raw = f.read(MAX_ARTIFACT_BYTES + 1)
    require(len(raw) <= MAX_ARTIFACT_BYTES, "contract artifact exceeds byte cap")
    return strict_json(raw)


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    total = 0
    with regular_file(path) as f:
        while raw := f.readline(MAX_RECORD_BYTES + 1):
            total += len(raw)
            require(len(raw) <= MAX_RECORD_BYTES, "contract record exceeds byte cap")
            require(total <= MAX_ARTIFACT_BYTES, "contract cases exceed aggregate byte cap")
            require(len(rows) < 10000, "contract cases exceed record cap")
            line = raw.strip()
            if not line:
                fail("blank JSONL records are not allowed")
            row = strict_json(line)
            if not isinstance(row, dict):
                fail("case record must be an object")
            rows.append(row)
    return rows


def require(condition: bool, message: str) -> None:
    if not condition:
        fail(message)


def approx(a: float, b: float, tol: float = 1e-9) -> bool:
    return abs(a - b) <= tol


def json_number(value: Any) -> bool:
    return type(value) in (int, float) and math.isfinite(value)


def unique_ids(rows: Any, key: str) -> None:
    require(isinstance(rows, list), "definition collection must be an array")
    seen = set()
    for row in rows:
        require(isinstance(row, dict), "definition must be an object")
        identifier = row.get(key)
        require(isinstance(identifier, str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", identifier),
                "definition identifier must be bounded and safe")
        require(identifier not in seen, "duplicate definition identifier")
        seen.add(identifier)


def validate_policy(policy: dict[str, Any]) -> None:
    require(policy.get("schema_version") == POLICY_SCHEMA, "policy schema_version mismatch")
    require(policy.get("status") == "frozen_contract_not_evidence", "policy must be marked not evidence")
    non_claims = policy.get("non_claims")
    require(isinstance(non_claims, list) and len(non_claims) >= 4, "policy must list non-claims")
    serialized_non_claims = "\n".join(str(x).lower() for x in non_claims)
    for phrase in ["not an evaluator", "not a 300-family", "synthetic", "no live"]:
        require(phrase in serialized_non_claims, f"missing non-claim phrase: {phrase}")

    reqs = policy.get("promotion_requirements", {})
    relevance = reqs.get("relevance_cohort", {})
    require(relevance.get("minimum_primary_cases") == 300, "relevance cohort must require 300 primary cases")
    require(relevance.get("minimum_positive_cases") == 150, "relevance cohort must require 150 positives")
    require(relevance.get("minimum_no_match_cases") == 100, "relevance cohort must require 100 no-match cases")
    require(relevance.get("minimum_near_miss_cases_across_groups") == 50, "relevance cohort must require 50 near-miss cases")
    require("controlled_harm_cohort" in relevance.get("separate_from", []), "relevance cohort must stay separate from harm cohort")

    harm = reqs.get("controlled_harm", {})
    require(harm.get("minimum_independent_task_family_pairs_for_zero_event_iid_claim") == 150, "harm gate must require 150 independent family pairs for zero-event iid claim")
    bound = harm.get("maximum_one_sided_95_upper_bound")
    require(json_number(bound) and approx(bound, 0.02), "harm upper bound must be numeric 0.02")
    require(harm.get("endpoint") == "new_harm_or_unresolved", "harm endpoint must include unresolved cases")

    operational = reqs.get("operational_fallback", {})
    require(operational.get("minimum_representative_hook_invocations") == 500, "operational cohort must require 500 invocations")
    rate = operational.get("maximum_fallback_rate")
    require(json_number(rate) and approx(rate, 0.05), "operational fallback max must be numeric 0.05")

    loss_values = policy.get("loss_policy", {}).get("loss_values", {})
    expected_loss = {
        "correct_recommendation_on_positive": 0,
        "correct_no_match_abstention": 0,
        "false_abstention_on_positive": 1,
        "incorrect_recommendation_on_positive": 2,
        "needless_recommendation_on_no_match": 2,
        "operationally_unavailable_on_attempted_case": 2,
    }
    require(loss_values == expected_loss, "loss table must exactly match frozen 0/1/2 policy")
    require(all(type(x) is int for x in loss_values.values()), "loss values must be integers, not booleans")
    require(policy.get("uncertainty_and_sampling", {}).get("model_assumptions_required") is True,
            "confidence claims require a prospectively justified endpoint model")
    for name, values in {
        "overflow_candidate_coverage_at_254": {"minimum_positive_overflow_cases": 50, "minimum_rate": 0.98},
        "shortlist_candidate_coverage_at_m": {"minimum_rate": 0.95},
        "top_one_precision": {"minimum_rate": 0.90, "minimum_two_sided_95_wilson_lower_endpoint": 0.80},
        "positive_case_suggestion_rate": {"minimum_rate": 0.80, "abstention_counts_as_miss": True},
        "needless_suggestion_rate": {"maximum_rate": 0.05, "maximum_two_sided_95_wilson_upper_endpoint": 0.10},
    }.items():
        require(all((reqs.get(name, {}).get(k) is v if type(v) is bool
                     else json_number(reqs.get(name, {}).get(k)) and reqs[name][k] == v)
                    for k, v in values.items()),
                f"frozen promotion threshold mismatch: {name}")

    missing = policy.get("missing_failed_unstarted_semantics", {})
    require(missing.get("missing_replay_response", {}).get("status") == "not_estimable_for_policies_requiring_that_response", "missing replay responses must be not estimable")
    require(missing.get("unstarted_due_to_batch_cap_or_deadline", {}).get("status") == "unfinished_partial_report", "unstarted cases must be unfinished partial reports")


def validate_cases(cases: list[dict[str, Any]], policy: dict[str, Any]) -> None:
    require(cases, "synthetic cases must not be empty")
    seen_cases: set[str] = set()
    seen_families: set[str] = set()
    kinds: set[str] = set()
    allowed_kinds = set(policy.get("case_model", {}).get("case_kinds", []))
    for row in cases:
        require(row.get("schema_version") == CASE_SCHEMA, "case schema_version mismatch")
        case_id = row.get("case_id")
        family_id = row.get("family_id")
        require(isinstance(case_id, str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", case_id), "case_id must be a bounded safe identifier")
        require(case_id not in seen_cases, f"duplicate case_id {case_id}")
        seen_cases.add(case_id)
        require(isinstance(family_id, str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", family_id), f"{case_id}: invalid family_id")
        kind = row.get("case_kind")
        require(isinstance(kind, str) and kind in allowed_kinds, f"{case_id}: unknown case_kind")
        kinds.add(str(kind))
        y = row.get("acceptable_additional_invocations_y")
        require(isinstance(y, list), f"{case_id}: Y must be a list")
        require(all(isinstance(x, str) and re.fullmatch(r"[A-Za-z0-9_.:-]{1,128}", x) for x in y), f"{case_id}: invalid Y member")
        require(len(set(y)) == len(y), f"{case_id}: duplicate Y member")
        require(row.get("split") == "diagnostic_synthetic", f"{case_id}: synthetic case is not promotion evidence")
        require(type(row.get("primary_family_case")) is bool, "primary-family marker must be boolean")
        require("oracle" in row and isinstance(row["oracle"], dict), f"{case_id}: oracle object required")
        eligible = kind not in {"explicit_request", "roster_change", "operational_failure_semantics"}
        require(row["oracle"].get("advisory_metrics_eligible") is eligible,
                f"{case_id}: advisory metric eligibility contradicts case kind")
        require(row["oracle"].get("explicit_metrics_eligible") is (kind == "explicit_request"),
                f"{case_id}: explicit metric eligibility contradicts case kind")
        if row.get("primary_family_case") is True:
            require(family_id not in seen_families, f"family {family_id} has multiple primary cases")
            seen_families.add(family_id)
        if kind == "explicit_request":
            require(row["oracle"].get("advisory_metrics_eligible") is False, f"{case_id}: explicit request must not inflate advisory metrics")
        if kind in {"no_match_advisory", "loaded_reference_empty_y"}:
            require(y == [], f"{case_id}: no-match advisory case must have empty Y")
        if kind in {"positive_advisory", "near_miss_advisory", "multiple_valid_advisory", "planning_or_explanation", "repeatable_workflow_nonempty_y", "overflow_retrieval", "compacted_or_terse_context"}:
            require(len(y) >= 1, f"{case_id}: positive-like case must have non-empty Y")
    required_kinds = {
        "positive_advisory",
        "no_match_advisory",
        "near_miss_advisory",
        "multiple_valid_advisory",
        "explicit_request",
        "planning_or_explanation",
        "loaded_reference_empty_y",
        "repeatable_workflow_nonempty_y",
        "overflow_retrieval",
        "compacted_or_terse_context",
        "roster_change",
        "operational_failure_semantics",
    }
    require(required_kinds <= kinds, f"missing representative case kinds: {sorted(required_kinds - kinds)}")


def validate_expected(expected: dict[str, Any]) -> None:
    require(expected.get("schema_version") == EXPECTED_SCHEMA, "expected_values schema_version mismatch")
    for collection in ("loss_examples", "coverage_examples", "missing_failed_unstarted_examples", "harm_interval_examples"):
        unique_ids(expected.get(collection), "example_id")
    examples = expected.get("loss_examples")
    require(isinstance(examples, list) and examples, "loss_examples required")
    for ex in examples:
        loss = ex.get("loss")
        norm = ex.get("normalized_y")
        require(type(loss) is int and loss in (0, 1, 2), "loss must be integer 0, 1, or 2")
        semantic_loss = {
            ("ranked_in_y", True): 0,
            ("abstain", False): 0,
            ("abstain", True): 1,
            ("ranked_not_in_y", True): 2,
            ("ranked_any_skill", False): 2,
            ("unavailable_operational_failure", None): 2,
        }
        key = (ex.get("decision"), ex.get("y_nonempty"))
        require(key in semantic_loss and loss == semantic_loss[key], "loss contradicts its decision and acceptable set")
        require(approx(float(norm), float(loss) / 2.0), "normalized_y must equal loss/2")

    counter = expected.get("always_abstain_counterexample", {})
    cohort = counter.get("cohort", [])
    unique_ids(cohort, "case_id")
    require(cohort, "always_abstain cohort required")
    for row in cohort:
        require(type(row.get("y_nonempty")) is bool, "counterexample labels must be booleans")
        for field in ("always_abstain_loss", "candidate_policy_loss"):
            require(type(row.get(field)) is int and row[field] in (0, 1, 2),
                    "counterexample losses must be integers in 0/1/2")
        require(row["y_nonempty"] or row["candidate_policy_loss"] != 1,
                "no-match case cannot carry a false-abstention loss")
    always_total = sum(int(x["always_abstain_loss"]) for x in cohort)
    candidate_total = sum(int(x["candidate_policy_loss"]) for x in cohort)
    n = len(cohort)
    got = counter.get("expected", {})
    require(always_total == got.get("always_abstain_total_loss"), "always-abstain total mismatch")
    require(candidate_total == got.get("candidate_policy_total_loss"), "candidate policy total mismatch")
    require(approx(always_total / n, float(got.get("always_abstain_mean_loss")), 1e-10), "always-abstain mean mismatch")
    require(approx((always_total / n) / 2.0, float(got.get("always_abstain_mean_normalized_loss")), 1e-10), "always-abstain normalized mean mismatch")
    require(approx(candidate_total / n, float(got.get("candidate_policy_mean_loss")), 1e-10), "candidate mean mismatch")
    require(approx(candidate_total / n / 2, float(got.get("candidate_policy_mean_normalized_loss")), 1e-10), "candidate normalized mean mismatch")
    require(all(x["always_abstain_loss"] == (1 if x["y_nonempty"] else 0) for x in cohort), "always-abstain semantics mismatch")
    require(always_total > candidate_total, "always-abstain counterexample must make always abstain worse")

    coverage = expected.get("coverage_examples", [])
    require(bool(coverage), "coverage examples required")
    for ex in coverage:
        acceptable = set(ex["acceptable_y"])
        retrieved = set(ex["retrieved"])
        require(bool(acceptable), "positive coverage example must have acceptable skills")
        intersection = acceptable & retrieved
        require(ex["candidate_coverage_success"] is bool(intersection), "coverage success mismatch")
        require(approx(float(ex["set_recall"]), len(intersection) / len(acceptable)), "set recall mismatch")

    missing = {x["example_id"]: x for x in expected.get("missing_failed_unstarted_examples", [])}
    require(len(missing) == 3, "all missing/failed/unstarted examples are required")
    replay = missing["missing_replay_for_lower_gate_policy"]
    require(replay["case_count"] == replay["available_complete_cases"] + replay["missing_replay_responses"]
            and replay["expected_status"] == "not_estimable", "missing replay accounting mismatch")
    timeouts = missing["incorrect_replaced_by_timeouts"]
    require(timeouts["expected_loss_for_attempted_operational_failures_each"] == 2
            and timeouts["expected_total_loss"] == 2 * timeouts["attempted_timeouts_without_response"], "timeout loss mismatch")
    batch = missing["batch_cap_unstarted"]
    require(batch["scheduled_cases"] == batch["started_cases"] + batch["unstarted_cases"]
            and batch["expected_status"] == "partial_unfinished", "unstarted case accounting mismatch")

    harm_examples = expected.get("harm_interval_examples", [])
    require(len(harm_examples) == 2, "both zero-event interval examples required")
    for ex in harm_examples:
        flagged = int(ex.get("flagged"))
        n = int(ex.get("n"))
        require(flagged == 0 and n > 0, "v1 examples expect zero flagged and n > 0")
        upper = -math.expm1(math.log(0.05) / n)
        require(round(upper, 6) == ex.get("expected_upper_bound_rounded_6"), "harm upper bound mismatch")
        require((upper <= 0.02) is ex.get("passes_2_percent_gate"), "harm pass flag mismatch")

    sep = expected.get("cohort_separation_examples", {})
    require(sep.get("relevance_holdout_minimum_primary_cases") == 300, "expected values must preserve 300 relevance cases")
    require(sep.get("controlled_harm_minimum_independent_task_family_pairs_for_zero_event_iid_claim") == 150, "expected values must preserve 150 harm families")
    require(sep.get("operational_fallback_minimum_invocations") == 500, "expected values must preserve 500 operational invocations")
    require(sep.get("must_not_conflate") is True, "cohorts must be marked non-conflatable")


def main() -> None:
    parser = argparse.ArgumentParser(description="Validate SkillRanker v1 evaluation policy fixtures.")
    parser.add_argument("--policy", type=Path, default=DEFAULT_POLICY)
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--expected", type=Path, default=DEFAULT_EXPECTED)
    args = parser.parse_args()

    policy = load_json(args.policy)
    cases = load_jsonl(args.cases)
    expected = load_json(args.expected)
    require(isinstance(policy, dict), "policy root must be object")
    require(isinstance(expected, dict), "expected root must be object")
    require(expected.get("policy_id") == policy.get("policy_id"), "expected values must identify this policy")

    validate_policy(policy)
    validate_cases(cases, policy)
    validate_expected(expected)
    print(f"validated evaluation policy fixtures: {len(cases)} synthetic cases")
    print("note: validation checks artifact consistency only; it is not benchmark, provider, or harness evidence")


if __name__ == "__main__":
    try:
        main()
    except (OSError, KeyError, TypeError, ValueError, AttributeError, OverflowError):
        fail("artifact could not be read or has an invalid contract shape")
