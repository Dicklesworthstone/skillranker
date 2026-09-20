#!/usr/bin/env python3
"""Compile production portability leaves without compiling storage or the CLI.

The temporary probe uses the root manifest's rusqlite settings and the exact
reachable packages/checksums from Cargo.lock. It contains no copied Rust code.
Native macOS proof requires --native-macos on a Darwin host; cross-checks and
simulated platform names are not accepted as native evidence.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PROBE = "skillranker-platform-boundary"


def project_lock(lock_text: str, dependency: str) -> str:
    """Retain exact lock blocks reachable from one unambiguous dependency."""
    header, *blocks = re.split(r"(?m)^\[\[package\]\]\s*\n", lock_text)
    if tomllib.loads(header).get("version") != 4:
        raise ValueError("expected Cargo.lock format 4")
    packages = [(tomllib.loads("[[package]]\n" + block)["package"][0], block)
                for block in blocks]

    def resolve(spec: str) -> int:
        match = re.fullmatch(r"([^ ]+)(?: ([^ ()]+))?(?: \(([^)]+)\))?", spec)
        if not match:
            raise ValueError(f"invalid locked dependency: {spec}")
        name, version, source = match.groups()
        found = [i for i, (package, _) in enumerate(packages)
                 if package["name"] == name
                 and (version is None or package["version"] == version)
                 and (source is None or package.get("source") == source)]
        if len(found) != 1:
            raise ValueError(f"missing or ambiguous locked dependency: {spec}")
        return found[0]

    selected: set[int] = set()
    pending = [resolve(dependency)]
    while pending:
        index = pending.pop()
        if index in selected:
            continue
        selected.add(index)
        pending.extend(resolve(spec) for spec in packages[index][0].get("dependencies", []))
    probe = (f'[[package]]\nname = "{PROBE}"\nversion = "0.0.0"\n'
             f'dependencies = [{json.dumps(dependency)}]\n\n')
    return header + probe + "".join("[[package]]\n" + packages[i][1]
                                    for i in sorted(selected))


def validate_resolved_lock(root_text: str, probe_text: str) -> None:
    """Permit graph pruning, never a different package version/source/checksum."""
    def identity(package: dict) -> tuple:
        return package["name"], package["version"], package.get("source")

    allowed = {identity(p): p.get("checksum")
               for p in tomllib.loads(root_text)["package"]}
    for package in tomllib.loads(probe_text)["package"]:
        if identity(package) == (PROBE, "0.0.0", None):
            continue
        key = identity(package)
        if key not in allowed or package.get("checksum") != allowed[key]:
            raise ValueError(f"probe changed a locked dependency: {key}")


def prepare_probe(root: Path, destination: Path) -> str:
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    sqlite = manifest["dependencies"]["rusqlite"]
    if set(sqlite) != {"version", "default-features", "features"}:
        raise ValueError("review probe for changed rusqlite dependency settings")
    if not sqlite["version"].startswith("="):
        raise ValueError("rusqlite must remain exactly pinned")
    dependency = "rusqlite " + sqlite["version"][1:]
    lock = project_lock((root / "Cargo.lock").read_text(), dependency)
    settings = ", ".join(f"{key} = {json.dumps(value)}" for key, value in sqlite.items())
    (destination / "Cargo.toml").write_text(
        f'[package]\nname = "{PROBE}"\nversion = "0.0.0"\nedition = "2024"\n'
        'publish = false\n\n[workspace]\n\n[lib]\npath = "lib.rs"\n\n'
        f'[dependencies]\nrusqlite = {{ {settings} }}\n'
    )
    (destination / "Cargo.lock").write_text(lock)
    modules = ['#![forbid(unsafe_code)]\n']
    for name in ("sqlite_engine", "platform_path"):
        path = (root / "src" / f"{name}.rs").resolve(strict=True)
        modules.append(f'#[path = {json.dumps(str(path))}]\npub mod {name};\n')
    modules.append(
        "pub fn normalized_path(path: std::path::PathBuf) -> std::path::PathBuf {\n"
        "    platform_path::storage_path(path)\n}\n"
    )
    (destination / "lib.rs").write_text("\n".join(modules))
    return sqlite["version"][1:]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-macos", action="store_true",
                        help="require a real Darwin host and its native Rust target")
    parser.add_argument("--offline", action="store_true", help="use only cached dependencies")
    args = parser.parse_args(argv)
    if args.native_macos and platform.system() != "Darwin":
        parser.error("native macOS validation requires a Darwin host; nothing was compiled")
    if not shutil.which("cargo") or not shutil.which("rustc"):
        parser.error("cargo and rustc are required; nothing was compiled")
    # Do not let ambient flags turn this into a cfg simulation or a cross-run.
    if any(os.environ.get(key) for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")):
        parser.error("unset Rust flags before recording a native portability check")
    try:
        version = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True)
        host = next(line.removeprefix("host: ") for line in version.splitlines()
                    if line.startswith("host: "))
        if args.native_macos and not host.endswith("-apple-darwin"):
            parser.error("rustc host is not an Apple Darwin target")
        print(f"Platform: {platform.system()} {platform.machine()}\n{version}", flush=True)
        with tempfile.TemporaryDirectory(prefix="sr-platform-boundary-") as directory:
            probe = Path(directory)
            sqlite_version = prepare_probe(ROOT, probe)
            # Conservatively normalize the probe's feature graph (which may
            # prune dependencies enabled only by the full application). This
            # resolves metadata only; no build script or Rust test runs yet.
            normalize = ["cargo", "update", "--manifest-path", str(probe / "Cargo.toml"),
                         "--package", "rusqlite", "--precise", sqlite_version]
            if args.offline:
                normalize.append("--offline")
            subprocess.run(normalize, cwd=ROOT, check=True)
            validate_resolved_lock((ROOT / "Cargo.lock").read_text(),
                                   (probe / "Cargo.lock").read_text())
            command = ["cargo", "test", "--locked", "--manifest-path", str(probe / "Cargo.toml"),
                       "--target", host, "--target-dir", str(ROOT / "target/platform-boundary")]
            if args.offline:
                command.append("--offline")
            return subprocess.run(command, cwd=ROOT, check=False).returncode
    except (OSError, ValueError, StopIteration, subprocess.CalledProcessError) as error:
        print(f"Platform boundary check failed before completion: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
