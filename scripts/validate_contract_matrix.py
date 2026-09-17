#!/usr/bin/env python3
"""Validate reviewed coverage declarations; optionally verify mechanics receipts.

Declaration validation is never a test-execution or product acceptance receipt.
The separate authority file is reviewed policy, not generated runtime evidence.
"""

from pathlib import Path
import ast
import argparse
import os
import re
import json
import stat
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
MATRIX_FILE = ROOT / "tests/contract_matrix.toml"
ISSUES_FILE = ROOT / ".beads/issues.jsonl"
AUTHORITY_FILE = ROOT / "tests/contract_authority.toml"
MAX_DOCUMENT = 4 * 1024 * 1024
BOUND_FIELDS = ("owner_bead", "phase", "title", "member_beads", "platforms", "features",
                "e2e_suite", "e2e_cases", "assertion_ids")
# A pure library or check-script contract has no end-to-end suite of its own;
# borrowing unrelated mechanics cases would misstate its coverage.
NOT_APPLICABLE_SUITE = "not-applicable"
# Reviewed check-script entrypoints whose execution is the check itself. Any
# other plain function is a helper, not test evidence.
TRUSTED_CHECK_ENTRYPOINTS = frozenset({
    "scripts/check_dependency_graph.py::main",
    "scripts/validate_public_contracts.py::main",
    "scripts/validate_contract_matrix.py::main",
})

VALID_PHASES = {"P0", "P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9"}
VALID_STATUSES = {"planned", "executed", "passed", "failed"}


class InvalidMatrix(ValueError):
    """Fixed diagnostic codes avoid copying file contents into logs."""


def require(condition, code):
    if not condition:
        raise InvalidMatrix(code)


def read_document(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_size <= MAX_DOCUMENT, "document-file")
        value = stream.read(MAX_DOCUMENT + 1)
        require(len(value) <= MAX_DOCUMENT, "document-limit")
        return value.decode("utf-8")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate-json-key")
        result[key] = value
    return result


def load_beads():
    beads = {}
    for line in read_document(ISSUES_FILE).splitlines():
        if not line.strip():
            continue
        value = json.loads(line, object_pairs_hook=unique_object)
        require(type(value) is dict and type(value.get("id")) is str
                and value["id"] and value["id"] not in beads, "bead-record")
        beads[value["id"]] = value
    require(bool(beads), "empty-bead-inventory")
    return beads


def strings(value, *, empty=False):
    require(type(value) is list and (empty or bool(value)) and len(value) <= 1024,
            "selection-shape")
    require(all(type(item) is str and 0 < len(item) <= 256
                and item.isascii() and all(32 < ord(char) < 127 for char in item)
                for item in value), "selection-value")
    require(len(set(value)) == len(value), "duplicate-selection")
    return value


def indexed_boundaries(document, *, authority=False):
    require(type(document) is dict and set(document) == {"schema_version", "created_for_phase", "boundaries"},
            "document-fields")
    require(type(document["schema_version"]) is int and document["schema_version"] == 1, "schema-version")
    require(type(document["created_for_phase"]) is str
            and document["created_for_phase"] in VALID_PHASES, "created-for-phase")
    rows = document["boundaries"]
    require(type(rows) is list and 0 < len(rows) <= 1024, "empty-boundary-inventory")
    result = {}
    fields = {"id", *BOUND_FIELDS} | (set() if authority else {"unit_property_tests", "status"})
    for row in rows:
        require(type(row) is dict and set(row) == fields, "boundary-fields")
        require(type(row["id"]) is str and re.fullmatch(r"[a-z][a-z0-9_]{0,95}", row["id"]), "boundary-id")
        require(row["id"] not in result, "duplicate-boundary")
        require(type(row["phase"]) is str and row["phase"] in VALID_PHASES, "boundary-phase")
        require(type(row["owner_bead"]) is str and bool(row["owner_bead"]), "boundary-owner")
        require(type(row["e2e_suite"]) is str and re.fullmatch(r"[a-z][a-z0-9-]{0,63}", row["e2e_suite"]), "boundary-suite")
        require(type(row["title"]) is str and 0 < len(row["title"]) <= 512
                and row["title"].isprintable(), "boundary-title")
        not_applicable = row["e2e_suite"] == NOT_APPLICABLE_SUITE
        for field in ("member_beads", "platforms", "features", "e2e_cases", "assertion_ids"):
            strings(row[field], empty=field == "features"
                    or (not_applicable and field in ("e2e_cases", "assertion_ids")))
        if not_applicable:
            require(row["phase"] == "P0", "not-applicable-phase")
            require(row["e2e_cases"] == [] and row["assertion_ids"] == [], "not-applicable-selection")
        if not authority:
            strings(row["unit_property_tests"])
            require(type(row["status"]) is str and row["status"] in VALID_STATUSES, "boundary-status")
        result[row["id"]] = row
    return result


