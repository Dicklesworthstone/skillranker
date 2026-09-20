#!/usr/bin/env python3
"""Compile the production SQLite/platform boundary on a real Linux or macOS host.

This is an isolated test slice, NOT a claim that sr or disk storage is qualified.
It imports frozen production-module bytes, without compiling filesystem.rs,
ledger, replay, or the CLI. Requires Python 3.11+, Rust/C tooling and dependencies
already cached from the repository's Cargo.lock; all Cargo commands are offline.
The temporary build and lockfile are retained for inspection.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import tomllib

SLICE_NAME = "skillranker-native-storage-boundary"
SOURCE_PATHS = ("src/sqlite_engine.rs", "src/storage/platform.rs", "src/platform_path.rs")


def native_target(verbose: str, system: str, machine: str) -> str:
    """Reject a cross compiler instead of manufacturing a target_os cfg."""
    hosts = [line[6:].strip() for line in verbose.splitlines() if line.startswith("host: ")]
    architectures = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "AMD64": "x86_64"}
    arch = architectures.get(machine)
    if len(hosts) != 1 or arch is None:
        raise ValueError("cannot establish the native Rust host/architecture")
    suffixes = {
        "Darwin": ("apple-darwin",),
        "Linux": ("unknown-linux-gnu", "unknown-linux-musl"),
    }.get(system, ())
    if hosts[0] not in {f"{arch}-{suffix}" for suffix in suffixes}:
        raise ValueError("this check requires a native Linux/macOS Rust compiler matching this host")
    return hosts[0]


def native_toolchain(root: Path) -> tuple[Path, Path, str]:
    """Resolve the repository-selected toolchain before leaving its directory."""
    verbose = subprocess.check_output(["rustc", "-vV"], cwd=root, text=True)
    sysroot = Path(subprocess.check_output(
        ["rustc", "--print", "sysroot"], cwd=root, text=True,
    ).strip())
    if not sysroot.is_absolute():
        raise ValueError("Rust returned a non-absolute sysroot")
    rustc, cargo = sysroot / "bin/rustc", sysroot / "bin/cargo"
    if not all(path.is_file() and os.access(path, os.X_OK) for path in (rustc, cargo)):
        raise ValueError("the selected Rust sysroot must contain executable rustc and cargo")
    actual = subprocess.check_output([str(rustc), "-vV"], cwd=root, text=True)
    if actual != verbose:
        raise ValueError("the selected Rust toolchain changed during inspection")
    return rustc, cargo, verbose


def dependency_pins(manifest: dict, lock: dict) -> dict[str, dict]:
    pins = {}
    for name in ("nix", "rusqlite"):
        matches = [p for p in lock["package"] if p["name"] == name]
        if len(matches) != 1:
            raise ValueError(f"expected one unambiguous locked {name} package")
        package = matches[0]
        if not package.get("source", "").startswith("registry+"):
            raise ValueError(f"{name} must use its locked registry source")
        dependency = manifest["dependencies"][name]
        if not isinstance(dependency, dict) or dependency.get("default-features") is not False:
            raise ValueError(f"review changed {name} dependency features before running this slice")
        features = dependency.get("features", [])
        if not isinstance(features, list) or not all(isinstance(f, str) for f in features):
            raise ValueError(f"invalid {name} feature list")
        if name == "rusqlite" and "bundled" not in features:
            raise ValueError("the slice must use the production bundled SQLite engine")
        pins[name] = {"version": package["version"], "features": list(features)}
    return pins


def slice_manifest(production: dict, pins: dict[str, dict]) -> str:
    package = production["package"]
    result = (
        f'[package]\nname = "{SLICE_NAME}"\nversion = "0.0.0"\npublish = false\n'
        f'edition = {json.dumps(package["edition"])}\n'
        f'rust-version = {json.dumps(package["rust-version"])}\n'
        '[workspace]\n\n[[test]]\nname = "native_storage_boundary"\n'
        'path = "native_storage_boundary.rs"\n\n[dependencies]\n'
    )
    for name, pin in sorted(pins.items()):
        result += (
            f'{name} = {{ version = "={pin["version"]}", default-features = false, '
            f'features = {json.dumps(pin["features"])} }}\n'
        )
    return result + '\n[lints.rust]\nunsafe_code = "forbid"\n'


def verify_slice_lock(production: dict, actual: dict, pins: dict[str, dict]) -> None:
    """Permit only source/version/checksum identities already in Cargo.lock."""
    def identity(package: dict) -> tuple:
        return tuple(package.get(field) for field in ("name", "version", "source", "checksum"))

    allowed = {identity(p) for p in production["package"] if p.get("source")}
    packages = actual["package"]
    local = [p for p in packages if not p.get("source")]
    if len(local) != 1 or local[0]["name"] != SLICE_NAME or local[0]["version"] != "0.0.0":
        raise ValueError("unexpected local package in the isolated boundary lockfile")
    for package in packages:
        if package.get("source") and identity(package) not in allowed:
            raise ValueError(f"slice resolution drifted outside Cargo.lock: {package['name']}")
    for name, pin in pins.items():
        versions = [p["version"] for p in packages if p["name"] == name]
        if versions != [pin["version"]]:
            raise ValueError(f"slice does not contain exactly the locked {name} version")


def rust_path(path: Path) -> str:
    text = str(path)
    hashes = "#"
    while f'"{hashes}' in text:
        hashes += "#"
    return f'r{hashes}"{text}"{hashes}'


def slice_source(root: Path) -> str:
    return (
        '#![forbid(unsafe_code)]\n\n'
        f'#[path = {rust_path(root / SOURCE_PATHS[0])}]\nmod sqlite_engine;\n'
        f'#[path = {rust_path(root / SOURCE_PATHS[1])}]\nmod platform;\n'
        f'#[path = {rust_path(root / SOURCE_PATHS[2])}]\nmod platform_path;\n'
        + r'''
fn native_identity(stat: &nix::sys::stat::FileStat) -> platform::DirectoryIdentity {
    (stat.st_dev, stat.st_ino)
}

#[test]
fn native_descriptor_identity_uses_the_actual_platform_field_types() {
    let directory = std::fs::File::open(".").unwrap();
    let first = nix::sys::stat::fstat(&directory).unwrap();
    let second = nix::sys::stat::fstat(&directory).unwrap();
    assert_eq!(native_identity(&first), native_identity(&second));
}

#[test]
fn native_statfs_calls_the_production_filesystem_admission_boundary() {
    let directory = std::fs::File::open(".").unwrap();
    let stat = nix::sys::statfs::fstatfs(&directory).unwrap();
    #[cfg(target_os = "linux")]
    let expected = {
        use nix::sys::statfs::{BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, FsType, TMPFS_MAGIC};
        let kind = stat.filesystem_type();
        matches!(kind, EXT4_SUPER_MAGIC | BTRFS_SUPER_MAGIC | TMPFS_MAGIC)
            || kind == FsType(0x5846_5342)
    };
    #[cfg(target_os = "macos")]
    let expected = matches!(stat.filesystem_type_name(), "apfs" | "hfs");
    assert_eq!(platform::local_filesystem(&stat), expected);
    let current = std::env::current_dir().unwrap();
    assert!(platform::storage_path(current).is_absolute());
}
'''
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    root = args.repo.resolve()
    try:
        source_bytes = {path: (root / path).read_bytes() for path in SOURCE_PATHS}
        manifest_bytes = (root / "Cargo.toml").read_bytes()
        manifest = tomllib.loads(manifest_bytes.decode())
        lock_bytes = (root / "Cargo.lock").read_bytes()
        lock = tomllib.loads(lock_bytes.decode())
        pins = dependency_pins(manifest, lock)
        rustc, cargo, verbose = native_toolchain(root)
        target = native_target(verbose, platform.system(), platform.machine())
        build = Path(tempfile.mkdtemp(prefix="sr-native-storage-boundary-"))
        print(f"Boundary-only native check; retained build directory: {build}", flush=True)
        (build / "Cargo.toml").write_text(slice_manifest(manifest, pins))
        snapshot = build / "production"
        for relative, content in source_bytes.items():
            destination = snapshot / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content)
        (build / "native_storage_boundary.rs").write_text(slice_source(snapshot))
        (build / "Cargo.lock").write_bytes(lock_bytes)
        env = os.environ.copy()
        # A fresh target directory avoids treating unrelated pooled output as evidence.
        env["CARGO_TARGET_DIR"] = str(build / "target")
        env["RUSTC"] = str(rustc)
        # Stay on the inspected toolchain, even outside the repository's rustup
        # override. Empty wrappers disable inherited remote/cached compiler wrappers.
        env["RUSTC_WRAPPER"] = ""
        env["RUSTC_WORKSPACE_WRAPPER"] = ""
        # Resolve without compiling, retaining existing lockfile choices rather
        # than generate-lockfile's intentional upgrade to latest cached versions.
        command = [str(cargo), "metadata", "--offline", "--format-version", "1",
                   "--manifest-path", str(build / "Cargo.toml")]
        metadata = subprocess.check_output(command, cwd=build, env=env)
        (build / "cargo-metadata.json").write_bytes(metadata)
        derived = tomllib.loads((build / "Cargo.lock").read_text())
        verify_slice_lock(lock, derived, pins)
        # Do not begin compilation if source changed while resolving the slice.
        if any((root / path).read_bytes() != content for path, content in {
            **source_bytes, "Cargo.toml": manifest_bytes, "Cargo.lock": lock_bytes,
        }.items()):
            raise ValueError("production source changed while preparing the boundary check")
        print(json.dumps({
            "scope": "native boundary slice only; not sr or disk-store qualification",
            "native_target": target,
            "rustc": verbose.strip(),
            "rustc_path": str(rustc),
            "cargo_path": str(cargo),
            "production_manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
            "production_lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
            "source_sha256": {p: hashlib.sha256(b).hexdigest() for p, b in source_bytes.items()},
        }, indent=2), flush=True)
        subprocess.run([
            str(cargo), "test", "--locked", "--offline", "--manifest-path", str(build / "Cargo.toml"),
            "--target", target, "--test", "native_storage_boundary",
        ], cwd=build, env=env, check=True)
        if any((root / path).read_bytes() != content for path, content in {
            **source_bytes, "Cargo.toml": manifest_bytes, "Cargo.lock": lock_bytes,
        }.items()):
            raise ValueError("production source changed during the boundary check; discard this run")
        print("PASS: native SQLite/platform boundary slice only; full sr checks remain separate.")
        return 0
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"Boundary check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
