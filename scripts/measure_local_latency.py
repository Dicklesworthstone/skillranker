#!/usr/bin/env python3
"""Measure the local paths of one `sr` binary: wall time and peak RSS.

    scripts/measure_local_latency.py BINARY [--runs N] [--transcripts DIR]

Three paths, each run N times against the real `~/.claude/skills` roster with
private, throwaway XDG state (sr-roadmap-l1i.7.12):

- offline cache miss: the whole local phase, ending before any request;
- exact cache hit: one fill through the loopback TLS fixture, then offline
  hits served from the cache, which is the path the 100 ms p95 target names;
- offline hook: `sr hook claude --offline --no-persist` over the most recent
  real transcripts in DIR, if given.

No Jev request is made and no user state is written. Prints one JSON report.
It is a local measurement on the host it runs on: record the host, its load
and the roster with the numbers, and never read them as provider latency.
"""

import argparse
import glob
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

REPO = pathlib.Path(__file__).resolve().parent.parent
FIXTURE = REPO / "tests/fixtures/jev-tls"


def pct(values, p):
    values = sorted(values)
    index = max(0, min(len(values) - 1, -(-p * len(values) // 100) - 1))
    return values[index]


def summary(samples):
    walls = [s["wall_ms"] for s in samples]
    rss = [s["rss_kib"] for s in samples]
    return {
        "n": len(samples),
        "wall_ms": {"p50": pct(walls, 50), "p95": pct(walls, 95), "p99": pct(walls, 99),
                    "max": max(walls)},
        "peak_rss_mib": round(max(rss) / 1024, 1),
        "exits": sorted({s["exit"] for s in samples}),
    }


class Runner:
    def __init__(self, binary, root):
        self.binary = str(pathlib.Path(binary).resolve())
        self.root = root

    def env(self, port):
        return {
            "HOME": os.environ["HOME"],
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_STATE_HOME": str(self.root / "state"),
            "TYPESAFE_API_KEY": "synthetic-measurement-key",
            "TYPESAFE_ENDPOINT": f"https://localhost:{port}",
            "SSL_CERT_FILE": str(FIXTURE / "ca.pem"),
            "PATH": "/usr/bin:/bin",
        }

    def timed(self, args, port=1, stdin=None, cwd=None):
        streams = {"input": stdin} if stdin is not None else {"stdin": subprocess.DEVNULL}
        start = time.monotonic()
        proc = subprocess.run(["/usr/bin/time", "-f", "%M", self.binary, *args],
                              env=self.env(port), capture_output=True,
                              cwd=cwd or self.root / "workspace", **streams)
        wall = (time.monotonic() - start) * 1000
        rss = int(proc.stderr.decode().strip().splitlines()[-1])
        return {"wall_ms": round(wall), "rss_kib": rss, "exit": proc.returncode,
                "stdout": proc.stdout}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--runs", type=int, default=30)
    parser.add_argument("--transcripts", help="directory of real Claude Code transcripts")
    args = parser.parse_args()
    # Private stores refuse group-writable ancestors, as they should.
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="sr-latency-") as tmp:
        root = pathlib.Path(tmp)
        for name in ["config", "cache", "data", "state", "workspace"]:
            (root / name).mkdir()
        runner = Runner(args.binary, root)
        context = root / "workspace/context.json"
        context.write_text(json.dumps({
            "schema_version": 1, "harness": "claude_code", "producer_id": "measurement",
            "workspace_root": str(root / "workspace"), "session_id": "measure",
            "agent_id": None, "branch_id": None, "context_epoch": None,
            "current_request": {"event_id": "measure-1",
                                "text": "Our rust test suite started failing; find out why.",
                                "attachments_omitted": False,
                                "essential_attachment_missing": False},
            "events": [], "explicit_skill_references": [], "supplied_loads": []}))
        rank = ["rank", "--context", str(context), "--json"]
        roster = len(glob.glob(os.path.expanduser("~/.claude/skills/*/SKILL.md")))
        report = {"binary": runner.binary, "runs": args.runs, "user_skill_files": roster,
                  "host": {"cpus": os.cpu_count(), "load_before": os.getloadavg()}}

        report["offline_cache_miss"] = summary(
            [runner.timed(rank + ["--offline"]) for _ in range(args.runs)])

        server = subprocess.Popen(
            [sys.executable, str(FIXTURE / "provider_server.py"), "useful"],
            stdout=subprocess.PIPE, cwd=FIXTURE)
        port = json.loads(server.stdout.readline())["port"]
        fill = runner.timed(rank + ["--allow-network"], port=port)
        with socket.create_connection(("127.0.0.1", port)) as done:
            done.sendall(b"DONE")
        server.wait(timeout=10)
        report["fixture_fill"] = {"wall_ms": fill["wall_ms"], "exit": fill["exit"]}
        # The endpoint origin is part of the cache key: hit with the fill's.
        hits = [runner.timed(rank + ["--offline"], port=port) for _ in range(args.runs)]
        report["exact_cache_hit"] = summary(hits)
        report["exact_cache_hit"]["cache_hit"] = all(
            json.loads(hit["stdout"]).get("cache", {}).get("hit") for hit in hits)

        if args.transcripts:
            paths = sorted(glob.glob(os.path.join(args.transcripts, "*.jsonl")),
                           key=os.path.getmtime)[-6:]
            samples = []
            for path in paths:
                payload = json.dumps({
                    "hook_event_name": "UserPromptSubmit", "prompt": "continue",
                    "session_id": pathlib.Path(path).stem, "transcript_path": path,
                    "cwd": str(root / "workspace"), "prompt_id": "measure"}).encode()
                for _ in range(max(1, args.runs // len(paths))):
                    samples.append(runner.timed(
                        ["hook", "claude", "--offline", "--no-persist"], stdin=payload))
            report["hook_offline"] = summary(samples)
            report["hook_offline"]["transcript_mib"] = [
                round(os.path.getsize(p) / 2**20, 1) for p in paths]
        report["host"]["load_after"] = os.getloadavg()
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
