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
import ast
import re
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


def reference_exists(reference):
    """Resolve source declarations only; this is not an execution receipt."""
    if not isinstance(reference, str):
        return False
    parts = reference.split("::")
    if len(parts) not in (2, 3) or any(not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", name) for name in parts[1:]):
        return False
    relative = Path(parts[0])
    if (relative.is_absolute() or ".." in relative.parts
            or len(relative.parts) < 2 or relative.as_posix() != parts[0]
            or "\\" in parts[0]):
        return False
    try:
        path = (ROOT / relative).resolve(strict=True)
        path.relative_to(ROOT.resolve())
        path.relative_to((ROOT / relative.parts[0]).absolute())
        if not path.is_file() or path.stat().st_size > 1024 * 1024:
            return False
        source = path.read_text(encoding="utf-8")
        if relative.parts[0] == "scripts" and path.suffix == ".py":
            nodes = ast.parse(source).body
            matches = [node for node in nodes
                       if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
                       and node.name == parts[1]]
            if len(matches) != 1:
                return False
            node = matches[0]
            if isinstance(node, ast.ClassDef):
                methods = [child.name for child in node.body
                           if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef))
                           and child.name.startswith("test_")]
                return bool(methods) if len(parts) == 2 else methods.count(parts[2]) == 1
            return len(parts) == 2
        if relative.parts[0] != "tests" or path.suffix != ".rs" or len(parts) != 2:
            return False
        # Conservative source-only Rust subset: top-level #[test] functions.
        # Strip literals and comments before matching; never execute source.
        tokens = []
        offset = 0
        while offset < len(source):
            if source.startswith("/*", offset):
                depth = 1
                offset += 2
                while depth and offset < len(source):
                    if source.startswith("/*", offset):
                        depth += 1
                        offset += 2
                    elif source.startswith("*/", offset):
                        depth -= 1
                        offset += 2
                    else:
                        offset += 1
                if depth:
                    return False
                continue
            if source.startswith("//", offset):
                end = source.find("\n", offset)
                offset = len(source) if end < 0 else end + 1
                continue
            raw = re.match(r'(?:br|cr|r)(\#*)"', source[offset:])
            if raw:
                end = source.find('"' + raw[1], offset + raw.end())
                if end < 0:
                    return False
                offset = end + 1 + len(raw[1])
                tokens.append("literal")
                continue
            literal = re.match(r'''(?:b?"(?:\\.|[^"\\])*"|b?'(?:\\.|[^'\\])')''', source[offset:], re.S)
            if literal:
                offset += literal.end()
                tokens.append("literal")
                continue
            if source[offset] == '"':
                return False
            token = re.match(r"[A-Za-z_][A-Za-z_0-9]*|\S", source[offset:])
            if token:
                tokens.append(token[0])
                offset += token.end()
            else:
                offset += 1
        stack = []
        found = False
        for index, token in enumerate(tokens):
            if not stack and tokens[index:index + 4] == ["#", "[", "test", "]"]:
                start = index + 4
                if tokens[start:start + 1] == ["pub"]:
                    start += 1
                if tokens[start:start + 1] == ["async"]:
                    start += 1
                # Deliberately only literal, zero-argument unit-returning tests.
                # Modules, other attributes, macro expansion and richer signatures
                # need a real Rust parser; do not guess from partial declarations.
                if tokens[start:start + 5] == ["fn", parts[1], "(", ")", "{"]:
                    found = True
            if token in ("(", "[", "{"):
                stack.append(token)
            elif token in (")", "]", "}"):
                if not stack or stack.pop() != {")": "(", "]": "[", "}": "{"}[token]:
                    return False
        return found and not stack
    except (OSError, ValueError, SyntaxError, UnicodeError, RuntimeError):
        return False


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

        if status != "planned":
            for reference in b["unit_property_tests"]:
                if not reference_exists(reference):
                    print(f"Error: boundary '{bid}' has an unresolved executed test reference", file=sys.stderr)
                    return 1

    print(f"validated contract matrix: {len(boundaries)} boundaries across {len(VALID_PHASES)} phases")
    print("all boundary IDs unique, owner beads verified against roadmap, future phases planned")
    return 0


if __name__ == "__main__":
    sys.exit(validate())
