#!/usr/bin/env python3
"""Contract and adversarial regression tests for tests/contract_matrix.toml.

Verifies:
- Source-backed executed references resolve; prospective planned references remain allowed.
- Corrupted matrices (duplicate IDs, invalid phases, future passed claims,
  missing fields, bad types) are rejected with clear errors.
"""

from pathlib import Path
import contextlib
import io
import json
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
import sys
sys.path.insert(0, str(ROOT / "scripts"))
import validate_contract_matrix as vcm


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
            issues.write_text('{"id": "fixture-owner"}\n', encoding="utf-8")
            matrix = root / "matrix.toml"
            matrix.write_text(f'''schema_version = 1
created_for_phase = "P0"
[[boundaries]]
id = "fixture"
owner_bead = "fixture-owner"
phase = "P0"
platforms = []
features = []
unit_property_tests = [{json.dumps(reference)}]
e2e_cases = []
assertion_ids = []
status = "{status}"
''', encoding="utf-8")
            with patch.multiple(vcm, ROOT=root, MATRIX_FILE=matrix, ISSUES_FILE=issues):
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

    def test_rejection_of_duplicate_boundary_id(self):
        """Duplicate boundary IDs must be rejected."""
        sample = """
schema_version = 1
created_for_phase = "P0"

[[boundaries]]
id = "duplicate_id"
owner_bead = "sr-roadmap-l1i.1.1"
phase = "P0"
title = "Boundary 1"
unit_property_tests = []
e2e_suite = "runner-smoke"
e2e_cases = []
assertion_ids = []
platforms = ["linux"]
features = []
status = "planned"

[[boundaries]]
id = "duplicate_id"
owner_bead = "sr-roadmap-l1i.1.2"
phase = "P0"
title = "Boundary 2"
unit_property_tests = []
e2e_suite = "runner-smoke"
e2e_cases = []
assertion_ids = []
platforms = ["linux"]
features = []
status = "planned"
"""
        with tempfile.NamedTemporaryFile("w", suffix=".toml") as tmp:
            tmp.write(sample)
            tmp.flush()
            orig = vcm.MATRIX_FILE
            try:
                vcm.MATRIX_FILE = Path(tmp.name)
                ret = vcm.validate()
                self.assertEqual(ret, 1, "validator must reject duplicate boundary IDs")
            finally:
                vcm.MATRIX_FILE = orig

    def test_rejection_of_future_passed_claim(self):
        """Falsely marking a future phase boundary as passed must be rejected."""
        sample = """
schema_version = 1
created_for_phase = "P0"

[[boundaries]]
id = "fake_p4_claim"
owner_bead = "sr-roadmap-l1i.5.1"
phase = "P4"
title = "Fake P4 Passed"
unit_property_tests = []
e2e_suite = "core-cli"
e2e_cases = []
assertion_ids = []
platforms = ["linux"]
features = []
status = "passed"
"""
        with tempfile.NamedTemporaryFile("w", suffix=".toml") as tmp:
            tmp.write(sample)
            tmp.flush()
            orig = vcm.MATRIX_FILE
            try:
                vcm.MATRIX_FILE = Path(tmp.name)
                ret = vcm.validate()
                self.assertEqual(ret, 1, "validator must reject future boundary marked passed at P0")
            finally:
                vcm.MATRIX_FILE = orig

    def test_rejection_of_unknown_owner_bead(self):
        """Owner beads not in .beads/issues.jsonl must be rejected."""
        sample = """
schema_version = 1
created_for_phase = "P0"

[[boundaries]]
id = "invented_bead"
owner_bead = "sr-roadmap-invented-bead-999"
phase = "P0"
title = "Invented Bead Boundary"
unit_property_tests = []
e2e_suite = "runner-smoke"
e2e_cases = []
assertion_ids = []
platforms = ["linux"]
features = []
status = "planned"
"""
        with tempfile.NamedTemporaryFile("w", suffix=".toml") as tmp:
            tmp.write(sample)
            tmp.flush()
            orig = vcm.MATRIX_FILE
            try:
                vcm.MATRIX_FILE = Path(tmp.name)
                ret = vcm.validate()
                self.assertEqual(ret, 1, "validator must reject unknown owner beads")
            finally:
                vcm.MATRIX_FILE = orig


if __name__ == "__main__":
    unittest.main()
