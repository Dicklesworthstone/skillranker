#!/usr/bin/env python3
"""Tests of boundary-runner validation logic; these do not execute native Rust."""
from __future__ import annotations

import copy
from pathlib import Path
import tomllib
import unittest
from unittest import mock

import check_native_storage_boundary as boundary

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def production_fixture() -> tuple[dict, dict]:
    """Small synthetic lock graph, not a replacement for production Cargo.lock."""
    manifest = {
        "package": {"edition": "2024", "rust-version": "1.95"},
        "dependencies": {
            "nix": {"version": "0.31", "default-features": False, "features": ["fs", "user"]},
            "rusqlite": {
                "version": "=0.40.2", "default-features": False,
                "features": ["bundled", "hooks", "limits"],
            },
        },
    }
    lock = {"package": [
        {"name": "skillranker", "version": "0.1.0"},
        {"name": "nix", "version": "0.31.0", "source": REGISTRY, "checksum": "a" * 64},
        {"name": "rusqlite", "version": "0.40.2", "source": REGISTRY, "checksum": "b" * 64},
        {"name": "unused-in-slice", "version": "1.0.0", "source": REGISTRY, "checksum": "c" * 64},
    ]}
    return manifest, lock


def derived_fixture(lock: dict) -> dict:
    return {"package": [
        {"name": boundary.SLICE_NAME, "version": "0.0.0"},
        copy.deepcopy(lock["package"][1]), copy.deepcopy(lock["package"][2]),
    ]}


class NativeHostTests(unittest.TestCase):
    def test_accepts_native_darwin_hosts(self):
        for machine, target in [("arm64", "aarch64-apple-darwin"), ("x86_64", "x86_64-apple-darwin")]:
            self.assertEqual(boundary.native_target(f"host: {target}\n", "Darwin", machine), target)

    def test_accepts_native_linux_gnu_and_musl(self):
        for arch in ("x86_64", "aarch64"):
            for abi in ("gnu", "musl"):
                target = f"{arch}-unknown-linux-{abi}"
                self.assertEqual(boundary.native_target(f"host: {target}\n", "Linux", arch), target)

    def test_rejects_foreign_os_or_wrong_architecture(self):
        for system, machine, target in [
            ("Linux", "aarch64", "aarch64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-unknown-linux-gnu"),
            ("Darwin", "arm64", "x86_64-apple-darwin"),
            ("FreeBSD", "x86_64", "x86_64-unknown-freebsd"),
        ]:
            with self.subTest(system=system, machine=machine, target=target), self.assertRaises(ValueError):
                boundary.native_target(f"host: {target}\n", system, machine)

    def test_rejects_missing_ambiguous_or_unknown_host(self):
        for verbose, machine in [
            ("rustc 1.95.0\n", "arm64"),
            ("host: aarch64-apple-darwin\nhost: x86_64-apple-darwin\n", "arm64"),
            ("host: aarch64-apple-darwin\n", "unknown"),
        ]:
            with self.subTest(verbose=verbose, machine=machine), self.assertRaises(ValueError):
                boundary.native_target(verbose, "Darwin", machine)


class ToolchainTests(unittest.TestCase):
    def test_pins_the_repository_selected_toolchain(self):
        root = Path("/synthetic/repo")
        verbose = "rustc 1.95.0\nhost: aarch64-apple-darwin\n"
        with mock.patch.object(boundary.subprocess, "check_output", side_effect=[
            verbose, "/synthetic/toolchain\n", verbose,
        ]) as run, mock.patch.object(Path, "is_file", return_value=True), \
                mock.patch.object(boundary.os, "access", return_value=True):
            rustc, cargo, actual = boundary.native_toolchain(root)
        self.assertEqual(rustc, Path("/synthetic/toolchain/bin/rustc"))
        self.assertEqual(cargo, Path("/synthetic/toolchain/bin/cargo"))
        self.assertEqual(actual, verbose)
        self.assertTrue(all(call.kwargs["cwd"] == root for call in run.call_args_list))
        self.assertEqual(run.call_args_list[-1].args[0], [str(rustc), "-vV"])

    def test_rejects_incomplete_or_changing_toolchains(self):
        verbose = "host: aarch64-apple-darwin\n"
        with mock.patch.object(boundary.subprocess, "check_output", side_effect=[
            verbose, "/synthetic/toolchain\n",
        ]), mock.patch.object(Path, "is_file", return_value=False):
            with self.assertRaisesRegex(ValueError, "executable rustc and cargo"):
                boundary.native_toolchain(Path("/synthetic/repo"))
        with mock.patch.object(boundary.subprocess, "check_output", side_effect=[
            verbose, "/synthetic/toolchain\n", "host: x86_64-apple-darwin\n",
        ]), mock.patch.object(Path, "is_file", return_value=True), \
                mock.patch.object(boundary.os, "access", return_value=True):
            with self.assertRaisesRegex(ValueError, "changed during inspection"):
                boundary.native_toolchain(Path("/synthetic/repo"))


class DependencyBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.manifest, self.lock = production_fixture()
        self.pins = boundary.dependency_pins(self.manifest, self.lock)

    def test_manifest_uses_only_exact_pins_and_production_features(self):
        generated = tomllib.loads(boundary.slice_manifest(self.manifest, self.pins))
        self.assertEqual(set(generated["dependencies"]), {"nix", "rusqlite"})
        for name in ("nix", "rusqlite"):
            dependency = generated["dependencies"][name]
            self.assertEqual(dependency["version"], "=" + self.pins[name]["version"])
            self.assertFalse(dependency["default-features"])
            self.assertEqual(dependency["features"], self.manifest["dependencies"][name]["features"])
        self.assertEqual(generated["test"][0]["path"], "native_storage_boundary.rs")
        self.assertEqual(generated["workspace"], {})
        self.assertEqual(generated["lints"]["rust"]["unsafe_code"], "forbid")

    def test_rejects_changed_bundling_and_default_features(self):
        for name, field, value in [
            ("rusqlite", "features", ["hooks", "limits"]),
            ("rusqlite", "default-features", True),
            ("nix", "default-features", True),
            ("nix", "features", "fs"),
        ]:
            manifest = copy.deepcopy(self.manifest)
            manifest["dependencies"][name][field] = value
            with self.subTest(name=name, field=field), self.assertRaises(ValueError):
                boundary.dependency_pins(manifest, self.lock)

    def test_rejects_ambiguous_or_non_registry_direct_dependencies(self):
        ambiguous = copy.deepcopy(self.lock)
        ambiguous["package"].append(dict(ambiguous["package"][1], version="0.32.0"))
        with self.assertRaises(ValueError):
            boundary.dependency_pins(self.manifest, ambiguous)
        changed_source = copy.deepcopy(self.lock)
        changed_source["package"][1]["source"] = "git+unqualified-source"
        with self.assertRaises(ValueError):
            boundary.dependency_pins(self.manifest, changed_source)

    def test_accepts_only_the_resolved_production_subset(self):
        boundary.verify_slice_lock(self.lock, derived_fixture(self.lock), self.pins)

    def test_rejects_source_version_or_checksum_drift(self):
        for field, value in [("version", "0.99.0"), ("checksum", "d" * 64), ("source", "git+other")]:
            changed = derived_fixture(self.lock)
            changed["package"][1][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                boundary.verify_slice_lock(self.lock, changed, self.pins)

    def test_rejects_new_transitive_packages_and_local_injection(self):
        for package in [
            {"name": "new-package", "version": "1.0.0", "source": REGISTRY, "checksum": "d" * 64},
            {"name": "local-replacement", "version": "0.0.0"},
        ]:
            changed = derived_fixture(self.lock)
            changed["package"].append(package)
            with self.subTest(package=package), self.assertRaises(ValueError):
                boundary.verify_slice_lock(self.lock, changed, self.pins)

    def test_rejects_missing_or_duplicated_direct_dependency(self):
        changed = derived_fixture(self.lock)
        changed["package"].pop()
        with self.assertRaises(ValueError):
            boundary.verify_slice_lock(self.lock, changed, self.pins)
        changed = derived_fixture(self.lock)
        changed["package"].append(copy.deepcopy(changed["package"][1]))
        with self.assertRaises(ValueError):
            boundary.verify_slice_lock(self.lock, changed, self.pins)

    def test_source_imports_the_production_boundary_not_the_storage_tree(self):
        source = boundary.slice_source(Path("/synthetic/snapshot"))
        self.assertIn('src/sqlite_engine.rs', source)
        self.assertIn('src/storage/platform.rs', source)
        self.assertIn('src/platform_path.rs', source)
        for excluded in ("filesystem.rs", "ledger.rs", "mod storage;", "mod replay;", "mod cli;"):
            self.assertNotIn(excluded, source)
        self.assertIn("platform::DirectoryIdentity", source)
        self.assertIn("platform::local_filesystem(&stat)", source)
        self.assertIn('#![forbid(unsafe_code)]', source)

    def test_rust_path_literal_handles_quotes_without_injection(self):
        literal = boundary.rust_path(Path('/synthetic/with"#quote/source.rs'))
        self.assertTrue(literal.startswith('r##"'))
        self.assertTrue(literal.endswith('"##'))


if __name__ == "__main__":
    unittest.main()
