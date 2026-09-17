#!/usr/bin/env python3
"""Audit root-reachable packages AND features for every supported feature set.

Cargo's tree projection excludes upstream dev-only oracle dependencies, while
including this project's normal, build and dev edges on every target platform.
No compilation, provider request or sibling checkout is needed.
"""
import hashlib
import itertools
import json
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
BANNED_PREFIXES = ("tokio", "reqwest", "ureq", "tantivy", "fastembed", "candle")
BANNED_NAMES = {"ms", "meta_skill", "meta-skill", "ort", "ort-sys", "tch", "torch-sys"}
BANNED_NAMES |= {"frankensearch", "frankensearch-lexical", "frankensearch-embed",
                 "frankensearch-rerank", "frankensearch-quill-gauntlet"}
BANNED_FEATURES = {"tantivy-oracle", "lexical-tantivy", "cass-compat"}
REQUIRED_PACKAGES = {"asupersync", "frankensearch-core", "frankensearch-quill"}
SEARCH_PACKAGES = {"frankensearch-core", "frankensearch-index", "frankensearch-quill"}


def feature_sets(features: dict) -> list[tuple[str, list[str]]]:
    """Cover default-enabled and default-disabled power sets, with a work bound."""
    names = sorted(set(features) - {"default"})
    if len(names) > 8:
        raise ValueError("feature matrix exceeds 512 combinations; define a reviewed support matrix")
    combinations = []
    for count in range(len(names) + 1):
        for subset in itertools.combinations(names, count):
            for defaults in (False, True):
                flags = [] if defaults else ["--no-default-features"]
                if subset:
                    flags += ["--features", ",".join(subset)]
                label = ("default" if defaults else "no-default") + ":" + ",".join(subset)
                combinations.append((label, flags))
    return combinations


def check_manifest(manifest: dict) -> None:
    sources = set()
    for name in ("frankensearch-core", "frankensearch-quill"):
        dependency = manifest.get("dependencies", {}).get(name)
        if not isinstance(dependency, dict) or dependency.get("default-features") is not False:
            raise ValueError(f"{name} must explicitly disable default features")
        if dependency.get("features"):
            raise ValueError(f"{name} must not opt into optional features")
        revision = dependency.get("rev", "")
        if ("path" in dependency or not dependency.get("git", "").startswith("https://")
                or len(revision) != 40 or any(char not in "0123456789abcdef" for char in revision)):
            raise ValueError(f"{name} requires a public HTTPS Git source and full immutable revision")
        sources.add((dependency["git"], revision))
    if len(sources) != 1:
        raise ValueError("Quill and core must use the same Git source/revision")


def audit_tree(output: str) -> dict:
    packages: dict[str, set[str]] = {}
    features: dict[str, set[str]] = {}
    for raw in output.splitlines():
        if not raw.strip():
            continue
        identity, separator, enabled = raw.removesuffix(" (*)").partition("|")
        parts = identity.split()
        if not separator or len(parts) < 2 or not parts[1].startswith("v"):
            raise ValueError("unrecognized Cargo dependency row")
        name = parts[0]
        packages.setdefault(name, set()).add(identity)
        features.setdefault(name, set()).update(filter(None, enabled.split(",")))
    prohibited = sorted(name for name in packages if name in BANNED_NAMES
                        or name.startswith(BANNED_PREFIXES)
                        or (name.startswith("frankensearch-") and name not in SEARCH_PACKAGES))
    if prohibited:
        raise ValueError("prohibited dependency packages: " + ", ".join(prohibited))
    for name in sorted(REQUIRED_PACKAGES):
        if len(packages.get(name, ())) != 1:
            raise ValueError(f"expected exactly one {name} package source/version")
    for name, enabled in features.items():
        forbidden = enabled & BANNED_FEATURES
        if name in {"frankensearch-quill", "frankensearch-core"}:
            forbidden |= enabled
        if forbidden:
            raise ValueError(f"prohibited features on {name}: " + ", ".join(sorted(forbidden)))
    return {
        "package_count": sum(map(len, packages.values())),
        "required_sources": {name: sorted(packages[name]) for name in sorted(REQUIRED_PACKAGES)},
        "required_features": {name: sorted(features[name]) for name in sorted(REQUIRED_PACKAGES)},
    }


def main() -> None:
    manifest_bytes = (ROOT / "Cargo.toml").read_bytes()
    lock_bytes = (ROOT / "Cargo.lock").read_bytes()
    manifest = tomllib.loads(manifest_bytes.decode())
    check_manifest(manifest)
    # Cargo also creates implicit features for optional dependencies. Reading
    # only [features] from TOML would silently skip those build combinations.
    metadata_command = ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]
    metadata = json.loads(subprocess.run(metadata_command, cwd=ROOT, check=True,
                                         capture_output=True, text=True, timeout=120).stdout)
    roots = [package for package in metadata["packages"]
             if Path(package["manifest_path"]).resolve() == (ROOT / "Cargo.toml").resolve()]
    if len(roots) != 1:
        raise ValueError("expected exactly one root package in Cargo metadata")
    reports = []
    for label, flags in feature_sets(roots[0]["features"]):
        command = ["cargo", "tree", "--locked", "--target", "all", "--color", "never",
                   "-e", "normal,build,dev", "--prefix", "none", "--format", "{p}|{f}", *flags]
        result = subprocess.run(command, cwd=ROOT, check=True, capture_output=True,
                                text=True, timeout=120)
        reports.append({"feature_set": label, "command": command,
                        "tree_sha256": hashlib.sha256(result.stdout.encode()).hexdigest(),
                        **audit_tree(result.stdout)})
    if manifest_bytes != (ROOT / "Cargo.toml").read_bytes() or lock_bytes != (ROOT / "Cargo.lock").read_bytes():
        raise ValueError("manifest or lockfile changed during dependency audit; rerun")
    print(json.dumps({"schema_version": 1, "status": "passed",
                      "metadata_command": metadata_command,
                      "manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
                      "lockfile_sha256": hashlib.sha256(lock_bytes).hexdigest(),
                      "graphs": reports}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, subprocess.SubprocessError, OSError) as error:
        raise SystemExit(str(error)) from None
