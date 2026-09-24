#!/usr/bin/env python3
"""Check repository documentation without executing its commands or using network.

Syntax and cross-document agreement are documentation evidence only. Runtime
schema validation of the ranked examples lives in tests/output_contract.rs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
DOCUMENTS = ("README.md", "AGENTS.md", "COMPREHENSIVE_PLAN_TO_DESIGN_SKILLRANKER.md")
FENCE = re.compile(r"^```([^\n]*)\n(.*?)^```[ \t]*$", re.MULTILINE | re.DOTALL)
LINK = re.compile(r"(?<!!)\[[^\]\n]+\]\(([^)\n]+)\)")


class InvalidDocument(ValueError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise InvalidDocument(message)


def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON key")
        result[key] = value
    return result


def finite_float(value: str) -> float:
    number = float(value)
    require(math.isfinite(number), "nonfinite JSON number")
    return number


def invalid_constant(_: str) -> None:
    raise InvalidDocument("nonfinite JSON constant")


def strict_json(text: str) -> object:
    return json.loads(text, object_pairs_hook=unique_object,
                      parse_float=finite_float, parse_constant=invalid_constant)


def shape(value: object) -> object:
    """Compare field/type contracts while allowing different illustrative inputs."""
    if isinstance(value, dict):
        return {key: shape(item) for key, item in value.items()}
    if isinstance(value, list):
        return sorted({json.dumps(shape(item), sort_keys=True) for item in value})
    if type(value) in (int, float):
        return "number"
    return type(value).__name__


def anchors(text: str) -> set[str]:
    """GitHub heading IDs for the ordinary Markdown headings used in this repo."""
    found: set[str] = set()
    counts: dict[str, int] = {}
    for line in FENCE.sub("", text).splitlines():
        match = re.match(r"^#{1,6}\s+(.+?)(?:\s+#+)?$", line)
        if match is None:
            continue
        heading = re.sub(r"<[^>]*>", "", match[1]).lower()
        slug = "".join(c for c in heading if c.isalnum() or c in "-_ ").replace(" ", "-")
        count = counts.get(slug, 0)
        counts[slug] = count + 1
        found.add(slug if count == 0 else f"{slug}-{count}")
    return found


def validate(root: Path) -> dict[str, object]:
    counts = {"json_examples": 0, "toml_examples": 0, "shell_examples": 0,
              "local_links": 0, "compared_examples": 0, "capability_phases": 0}
    inputs: dict[str, str] = {}
    examples: dict[str, list[dict[str, object]]] = {}
    texts: dict[str, str] = {}

    def read(relative: str) -> str:
        raw = (root / relative).read_bytes()
        inputs[relative] = hashlib.sha256(raw).hexdigest()
        return raw.decode("utf-8")

    for name in DOCUMENTS:
        text = read(name)
        texts[name] = text
        examples[name] = []
        for block in FENCE.finditer(text):
            language, body = block[1].strip(), block[2]
            location = f"{name}:{text[:block.start()].count(chr(10)) + 1}"
            try:
                if language == "json":
                    value = strict_json(body)
                    require(isinstance(value, dict), "JSON example must be an object")
                    examples[name].append(value)
                    counts["json_examples"] += 1
                elif language == "toml":
                    tomllib.loads(body)
                    counts["toml_examples"] += 1
                elif language in {"bash", "sh", "shell"}:
                    # --noprofile/--norc plus a minimal environment also prevents
                    # BASH_ENV and exported functions from running during -n.
                    result = subprocess.run(
                        ["/bin/bash", "--noprofile", "--norc", "-n"],
                        input=body, text=True, capture_output=True, timeout=5,
                        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}, check=False,
                    )
                    require(result.returncode == 0, "invalid shell syntax")
                    counts["shell_examples"] += 1
            except (ValueError, subprocess.TimeoutExpired) as error:
                raise InvalidDocument(f"{location}: invalid {language} example") from error
        for match in LINK.finditer(FENCE.sub("", text)):
            target = match[1].strip().strip("<>")
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc:
                continue  # Deliberately no network; external links are not verified.
            relative = unquote(parsed.path) or name
            path = (root / relative).resolve()
            require(path.is_relative_to(root), f"{name}: link escapes repository")
            require(path.exists(), f"{name}: missing local link {relative}")
            if parsed.fragment:
                require(path.suffix == ".md", f"{name}: unsupported local fragment")
                linked = read(path.relative_to(root).as_posix())
                require(unquote(parsed.fragment) in anchors(linked),
                        f"{name}: missing heading in {relative}")
            counts["local_links"] += 1

    readme = examples["README.md"]
    plan = examples["COMPREHENSIVE_PLAN_TO_DESIGN_SKILLRANKER.md"]
    for key in ("decision", "hookSpecificOutput"):
        left = [example for example in readme if key in example]
        right = [example for example in plan if key in example]
        require(len(left) == len(right) == 1, "missing or ambiguous shared JSON example")
        require(shape(left[0]) == shape(right[0]),
                f"README/plan {key} example field types disagree")
        counts["compared_examples"] += 1

    # The plan table is the independent source for staged command phases.
    # This catches absent commands as well as prematurely advanced phases.
    phases: dict[str, str] = {}
    command_table = texts[DOCUMENTS[2]].split("### Commands and release stages\n", 1)[1]
    command_table = command_table.split("Shared ranking controls:", 1)[0]
    for line in command_table.splitlines():
        if not line.startswith("| `sr"):
            continue
        phase = re.search(r"\| P([0-9])", line)
        if phase:
            for command in re.findall(r"`sr ([a-z][a-z-]*)", line.split("|", 2)[1]):
                phases.setdefault(command, "p" + phase[1])
    require(bool(phases), "missing command phase table")
    capability = strict_json(read("tests/fixtures/capabilities.v1.json"))
    require(isinstance(capability, dict), "invalid capability fixture")
    planned: dict[str, str] = {}
    for row in capability["planned_cli"]:
        require(row["name"] not in planned, "duplicate capability command")
        planned[row["name"]] = row["earliest_phase"]
    require(planned == phases, "capability command phases disagree with plan")
    require(not set(planned).intersection(capability["implemented_cli"]),
            "planned command advertised as implemented")
    counts["capability_phases"] = len(planned)
    read("LICENSE")
    inputs["scripts/validate_public_contracts.py"] = hashlib.sha256(
        (root / "scripts/validate_public_contracts.py").read_bytes()).hexdigest()
    return {"schema_version": 1, "scope": "documentation_consistency_only",
            "network_used": False, "examples_executed": False,
            "status": "passed", "checks": counts, "sha256": inputs}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT,
                        help="checkout root containing the documents")
    args = parser.parse_args()
    try:
        result = validate(args.root.resolve())
    except (OSError, ValueError, KeyError, IndexError, TypeError) as error:
        # Fixed errors for unexpected parser/I/O failures; no document body,
        # shell parser stderr, environment value or private argument is echoed.
        message = str(error) if isinstance(error, InvalidDocument) else "invalid documentation input"
        print(f"validation failed: {message}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
