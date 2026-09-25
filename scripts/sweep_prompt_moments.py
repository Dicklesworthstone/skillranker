#!/usr/bin/env python3
"""Replay every real prompt moment of a workspace's Claude sessions through `sr hook claude`.

A pre-deploy check against real traffic. It found four live hook failures that no
fixture covered. For each session transcript of the workspace (the consented,
project-local scope), it finds each prompt the user actually submitted. It cuts
the transcript just before that record, because the hook runs before Claude
writes the prompt. It then runs the hook in an owner-only sandbox:
- a sandbox ledger and cache;
- a placeholder API key;
- a loopback endpoint that refuses connections, so nothing leaves the machine.

It reports each moment's recorded outcome. A healthy moment ends at the provider
stage (`request-budget` or `network-failure`, the refused endpoint). Anything
else is refused locally and worth a look: `ambiguous-branch`, `unsupported-input`
or `insufficient-context`.

Output names sessions and line numbers only, never prompt text. The sandbox,
with its transcript cuts, is removed when the run ends. With --compare, each
moment runs through both binaries and differences are listed.

This is a regression check, not an availability measurement. The live ledger and
`sr stats` measure what actually happened.
"""
from __future__ import annotations

import argparse
import collections
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

REFUSING_ENDPOINT = "https://127.0.0.1:9"
PROVIDER_STAGE = {"request-budget", "network-failure", "timeout"}


def encoded_project_dir(workspace: Path) -> Path:
    resolved = str(workspace.resolve())
    return Path.home() / ".claude" / "projects" / resolved.replace("/", "-")


def prompt_moments(transcript: Path, notifications: bool = False):
    """Yield (line index, record) for each prompt the user submitted, and with
    `notifications` also each turn a task notification started."""
    with transcript.open(encoding="utf-8", errors="replace") as handle:
        for index, line in enumerate(handle):
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            content = (record.get("message") or {}).get("content")
            # Compaction summaries and harness meta messages (a tool-call
            # retry notice) are user records, but no one submitted them;
            # replaying them reported refusals that never happen live.
            if (
                record.get("type") != "user"
                or not record.get("promptId")
                or record.get("isSidechain")
                or record.get("isCompactSummary")
                or record.get("isVisibleInTranscriptOnly")
                or record.get("isMeta")
                or not isinstance(content, str)
            ):
                continue
            submitted = (record.get("origin") or {}).get("kind") in (
                None,
                "human",
            ) and record.get("promptSource") != "system"
            notified = (record.get("origin") or {}).get("kind") == "task-notification"
            if submitted or (notifications and notified):
                yield index, record


def run_hook(binary: str, sandbox: Path, payload: dict, workspace: Path) -> str:
    """Run the hook once and return the new ledger row's decision/reason."""
    ledger = sandbox / "data" / "sr" / "ledger.sqlite3"

    def rows() -> int:
        with sqlite3.connect(f"file:{ledger}?mode=ro", uri=True) as db:
            return db.execute("SELECT count(*) FROM ranking_events").fetchone()[0]

    before = rows()
    subprocess.run(
        [binary, "hook", "claude"],
        input=json.dumps(payload).encode(),
        env=sandbox_env(sandbox),
        cwd=workspace,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=30,
        check=False,
    )
    if rows() == before:
        return "no-row"
    with sqlite3.connect(f"file:{ledger}?mode=ro", uri=True) as db:
        decision, reason = db.execute(
            "SELECT decision, reason FROM ranking_events ORDER BY rowid DESC LIMIT 1"
        ).fetchone()
    return f"{decision}/{reason}"


def sandbox_env(sandbox: Path) -> dict:
    return {
        "PATH": "/usr/bin:/bin",
        "HOME": str(Path.home()),  # the real skill roster is what the hook sees
        "XDG_DATA_HOME": str(sandbox / "data"),
        "XDG_CACHE_HOME": str(sandbox / "cache"),
        "XDG_STATE_HOME": str(sandbox / "state"),
        "TYPESAFE_API_KEY": "placeholder-not-a-key",
        "TYPESAFE_ENDPOINT": REFUSING_ENDPOINT,
    }


def init_ledger(binary: str, sandbox: Path) -> None:
    subprocess.run(
        [binary, "ledger", "init", "--json"],
        env=sandbox_env(sandbox),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=60,
        check=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", default=str(Path.home() / ".local/bin/sr"))
    parser.add_argument("--compare", help="a second sr binary to diff against")
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--limit", type=int, default=0, help="at most N moments (0: all)")
    parser.add_argument(
        "--notifications",
        action="store_true",
        help="also replay turns that a task notification started",
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    project_dir = encoded_project_dir(args.workspace)
    transcripts = sorted(project_dir.glob("*.jsonl"))
    if not transcripts:
        print(f"no transcripts in {project_dir}", file=sys.stderr)
        return 2
    binaries = [args.binary] + ([args.compare] if args.compare else [])
    # mkdtemp creates the directory owner-only (0700) under the sticky /tmp.
    sandbox_root = Path(tempfile.mkdtemp(prefix="sr-prompt-sweep-", dir="/tmp"))
    try:
        sandboxes = []
        for n, binary in enumerate(binaries):
            sandbox = sandbox_root / f"b{n}"
            sandbox.mkdir(mode=0o700)
            init_ledger(binary, sandbox)
            sandboxes.append(sandbox)
        cuts = sandbox_root / "cuts"
        cuts.mkdir(mode=0o700)
        results = []
        for transcript in transcripts:
            lines = transcript.read_bytes().splitlines(keepends=True)
            for index, record in prompt_moments(transcript, args.notifications):
                if args.limit and len(results) >= args.limit:
                    break
                cut = cuts / f"{transcript.stem}__{index:06d}.jsonl"
                cut.write_bytes(b"".join(lines[:index]))
                cut.chmod(0o600)
                payload = {
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": transcript.stem,
                    "prompt_id": record["promptId"],
                    "prompt": record["message"]["content"],
                    "transcript_path": str(cut),
                    "cwd": str(args.workspace),
                }
                outcomes = [
                    run_hook(binary, sandbox, payload, args.workspace)
                    for binary, sandbox in zip(binaries, sandboxes)
                ]
                results.append(
                    {"session": transcript.stem, "line": index, "outcomes": outcomes}
                )
                cut.unlink()
    finally:
        shutil.rmtree(sandbox_root, ignore_errors=True)

    counts = [collections.Counter(r["outcomes"][n] for r in results) for n in range(len(binaries))]
    local_refusals = [
        r for r in results if r["outcomes"][-1].split("/")[-1] not in PROVIDER_STAGE
    ]
    diffs = [r for r in results if len(set(r["outcomes"])) > 1]
    if args.json:
        print(json.dumps({"binaries": binaries, "moments": len(results),
                          "counts": [dict(c) for c in counts],
                          "local_refusals": local_refusals, "differences": diffs}, indent=2))
    else:
        print(f"{len(results)} prompt moments from {len(transcripts)} transcripts")
        for binary, count in zip(binaries, counts):
            print(f"  {binary}:")
            for outcome, total in count.most_common():
                print(f"    {total:5d}  {outcome}")
        for r in diffs:
            print(f"  differs {r['session']}:{r['line']}: " + " -> ".join(r["outcomes"]))
        for r in local_refusals:
            print(f"  refused locally {r['session']}:{r['line']}: {r['outcomes'][-1]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