def mechanics_catalogs():
    # These are trusted repository drivers. The child-scenario JSON is not the
    # outer certificate's catalog and cannot confer authority on invented IDs.
    sys.path.insert(0, str(Path(__file__).resolve().parent / "e2e"))
    import evidence
    import runner_contract
    smoke = evidence.manifest(evidence.read_json(ROOT / "scripts/e2e/suites/runner-smoke.json"))
    return {
        "runner-smoke": {case["id"]: set(case["assertions"]) for case in smoke["cases"]},
        "runner-contract": {name: {"satisfied"} for name in runner_contract.CHECKS},
    }


def is_unittest_case(node, module_nodes):
    """Accept only a direct unittest.TestCase base bound by an unaliased import.

    Source-only and deliberately conservative: aliases, indirect bases and
    rebinding are rejected rather than resolved.
    """
    bindings = {}
    for statement in module_nodes:
        names = []
        if isinstance(statement, ast.Import):
            names = [(alias.asname or alias.name.split(".")[0],
                      "module:unittest" if alias.name == "unittest" and alias.asname is None else "other")
                     for alias in statement.names]
        elif isinstance(statement, ast.ImportFrom):
            names = [(alias.asname or alias.name,
                      "class:TestCase" if statement.module == "unittest" and statement.level == 0
                      and alias.name == "TestCase" and alias.asname is None else "other")
                     for alias in statement.names]
        elif isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            names = [(statement.name, "other")]
        elif isinstance(statement, (ast.Assign, ast.AnnAssign, ast.AugAssign)):
            targets = statement.targets if isinstance(statement, ast.Assign) else [statement.target]
            names = [(name.id, "other") for target in targets for name in ast.walk(target)
                     if isinstance(name, ast.Name)]
        for name, origin in names:
            bindings.setdefault(name, []).append(origin)
    for base in node.bases:
        if (isinstance(base, ast.Attribute) and base.attr == "TestCase"
                and isinstance(base.value, ast.Name) and bindings.get(base.value.id) == ["module:unittest"]):
            return True
        if isinstance(base, ast.Name) and base.id == "TestCase" and bindings.get("TestCase") == ["class:TestCase"]:
            return True
    return False


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
                if not is_unittest_case(node, nodes):
                    return False
                methods = {}
                for child in node.body:
                    if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        methods[child.name] = child.name.startswith("test_")
                    elif isinstance(child, (ast.Assign, ast.AnnAssign, ast.AugAssign, ast.Delete)):
                        if isinstance(child, ast.AnnAssign) and child.value is None:
                            continue
                        targets = child.targets if isinstance(child, (ast.Assign, ast.Delete)) else [child.target]
                        for target in targets:
                            for name in ast.walk(target):
                                if isinstance(name, ast.Name):
                                    methods[name.id] = False
                    elif isinstance(child, ast.ClassDef):
                        methods[child.name] = False
                return any(methods.values()) if len(parts) == 2 else methods.get(parts[2], False)
            return (len(parts) == 2 and isinstance(node, ast.FunctionDef)
                    and reference in TRUSTED_CHECK_ENTRYPOINTS)
        # Cargo discovers integration tests only as direct children of tests/.
        if (relative.parts[0] != "tests" or len(relative.parts) != 2
                or path.suffix != ".rs" or len(parts) != 2):
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
        # What ended the previous top-level construct: an item boundary, an
        # inner attribute, an outer attribute, or something else.
        previous = "start"
        for index, token in enumerate(tokens):
            if not stack and tokens[index:index + 4] == ["#", "[", "test", "]"] \
                    and previous in ("start", "item", "inner-attribute"):
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
                kind = None
                if token == "[" and not stack and index >= 1 and tokens[index - 1] == "#":
                    kind = "outer-attribute"
                elif token == "[" and not stack and tokens[max(index - 2, 0):index] == ["#", "!"]:
                    kind = "inner-attribute"
                stack.append((token, kind))
            elif token in (")", "]", "}"):
                if not stack:
                    return False
                opener, kind = stack.pop()
                if opener != {")": "(", "]": "[", "}": "{"}[token]:
                    return False
                if not stack:
                    previous = kind or ("item" if token == "}" else "other")
            elif not stack:
                if token == ";":
                    previous = "item"
                elif token not in ("#", "!"):
                    previous = "other"
        return found and not stack
    except (OSError, ValueError, SyntaxError, UnicodeError, RuntimeError):
        return False


