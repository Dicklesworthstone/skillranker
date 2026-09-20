#!/usr/bin/env python3
"""Tests for the isolated probe driver; these do not execute or validate Rust."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("boundary", ROOT / "scripts/check_platform_boundary.py")
boundary = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(boundary)

LOCK = '''version = 4

[[package]]
name = "skillranker"
version = "0.1.0"
dependencies = ["rusqlite", "unrelated"]

[[package]]
name = "rusqlite"
version = "0.40.2"
source = "registry+example"
checksum = "sqlite-checksum"
dependencies = ["ffi 1.0.0", "shared"]

[[package]]
name = "ffi"
version = "1.0.0"
source = "registry+example"
checksum = "ffi-checksum"
dependencies = ["shared"]

[[package]]
name = "shared"
version = "1.0.0"
source = "registry+example"
checksum = "shared-checksum"

[[package]]
name = "unrelated"
version = "1.0.0"
source = "git+unrelated"
'''


class PlatformBoundaryContract(unittest.TestCase):
    def test_projection_retains_only_exact_transitive_packages(self):
        result = tomllib.loads(boundary.project_lock(LOCK, "rusqlite 0.40.2"))
        packages = {p["name"]: p for p in result["package"]}
        self.assertEqual(set(packages), {boundary.PROBE, "rusqlite", "ffi", "shared"})
        original = tomllib.loads(LOCK)["package"]
        for package in original:
            if package["name"] in {"rusqlite", "ffi", "shared"}:
                self.assertEqual(packages[package["name"]], package)
        self.assertEqual(packages[boundary.PROBE]["dependencies"], ["rusqlite 0.40.2"])

    def test_missing_dependency_fails_closed(self):
        with self.assertRaisesRegex(ValueError, "missing or ambiguous"):
            boundary.project_lock(LOCK.replace('"shared"', '"missing"', 1), "rusqlite 0.40.2")

    def test_ambiguous_versions_require_a_qualifier(self):
        lock = LOCK + '\n[[package]]\nname = "shared"\nversion = "2.0.0"\n'
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            boundary.project_lock(lock, "rusqlite 0.40.2")
        lock = lock.replace('["shared"]', '["shared 1.0.0"]')
        lock = lock.replace('"ffi 1.0.0", "shared"', '"ffi 1.0.0", "shared 1.0.0"')
        result = tomllib.loads(boundary.project_lock(lock, "rusqlite 0.40.2"))
        self.assertEqual([p["version"] for p in result["package"] if p["name"] == "shared"], ["1.0.0"])

    def test_source_qualifiers_are_preserved(self):
        lock = LOCK.replace('"ffi 1.0.0"', '"ffi 1.0.0 (registry+example)"')
        result = tomllib.loads(boundary.project_lock(lock, "rusqlite 0.40.2"))
        sqlite = next(p for p in result["package"] if p["name"] == "rusqlite")
        self.assertIn("ffi 1.0.0 (registry+example)", sqlite["dependencies"])

    def test_unknown_lock_format_is_not_reinterpreted(self):
        with self.assertRaisesRegex(ValueError, "format 4"):
            boundary.project_lock(LOCK.replace("version = 4", "version = 5", 1), "rusqlite 0.40.2")

    def test_probe_uses_production_sources_and_dependency_settings(self):
        with tempfile.TemporaryDirectory(prefix="sr-probe-contract-") as directory:
            root = Path(directory)
            (root / "src").mkdir()
            for name in ("sqlite_engine", "platform_path"):
                (root / "src" / f"{name}.rs").write_text("// fixture source\n")
            (root / "Cargo.toml").write_text(
                '[dependencies]\nrusqlite = { version = "=0.40.2", '
                'default-features = false, features = ["bundled", "hooks", "limits"] }\n')
            (root / "Cargo.lock").write_text(LOCK)
            destination = root / "probe with spaces"
            destination.mkdir()
            boundary.prepare_probe(root, destination)
            manifest = tomllib.loads((destination / "Cargo.toml").read_text())
            original = tomllib.loads((root / "Cargo.toml").read_text())
            self.assertEqual(manifest["dependencies"], original["dependencies"])
            source = (destination / "lib.rs").read_text()
            self.assertNotIn("mod storage", source)
            for name in ("sqlite_engine", "platform_path"):
                self.assertIn(str(root / "src" / f"{name}.rs"), source)
            self.assertEqual((root / "Cargo.lock").read_text(), LOCK)

    def test_normalized_lock_may_prune_but_not_change_versions_or_checksums(self):
        projected = boundary.project_lock(LOCK, "rusqlite 0.40.2")
        boundary.validate_resolved_lock(LOCK, projected)
        for altered in [projected.replace("ffi-checksum", "different-checksum"),
                        projected.replace('version = "0.40.2"', 'version = "0.40.3"'),
                        projected.replace("registry+example", "registry+other")]:
            with self.assertRaisesRegex(ValueError, "changed a locked dependency"):
                boundary.validate_resolved_lock(LOCK, altered)

    def test_non_native_host_refuses_before_toolchain_access(self):
        with patch.object(boundary.platform, "system", return_value="Linux"), \
             patch.object(boundary.shutil, "which") as which, \
             contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
            boundary.main(["--native-macos"])
        self.assertEqual(caught.exception.code, 2)
        which.assert_not_called()


if __name__ == "__main__":
    unittest.main()
