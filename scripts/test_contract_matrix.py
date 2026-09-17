#!/usr/bin/env python3
"""Contract and adversarial regression tests for tests/contract_matrix.toml.

Verifies:
- Source-backed executed references resolve; prospective planned references remain allowed.
- Corrupted matrices (duplicate IDs, invalid phases, future passed claims,
  missing fields, bad types) are rejected with clear errors.
"""

from pathlib import Path
import copy
import subprocess
import sys
import contextlib
import io
import json
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import validate_contract_matrix as vcm  # noqa: E402


def authority_toml(data):
    lines = ['schema_version = 1', 'created_for_phase = "P0"']
    for row in data["boundaries"]:
        lines += ["[[boundaries]]"]
        lines += [key + " = " + json.dumps(row[key]) for key in ("id", *vcm.BOUND_FIELDS)]
    return "\n".join(lines) + "\n"


def declaration_fixture():
    # Two real roadmap IDs make the wrong-but-existing owner and omitted
    # aggregate member tests meaningful, with one complete successful twin.
    row = {"id": "fixture", "owner_bead": "sr-roadmap-l1i.1.1", "phase": "P0",
           "member_beads": ["sr-roadmap-l1i.1.1", "sr-roadmap-l1i.1.2"],
           "platforms": ["linux"], "features": [], "title": "Reviewed fixture",
           "e2e_suite": "runner-smoke", "e2e_cases": ["success"], "assertion_ids": ["behavior"],
           "unit_property_tests": ["scripts/test_contract_matrix.py::ContractMatrixTests"], "status": "executed"}
    data = {"schema_version": 1, "created_for_phase": "P0", "boundaries": [row]}
    authority = tomllib.loads(authority_toml(data))
    beads = {name: {"issue_type": "task"} for name in row["member_beads"]}
    return data, authority, beads, {"runner-smoke": {"success": {"behavior"}}}