def validate_documents(data, authority, beads, catalogs):
    rows = indexed_boundaries(data)
    expected = indexed_boundaries(authority, authority=True)
    require(data["created_for_phase"] == authority["created_for_phase"], "authority-phase")
    require(set(rows) == set(expected), "boundary-coverage")
    required_beads = {key for key, value in beads.items()
                      if key.startswith("sr-roadmap-l1i.") and value.get("issue_type") != "epic"}
    require(bool(required_beads), "empty-roadmap-inventory")
    covered = set()
    for bid, row in rows.items():
        for field in BOUND_FIELDS:
            require(row[field] == expected[bid][field], "authority-" + field)
        members = row["member_beads"]
        require(row["owner_bead"] in members, "owner-not-member")
        require(set(members) <= required_beads and not (set(members) & covered), "member-coverage")
        for member in members:
            match = re.fullmatch(r"sr-roadmap-l1i\.(\d+)(?:\.\d+)+", member)
            require(match is not None and int(match[1]) - 1 == int(row["phase"][1:]), "member-phase")
        covered.update(members)
        future = int(row["phase"][1:]) > int(data["created_for_phase"][1:])
        require(not future or row["status"] == "planned", "future-execution")
        require(row["phase"] == "P0" or len(members) == 1 or row["status"] == "planned",
                "expand-product-aggregate-before-execution")
        if row["status"] != "planned":
            require(all(reference_exists(ref) for ref in row["unit_property_tests"]), "unresolved-test-reference")
            require(row["e2e_suite"] in catalogs or row["e2e_suite"] == NOT_APPLICABLE_SUITE,
                    "unregistered-executed-suite")
        if row["e2e_suite"] in catalogs:
            catalog = catalogs[row["e2e_suite"]]
            for case in row["e2e_cases"]:
                require(case in catalog, "unknown-case")
                require(set(row["assertion_ids"]) <= catalog[case], "unknown-assertion")
    require(covered == required_beads, "roadmap-coverage")
    return rows


def validate_mechanics_receipts(rows, receipts, *, required_tier="runner-mechanics"):
    """Recheck complete mechanics reports against this checkout and interpreter.

    This deliberately cannot attest Rust tests or a product gate. Unit execution
    needs its own command and build evidence in the phase acceptance review.
    """
    require(required_tier == "runner-mechanics", "unsupported-evidence-tier")
    require(set(receipts) == {"runner-smoke", "runner-contract"}, "missing-suite-receipt")
    require(all(isinstance(path, Path) for path in receipts.values()), "receipt-path")
    sys.path.insert(0, str(Path(__file__).resolve().parent / "e2e"))
    import evidence
    import runner
    import runner_contract
    binary = Path(sys.executable).resolve()
    expected = runner.identity(binary)
    certificate_identity = runner.identity(binary, runner_contract.FIXTURE)
    smoke = evidence.manifest(evidence.read_json(ROOT / "scripts/e2e/suites/runner-smoke.json"))
    try:
        results = {
            "runner-smoke": evidence.validate(receipts["runner-smoke"], smoke, expected),
            "runner-contract": runner_contract.validate_certificate(receipts["runner-contract"], certificate_identity),
        }
    except evidence.InvalidEvidence:
        raise InvalidMatrix("incompatible-suite-receipt") from None
    for result in results.values():
        require(result["runner_status"] == "passed" and result["product_gate"] == "not-applicable",
                "incomplete-suite-receipt")
    for row in rows.values():
        if row["phase"] == "P0" and row["e2e_suite"] != NOT_APPLICABLE_SUITE:
            require(row["e2e_suite"] in results and row["platforms"] == [expected["platform"]]
                    and row["features"] == expected["features"], "incompatible-boundary-receipt")
    require(expected == runner.identity(binary)
            and certificate_identity == runner.identity(binary, runner_contract.FIXTURE), "source-changed")


def validate(*, receipts=None):
    try:
        data = tomllib.loads(read_document(MATRIX_FILE))
        authority = tomllib.loads(read_document(AUTHORITY_FILE))
        rows = validate_documents(data, authority, load_beads(), mechanics_catalogs())
        if receipts is not None:
            validate_mechanics_receipts(rows, receipts)
    except InvalidMatrix as error:
        print("invalid contract matrix: " + str(error), file=sys.stderr)
        return 1
    except (OSError, ValueError, TypeError, RecursionError, UnicodeError, subprocess.SubprocessError):
        print("invalid contract matrix: document-or-receipt", file=sys.stderr)
        return 1
    print(f"validated {len(rows)} coverage declarations; unit execution and product acceptance not certified")
    if receipts is not None:
        print("complete matching runner mechanics receipts verified; product gate not applicable")
    return 0


class QuietParser(argparse.ArgumentParser):
    def error(self, message):
        self.exit(2, "invalid contract matrix: arguments; use --help\n")


def main():
    parser = QuietParser(description=__doc__)
    parser.add_argument("--require-mechanics", action="store_true")
    parser.add_argument("--smoke-receipt", type=Path)
    parser.add_argument("--certificate-receipt", type=Path)
    args = parser.parse_args()
    receipts = None
    if args.require_mechanics or args.smoke_receipt is not None or args.certificate_receipt is not None:
        receipts = {}
        if args.smoke_receipt is not None:
            receipts["runner-smoke"] = args.smoke_receipt
        if args.certificate_receipt is not None:
            receipts["runner-contract"] = args.certificate_receipt
    return validate(receipts=receipts)


if __name__ == "__main__":
    sys.exit(main())
