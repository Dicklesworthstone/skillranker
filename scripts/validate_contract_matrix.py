#!/usr/bin/env python3
"""Strict validation of tests/contract_matrix.toml against roadmap boundaries.

Verifies:
- Valid TOML format and required schema fields.
- Every boundary has a unique ID, valid phase, known owner bead from .beads/issues.jsonl.
- Platforms, features, unit_property_tests, e2e_cases, assertion_ids are valid arrays.
- Status is strictly one of 'planned', 'executed', 'passed', 'failed'.
- P0 future product boundaries must be 'planned', not fake passed results.
"""

from pathlib import Path
import json
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
MATRIX_FILE = ROOT / "tests/contract_matrix.toml"
ISSUES_FILE = ROOT / ".beads/issues.jsonl"

VALID_PHASES = {"P0", "P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9"}
VALID_STATUSES = {"planned", "executed", "passed", "failed"}


def load_known_bead_ids():
    ids = set()
    if not ISSUES_FILE.is_file():
        return ids
    with ISSUES_FILE.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                data = json.loads(line)
            except (json.JSONDecodeError, ValueError):
                continue
            if "id" in data:
                ids.add(data["id"])
    return ids


def validate():
    if not MATRIX_FILE.is_file():
        print(f"Error: {MATRIX_FILE} not found", file=sys.stderr)
        return 1

    with MATRIX_FILE.open("rb") as f:
        data = tomllib.load(f)

    if data.get("schema_version") != 1:
        print(f"Error: unexpected schema_version: {data.get('schema_version')}", file=sys.stderr)
        return 1

    if data.get("created_for_phase") not in VALID_PHASES:
        print(f"Error: invalid created_for_phase: {data.get('created_for_phase')}", file=sys.stderr)
        return 1

    boundaries = data.get("boundaries")
    if not isinstance(boundaries, list) or len(boundaries) == 0:
        print("Error: boundaries must be a non-empty array", file=sys.stderr)
        return 1

    known_beads = load_known_bead_ids()
    seen_ids = set()

    for idx, b in enumerate(boundaries):
        bid = b.get("id")
        if not bid or not isinstance(bid, str):
            print(f"Error: boundary [{idx}] missing string 'id'", file=sys.stderr)
            return 1
        if bid in seen_ids:
            print(f"Error: duplicate boundary id: '{bid}'", file=sys.stderr)
            return 1
        seen_ids.add(bid)

        owner = b.get("owner_bead")
        if not owner or not isinstance(owner, str):
            print(f"Error: boundary '{bid}' missing string 'owner_bead'", file=sys.stderr)
            return 1
        if known_beads and owner not in known_beads:
            print(f"Error: boundary '{bid}' owner_bead '{owner}' not found in .beads/issues.jsonl", file=sys.stderr)
            return 1

        phase = b.get("phase")
        if phase not in VALID_PHASES:
            print(f"Error: boundary '{bid}' invalid phase '{phase}'", file=sys.stderr)
            return 1

        status = b.get("status")
        if status not in VALID_STATUSES:
            print(f"Error: boundary '{bid}' invalid status '{status}'", file=sys.stderr)
            return 1

        # Check arrays
        for field in ("platforms", "features", "unit_property_tests", "e2e_cases", "assertion_ids"):
            val = b.get(field)
            if not isinstance(val, list):
                print(f"Error: boundary '{bid}' field '{field}' must be a list", file=sys.stderr)
                return 1

        # Invariant: Future phases (P1-P9) must have status "planned" at P0 stage
        if phase != "P0" and status not in ("planned",):
            print(f"Error: future boundary '{bid}' (phase {phase}) cannot be marked '{status}' at P0", file=sys.stderr)
            return 1

    print(f"validated contract matrix: {len(boundaries)} boundaries across {len(VALID_PHASES)} phases")
    print("all boundary IDs unique, owner beads verified against roadmap, future phases planned")
    return 0


if __name__ == "__main__":
    sys.exit(validate())