class ContractMatrixTests(unittest.TestCase):
    def validate_fixture(self, reference, status="executed", files=None, escape=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "repo"
            root.mkdir()
            for name, source in (files or {}).items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(source, encoding="utf-8")
            if escape:
                outside = Path(temporary) / "outside.py"
                outside.write_text("def test_real(): pass\n", encoding="utf-8")
                (root / "scripts").mkdir(exist_ok=True)
                (root / "scripts/escape.py").symlink_to(outside)
            issues = root / "issues.jsonl"
            issues.write_text('{"id":"sr-roadmap-l1i.1.1","issue_type":"task"}\n', encoding="utf-8")
            matrix = root / "matrix.toml"
            matrix.write_text(f'''schema_version = 1
created_for_phase = "P0"
[[boundaries]]
id = "fixture"
owner_bead = "sr-roadmap-l1i.1.1"
member_beads = ["sr-roadmap-l1i.1.1"]
title = "Source declaration resolution"
phase = "P0"
platforms = ["linux"]
features = []
unit_property_tests = [{json.dumps(reference)}]
e2e_suite = "runner-smoke"
e2e_cases = ["success"]
assertion_ids = ["behavior"]
status = "{status}"
''', encoding="utf-8")
            authority = root / "authority.toml"
            authority.write_text(authority_toml(tomllib.loads(matrix.read_text())), encoding="utf-8")
            suite = root / "scripts/e2e/suites/runner-smoke.json"
            suite.parent.mkdir(parents=True, exist_ok=True)
            suite.write_bytes((ROOT / "scripts/e2e/suites/runner-smoke.json").read_bytes())
            with patch.multiple(vcm, ROOT=root, MATRIX_FILE=matrix, ISSUES_FILE=issues,
                                AUTHORITY_FILE=authority):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    return vcm.validate()

    def test_real_source_declarations_accepted(self):
        sources = {
            "scripts/check.py": '''raise RuntimeError("must never import this module")
def check_contract(): pass
class Contract:
    def test_behavior(self): pass
''',
            "tests/contract.rs": "#[test]\nfn real_contract() {}\n",
        }
        for reference in ("scripts/check.py::check_contract", "scripts/check.py::Contract",
                          "scripts/check.py::Contract::test_behavior", "tests/contract.rs::real_contract"):
            with self.subTest(reference=reference):
                self.assertEqual(self.validate_fixture(reference, files=sources), 0)

    def test_missing_executed_references_rejected(self):
        sources = {"scripts/check.py": "def real(): pass\n", "tests/contract.rs": "#[test]\nfn real() {}\n"}
        for reference in ("scripts/missing.py::real", "scripts/check.py::missing",
                          "tests/missing.rs::real", "tests/contract.rs::missing"):
            for status in ("executed", "passed", "failed"):
                with self.subTest(reference=reference, status=status):
                    self.assertEqual(self.validate_fixture(reference, status, sources), 1)

    def test_non_declarations_cannot_establish_source_evidence(self):
        cases = [
            ("scripts/check.py::fake", "# def fake(): pass\n"),
            ("scripts/check.py::fake", 'text = "def fake(): pass"\n'),
            ("scripts/check.py::Empty", "class Empty: pass\n"),
            ("scripts/check.py::Contract::missing", "class Contract:\n    def test_real(self): pass\n"),
            ("scripts/check.py::nested", "def outer():\n    def nested(): pass\n"),
            ("tests/check.rs::fake", "// #[test]\n// fn fake() {}\n"),
            ("tests/check.rs::fake", "/* nested /* comment */ #[test] fn fake() {} */"),
            ("tests/check.rs::fake", 'const TEXT: &str = "#[test] fn fake() {}";'),
            ("tests/check.rs::fake", 'const TEXT: &str = r##"#[test] fn fake() {}"##;'),
            ("tests/check.rs::fake", "fn fake() {}"),
            ("tests/check.rs::fake", "#[test] fn fake();"),
            ("tests/check.rs::fake", "tokens!(#[test] fn fake() {});"),
            ("tests/check.rs::fake", "#[other(#[test] fn fake() {})] fn actual() {}"),
        ]
        for reference, source in cases:
            with self.subTest(reference=reference, source=source):
                self.assertEqual(self.validate_fixture(reference, files={reference.split("::")[0]: source}), 1)

    def test_malformed_and_escaping_references_rejected(self):
        for reference in (7, "", "scripts/check.py", "scripts/check.py::", "scripts/check.py::real()",
                          "tests/check.rs::module::real", "scripts/../outside.py::real",
                          "/scripts/check.py::real", "other/check.py::real", "scripts/check.txt::real"):
            with self.subTest(reference=reference):
                self.assertEqual(self.validate_fixture(reference), 1)
        self.assertEqual(self.validate_fixture("scripts/escape.py::test_real", escape=True), 1)
        self.assertEqual(self.validate_fixture("scripts/directory.py::real", files={"scripts/directory.py/child": ""}), 1)

    def test_planned_future_references_need_not_exist(self):
        for reference in ("scripts/future.py::FutureContract::test_future", "tests/future.rs::future"):
            with self.subTest(reference=reference):
                self.assertEqual(self.validate_fixture(reference, status="planned"), 0)

    def test_all_p0_boundaries_covered(self):
        """Matrix must include all foundational P0 boundaries."""
        with (ROOT / "tests/contract_matrix.toml").open("rb") as f:
            data = tomllib.load(f)
        p0_ids = {b["id"] for b in data["boundaries"] if b["phase"] == "P0"}
        expected_p0 = {
            "p0_bootstrap",
            "p0_identity_provenance",
            "p0_output_schemas",
            "p0_resource_limits",
            "p0_config_authority",
            "p0_adapter_contracts",
            "p0_dependency_slices",
            "p0_eval_policy",
            "p0_process_engine",
            "p0_evidence_scaffolding",
            "p0_adversarial_certification",
            "p0_contract_reconciliation",
            "p0_acceptance_gate",
        }
        for exp in expected_p0:
            self.assertIn(exp, p0_ids, f"missing expected P0 boundary '{exp}'")

    def test_future_phases_must_be_planned(self):
        """No future boundary (P1-P9) may be claimed as executed or passed in P0."""
        with (ROOT / "tests/contract_matrix.toml").open("rb") as f:
            data = tomllib.load(f)
        for b in data["boundaries"]:
            if b["phase"] != "P0":
                self.assertEqual(
                    b["status"],
                    "planned",
                    f"boundary '{b['id']}' in phase '{b['phase']}' cannot have status '{b['status']}' at P0",
                )

    def test_reviewed_complete_inventory_accepts_honest_twin(self):
        data, authority, beads, catalogs = declaration_fixture()
        with patch.object(vcm, "ROOT", ROOT):
            self.assertEqual(set(vcm.validate_documents(data, authority, beads, catalogs)), {"fixture"})

    def test_mutated_declarations_rejected_for_specific_reason(self):
        # Expectations are independent fixed codes, not calculated from the validator.
        cases = [
            ("owner", lambda d: d["boundaries"][0].update(owner_bead="sr-roadmap-l1i.1.2"), "authority-owner_bead"),
            ("suite", lambda d: d["boundaries"][0].update(e2e_suite="invented"), "authority-e2e_suite"),
            ("case", lambda d: d["boundaries"][0].update(e2e_cases=["invented"]), "authority-e2e_cases"),
            ("assertion", lambda d: d["boundaries"][0].update(assertion_ids=["invented"]), "authority-assertion_ids"),
            ("empty", lambda d: d["boundaries"][0].update(e2e_cases=[]), "selection-shape"),
            ("duplicate", lambda d: d["boundaries"][0].update(e2e_cases=["success", "success"]), "duplicate-selection"),
            ("no-unit", lambda d: d["boundaries"][0].update(unit_property_tests=[]), "selection-shape"),
            ("bad-platform", lambda d: d["boundaries"][0].update(platforms=["darwin"]), "authority-platforms"),
            ("bad-feature", lambda d: d["boundaries"][0].update(features=["tui"]), "authority-features"),
            ("missing-member", lambda d: d["boundaries"][0].update(member_beads=["sr-roadmap-l1i.1.1"]), "authority-member_beads"),
            ("boolean-version", lambda d: d.update(schema_version=True), "schema-version"),
            ("invalid-phase", lambda d: d["boundaries"][0].update(phase=[]), "boundary-phase"),
            ("invalid-status", lambda d: d["boundaries"][0].update(status={}), "boundary-status"),
            ("non-string-selection", lambda d: d["boundaries"][0].update(e2e_cases=[7]), "selection-value"),
            ("duplicate-row", lambda d: d["boundaries"].append(copy.deepcopy(d["boundaries"][0])), "duplicate-boundary"),
            ("missing-field", lambda d: d["boundaries"][0].pop("features"), "boundary-fields"),
            ("private-extra", lambda d: d["boundaries"][0].update(private="SYNTHETIC_PRIVATE_VALUE"), "boundary-fields"),
        ]
        for name, mutate, expected in cases:
            with self.subTest(case=name):
                data, authority, beads, catalogs = declaration_fixture()
                mutate(data)
                with self.assertRaisesRegex(vcm.InvalidMatrix, "^" + expected + "$"):
                    vcm.validate_documents(data, authority, beads, catalogs)

    def test_authority_cannot_omit_required_rows_or_roadmap_members(self):
        for mode, expected in (("missing-row", "boundary-coverage"),
                               ("empty-authority", "empty-boundary-inventory"),
                               ("missing-member", "roadmap-coverage"),
                               ("wrong-phase", "member-phase"),
                               ("wrong-owner", "owner-not-member")):
            with self.subTest(mode=mode):
                data, authority, beads, catalogs = declaration_fixture()
                if mode == "missing-row":
                    extra = copy.deepcopy(authority["boundaries"][0])
                    extra["id"] = "required_extra"
                    authority["boundaries"].append(extra)
                elif mode == "empty-authority":
                    authority["boundaries"] = []
                else:
                    for document in (data, authority):
                        row = document["boundaries"][0]
                        if mode == "missing-member":
                            row["member_beads"] = ["sr-roadmap-l1i.1.1"]
                        elif mode == "wrong-phase":
                            row["phase"] = "P1"
                            document["created_for_phase"] = "P1"
                        else:
                            row["owner_bead"] = "sr-roadmap-l1i.1.3"
                with self.assertRaisesRegex(vcm.InvalidMatrix, "^" + expected + "$"):
                    vcm.validate_documents(data, authority, beads, catalogs)

    def test_real_catalog_overrides_invented_authority_cases(self):
        for field, value, code in (("e2e_suite", "invented", "unregistered-executed-suite"),
                                   ("e2e_cases", ["invented"], "unknown-case"),
                                   ("assertion_ids", ["invented"], "unknown-assertion")):
            data, authority, beads, catalogs = declaration_fixture()
            for document in (data, authority):
                document["boundaries"][0][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(vcm.InvalidMatrix, "^" + code + "$"):
                vcm.validate_documents(data, authority, beads, catalogs)

    def test_future_execution_cannot_be_authorized_by_matrix(self):
        data, authority, beads, catalogs = declaration_fixture()
        for document in (data, authority):
            row = document["boundaries"][0]
            row.update(phase="P1", owner_bead="sr-roadmap-l1i.2.1", member_beads=["sr-roadmap-l1i.2.1"])
        beads = {"sr-roadmap-l1i.2.1": {"issue_type": "task"}}
        with self.assertRaisesRegex(vcm.InvalidMatrix, "^future-execution$"):
            vcm.validate_documents(data, authority, beads, catalogs)
        data["boundaries"][0]["status"] = "planned"
        self.assertEqual(len(vcm.validate_documents(data, authority, beads, catalogs)), 1)

    def test_missing_empty_malformed_and_duplicate_bead_inventory_rejected(self):
        for content in (None, "", "{", "[]", '{"id":"a","id":"b"}', '{"id":"a"}\n{"id":"a"}\n'):
            with tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "issues.jsonl"
                if content is not None:
                    path.write_text(content)
                with patch.object(vcm, "ISSUES_FILE", path), self.subTest(content=content):
                    with self.assertRaises((vcm.InvalidMatrix, OSError, ValueError)):
                        vcm.load_beads()

    def test_missing_receipts_and_fixture_interpreter_substitution_rejected(self):
        with self.assertRaisesRegex(vcm.InvalidMatrix, "^missing-suite-receipt$"):
            vcm.validate_mechanics_receipts({}, {})
        with self.assertRaisesRegex(vcm.InvalidMatrix, "^unsupported-evidence-tier$"):
            vcm.validate_mechanics_receipts({}, {}, required_tier="rust-product")

    def test_checked_in_matrix_and_fail_closed_cli(self):
        command = [sys.executable, "-I", "-B", str(ROOT / "scripts/validate_contract_matrix.py")]
        good = subprocess.run(command, capture_output=True, text=True, timeout=20, check=False)
        self.assertEqual(good.returncode, 0, good.stderr)
        self.assertIn("unit execution and product acceptance not certified", good.stdout)
        missing = subprocess.run(command + ["--require-mechanics"], capture_output=True, text=True, timeout=20, check=False)
        self.assertEqual(missing.returncode, 1)
        self.assertIn("missing-suite-receipt", missing.stderr)

    def test_cli_usage_errors_do_not_echo_private_arguments(self):
        command = [sys.executable, "-I", "-B", str(ROOT / "scripts/validate_contract_matrix.py")]
        canary = "SYNTHETIC_PRIVATE_MATRIX_ARGUMENT"
        for arguments in (["--unknown", canary], [canary], ["--smoke-receipt"]):
            result = subprocess.run(command + arguments, capture_output=True, text=True,
                                    timeout=20, check=False)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, "")
            self.assertEqual(result.stderr, "invalid contract matrix: arguments; use --help\n")
        help_result = subprocess.run(command + ["--help"], capture_output=True, text=True,
                                     timeout=20, check=False)
        self.assertEqual(help_result.returncode, 0)
        self.assertIn("--require-mechanics", help_result.stdout)

    def test_receipt_gate_requires_real_matching_complete_reports(self):
        # Retain all real runs and mutations; run this in a frozen checkout.
        # A complete accepted twin prevents an always-reject validator passing.
        import hashlib
        sys.path.insert(0, str(ROOT / "scripts/e2e"))
        import evidence
        parent = Path(tempfile.mkdtemp(prefix="sr-matrix-receipts-"))
        receipts = {}
        for suite, pattern in (("runner-smoke", "sr-e2e-*"), ("runner-contract", "sr-contract-*")):
            result = subprocess.run([str(ROOT / "scripts/e2e/run.sh"), "--suite", suite,
                                     "--artifacts", str(parent)], capture_output=True,
                                    timeout=150, check=False)
            (parent / (suite + ".log")).write_bytes(result.stdout + result.stderr)
            self.assertEqual(result.returncode, 0, f"real {suite} failed; retained at {parent}")
            matches = list(parent.glob(pattern))
            self.assertEqual(len(matches), 1)
            receipts[suite] = matches[0]
        data, authority, beads, catalogs = declaration_fixture()
        rows = vcm.validate_documents(data, authority, beads, catalogs)
        vcm.validate_mechanics_receipts(rows, receipts)
        original = receipts["runner-smoke"]
        for field, value in (("source_sha256", "0" * 64), ("binary_sha256", "0" * 64),
                             ("lock_sha256", "0" * 64), ("platform", "darwin"),
                             ("features", ["tui"]), ("binary_role", "rust-product")):
            with self.subTest(identity_field=field):
                candidate = Path(tempfile.mkdtemp(prefix="mismatch-", dir=parent))
                events = [evidence.decode(line) for line in (original / "events.jsonl").read_bytes().splitlines()]
                events[0]["identity"][field] = value
                raw = b"".join(evidence.encode(event) for event in events)
                summary = evidence.read_json(original / "summary.json")
                summary["events_sha256"] = hashlib.sha256(raw).hexdigest()
                (candidate / "events.jsonl").write_bytes(raw)
                (candidate / "summary.json").write_bytes(evidence.encode(summary))
                with self.assertRaisesRegex(vcm.InvalidMatrix, "^incompatible-suite-receipt$"):
                    vcm.validate_mechanics_receipts(rows, {**receipts, "runner-smoke": candidate})
        partial = subprocess.run([str(ROOT / "scripts/e2e/run.sh"), "--suite", "runner-smoke",
                                  "--case", "success", "--artifacts", str(parent)],
                                 capture_output=True, timeout=30, check=False)
        self.assertEqual(partial.returncode, 3)
        partial_dir = next(path for path in parent.glob("sr-e2e-*") if path != original)
        with self.assertRaisesRegex(vcm.InvalidMatrix, "^incomplete-suite-receipt$"):
            vcm.validate_mechanics_receipts(rows, {**receipts, "runner-smoke": partial_dir})
        absent = Path(tempfile.mkdtemp(prefix="absent-", dir=parent))
        with self.assertRaisesRegex(vcm.InvalidMatrix, "^incompatible-suite-receipt$"):
            vcm.validate_mechanics_receipts(rows, {**receipts, "runner-smoke": absent})
        # A valid report for a different required cell cannot satisfy it.
        for field, value in (("platforms", ["darwin"]), ("features", ["tui"])):
            mismatched = copy.deepcopy(rows)
            mismatched["fixture"][field] = value
            with self.assertRaisesRegex(vcm.InvalidMatrix, "^incompatible-boundary-receipt$"):
                vcm.validate_mechanics_receipts(mismatched, receipts)
        vcm.validate_mechanics_receipts(rows, receipts)


if __name__ == "__main__":
    unittest.main()
