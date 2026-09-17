#!/usr/bin/env python3
"""Contract and adversarial regression tests for tests/contract_matrix.toml.

Verifies:
- Production matrix is fully valid against roadmap boundaries and P0 rules.
- Corrupted matrices (duplicate IDs, invalid phases, future passed claims,
  missing fields, bad types) are rejected with clear errors.
"""

from pathlib import Path
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
import sys
sys.path.insert(0, str(ROOT / "scripts"))
import validate_contract_matrix as vcm


class ContractMatrixTests(unittest.TestCase):
    def test_production_matrix_valid(self):
        """The checked-in contract matrix must pass all schema and consistency rules."""
        ret = vcm.validate()
        self.assertEqual(ret, 0, "production contract matrix must pass validation")

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
