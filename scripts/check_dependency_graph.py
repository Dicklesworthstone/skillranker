#!/usr/bin/env python3
"""Check all enabled normal/build/dev dependencies without compiling anything."""
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
BANNED_PREFIXES = ("tokio", "reqwest", "ureq", "tantivy", "fastembed", "candle")
BANNED_NAMES = {"meta_skill", "meta-skill", "ort", "ort-sys", "tch", "torch-sys"}


def main() -> None:
    result = subprocess.run(
        ["cargo", "tree", "--locked", "--all-features", "--color", "never",
         "-e", "normal,build,dev", "--prefix", "none"],
        cwd=ROOT, check=True, capture_output=True, text=True, timeout=120,
    )
    packages = {line.removesuffix(" (*)") for line in result.stdout.splitlines() if line}
    prohibited = sorted({line.split()[0] for line in packages
                         if line.split()[0] in BANNED_NAMES
                         or line.split()[0].startswith(BANNED_PREFIXES)})
    runtimes = {line for line in packages if line.startswith("asupersync ")}
    quill = {line for line in packages if line.startswith("frankensearch-quill ")}
    if prohibited:
        raise SystemExit("prohibited dependency packages: " + ", ".join(prohibited))
    if len(runtimes) != 1 or len(quill) != 1:
        raise SystemExit("expected exactly one Asupersync source and one Quill source")
    print("dependency graph verified: one Asupersync source, one Quill source, no prohibited packages")


if __name__ == "__main__":
    main()
