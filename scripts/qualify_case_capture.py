#!/usr/bin/env python3
"""Qualify an explicitly selected sr binary on Linux with real export failures.

Run through RCH's non-compilation job lane. Requires unshare, mount, strace,
and CAP_SYS_ADMIN on the test worker. The child owns a private mount namespace
and a 64 KiB tmpfs; no shared filesystem is filled. All inputs are synthetic,
child environments omit credentials, and retained fixtures are not deleted.
This proves CLI/filesystem behavior, not live provider or usefulness quality.
"""

import argparse
import errno
import fcntl
import hashlib
import json
import os
import select
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def qualify(binary):
    binary_hash = digest(binary)
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix="sr-capture-filesystem-", dir="/tmp"))
    workspace = root / "workspace"
    for directory in [
        workspace / ".claude/skills/alpha",
        root / "home",
        root / "config",
        root / "data",
        root / "cache",
        root / "volume",
    ]:
        directory.mkdir(parents=True, mode=0o700)
    canary = "sk-aB7cD8eF9gH0jK1mN2pQ3rS4tU5vW6xY"
    request = f"Review Rust tests. {' ' * 85}{canary}"
    (workspace / ".claude/skills/alpha/SKILL.md").write_text(
        f"---\nname: alpha\ndescription: Review Rust tests. API key {canary}\n---\nTests.\n"
    )
    current = {
        "event_id": "request-1",
        "text": request,
        "attachments_omitted": False,
        "essential_attachment_missing": False,
    }
    event = {
        "event_id": "request-1",
        "parent_id": None,
        "turn_id": "turn-1",
        "agent_id": None,
        "branch_id": None,
        "role": "user",
        "kind": "message",
        "timestamp_unix_ms": None,
        "text": request,
        "tool": None,
    }
    context = {
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": "synthetic-filesystem-qualification",
        "workspace_root": str(workspace),
        "session_id": "synthetic-capture-session",
        "agent_id": None,
        "branch_id": None,
        "context_epoch": None,
        "current_request": current,
        "events": [event],
        "explicit_skill_references": [],
        "supplied_loads": [],
    }
    (workspace / "context.json").write_text(json.dumps(context))
    env = {
        "HOME": str(root / "home"),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "PATH": "/usr/bin:/bin",
    }

    def case_command(target, names=("alpha",), timeout_ms=1000):
        command = [
            str(binary),
            "rank",
            "--context",
            "context.json",
            "--save-case",
            str(target),
            "--offline",
            "--no-cache",
            "--no-ledger",
            "--timeout-ms",
            str(timeout_ms),
            "--json",
        ]
        for name in names:
            command.extend(["--require-skill", name])
        return command

    def run_case(target, delayed=False, names=("alpha",), timeout_ms=1000):
        command = case_command(target, names, timeout_ms)
        if delayed:
            command = [
                "/usr/bin/strace",
                "-o",
                str(root / "late-fsync.trace"),
                "-f",
                "-e",
                "trace=fsync",
                "-e",
                "inject=fsync:delay_enter=1500ms:when=1",
                *command,
            ]
        started = time.monotonic()
        output = subprocess.run(
            command,
            cwd=workspace,
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=15,
            check=False,
        )
        (root / f"{target.name}.stdout.json").write_bytes(output.stdout)
        (root / f"{target.name}.stderr.txt").write_bytes(output.stderr)
        document = json.loads(output.stdout)
        return {
            "exit_code": output.returncode,
            "decision": document["decision"],
            "error_kind": document.get("error", {}).get("kind"),
            "http_attempts": document["usage"]["http_attempts"],
            "elapsed_seconds": time.monotonic() - started,
            "target_exists": target.exists(),
        }

    def check_saved(target, before):
        raw = target.read_bytes()
        case = json.loads(raw)
        require(
            canary.encode() not in raw and canary[:12].encode() not in raw,
            "capture retained synthetic secret text",
        )
        require(b"[REDACTED]" in raw, "capture lacks redaction markers")
        require(
            before
            <= case["local_evidence"]["as_of_unix_ms"]
            <= time.time_ns() // 1_000_000,
            "capture time is not in the wall-clock invocation window",
        )
        require(
            stat.S_IMODE(target.stat().st_mode) == 0o600, "capture is not owner-only"
        )
        return digest(target)

    volume = root / "volume"
    subprocess.run(
        [
            "/usr/bin/mount",
            "-t",
            "tmpfs",
            "-o",
            "size=64k,mode=0700",
            "tmpfs",
            str(volume),
        ],
        env=env,
        timeout=5,
        check=True,
        capture_output=True,
    )
    size = os.statvfs(volume)
    require(
        size.f_blocks * size.f_frsize <= 128 * 1024,
        "refuse to fill a larger filesystem",
    )
    before = time.time_ns() // 1_000_000
    good = run_case(volume / "before-full.json")
    require(
        good["exit_code"] == 0 and good["decision"] == "explicit",
        "successful full-disk counterpart did not complete",
    )
    good["case_sha256"] = check_saved(volume / "before-full.json", before)

    filled = False
    with (volume / "filler").open("wb", buffering=0) as filler:
        for _ in range(64):
            try:
                filler.write(b"x" * 4096)
            except OSError as error:
                if error.errno != errno.ENOSPC:
                    raise
                filled = True
                break
    require(filled, "real tmpfs ENOSPC must be observed before testing the CLI")
    full = run_case(volume / "full.json")
    require(
        (full["exit_code"], full["decision"], full["error_kind"])
        == (9, "unavailable", "storage-failure"),
        "full disk did not return the storage failure",
    )
    require(not full["target_exists"], "failed full-disk capture published a target")
    require(
        digest(volume / "before-full.json") == good["case_sha256"],
        "full-disk failure changed a previous capture",
    )
    # Preserve the volume's bytes outside its process-owned mount namespace.
    snapshot = root / "volume-snapshot"
    snapshot.mkdir(mode=0o700)
    for name in ["before-full.json", "filler"]:
        shutil.copyfile(volume / name, snapshot / name)
        os.chmod(snapshot / name, 0o600)
    require(
        digest(snapshot / "before-full.json") == good["case_sha256"],
        "retained filesystem snapshot changed the successful capture",
    )

    before = time.time_ns() // 1_000_000
    timely = run_case(workspace / "timely.json")
    require(
        timely["exit_code"] == 0 and timely["decision"] == "explicit",
        "successful late-write counterpart did not complete",
    )
    timely["case_sha256"] = check_saved(workspace / "timely.json", before)
    late = run_case(workspace / "late.json", delayed=True)
    require(
        (late["exit_code"], late["decision"], late["error_kind"])
        == (6, "unavailable", "timeout"),
        "late export was not withheld as timeout",
    )
    require(late["elapsed_seconds"] >= 1.5, "real fsync was not delayed")
    require(
        "DELAYED" in (root / "late-fsync.trace").read_text(), "delay was not traced"
    )
    require(
        late["target_exists"],
        "completed export should remain after late success is withheld",
    )
    late["case_sha256"] = check_saved(workspace / "late.json", before)
    # Raced targets arrive after the exclusive private temporary file is written.
    # Delay a real fsync, then create the target while publication is still pending.
    raced = []
    for shape in ["file", "symlink"]:
        directory = workspace / f"race-{shape}"
        directory.mkdir(mode=0o700)
        target = directory / "case.json"
        marker = b"preserve raced-in synthetic content"
        referent = directory / "referent"
        if shape == "symlink":
            referent.write_bytes(marker)
        trace = root / f"race-{shape}.trace"
        argv = [
            "/usr/bin/strace",
            "-o",
            str(trace),
            "-f",
            "-e",
            "trace=fsync",
            "-e",
            "inject=fsync:delay_enter=500ms:when=1",
            *case_command(target, timeout_ms=3000),
        ]
        started = time.monotonic()
        with subprocess.Popen(
            argv,
            cwd=workspace,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ) as process:
            until = started + 2
            while time.monotonic() < until:
                partials = list(directory.glob(".sr-partial-*"))
                if any(p.stat().st_size > 0 for p in partials):
                    break
                require(
                    process.poll() is None, "publisher exited before the target race"
                )
                time.sleep(0.002)
            else:
                process.communicate(timeout=15)
                raise RuntimeError("publisher never reached the private export write")
            require(not target.exists(), "target was published before the race")
            if shape == "symlink":
                target.symlink_to(referent)
            else:
                with target.open("xb") as handle:
                    handle.write(marker)
            stdout, stderr = process.communicate(timeout=15)
        (root / f"race-{shape}.stdout.json").write_bytes(stdout)
        (root / f"race-{shape}.stderr.txt").write_bytes(stderr)
        document = json.loads(stdout)
        require(
            (process.returncode, document["decision"], document["error"]["kind"])
            == (9, "unavailable", "storage-failure"),
            "raced target was not refused",
        )
        require(target.read_bytes() == marker, "raced target was overwritten")
        require(
            target.is_symlink() == (shape == "symlink"), "raced target type changed"
        )
        require("DELAYED" in trace.read_text(), "target race did not delay real fsync")
        require(document["usage"]["http_attempts"] == 0, "race sent a provider attempt")
        raced.append(
            {
                "shape": shape,
                "exit_code": process.returncode,
                "http_attempts": document["usage"]["http_attempts"],
                "target_preserved": True,
            }
        )

    # Large but legal markdown fallback descriptions exercise the export cap.
    # 31 descriptions fit; the same request with the 32nd exceeds 16 MiB.
    names = [f"cap-{index:02}" for index in range(32)]
    for name in names:
        directory = workspace / ".claude/skills" / name
        directory.mkdir(mode=0o700)
        (directory / "SKILL.md").write_text(f"# {name}\n\n" + '"' * 260_000 + "\n")
    large_request = "Review Rust tests. " + '"' * 100_000 + " " + canary
    context["current_request"]["text"] = large_request
    context["events"][0]["text"] = large_request
    (workspace / "context.json").write_text(json.dumps(context))
    before = time.time_ns() // 1_000_000
    cap_ok = run_case(workspace / "cap-ok.json", names=names[:31], timeout_ms=10_000)
    require(
        (cap_ok["exit_code"], cap_ok["decision"]) == (0, "explicit"),
        "within-cap counterpart failed",
    )
    cap_ok["case_sha256"] = check_saved(workspace / "cap-ok.json", before)
    cap_ok["case_bytes"] = (workspace / "cap-ok.json").stat().st_size
    require(cap_ok["case_bytes"] < 16 * 1024 * 1024, "within-cap export exceeded cap")
    cap_bad = run_case(workspace / "cap-bad.json", names=names, timeout_ms=10_000)
    require(
        (cap_bad["exit_code"], cap_bad["decision"], cap_bad["error_kind"])
        == (7, "unavailable", "oversized-input"),
        "capture overflow was not rejected",
    )
    require(not cap_bad["target_exists"], "oversized capture published a target")
    require(
        digest(workspace / "cap-ok.json") == cap_ok["case_sha256"],
        "overflow changed the successful counterpart",
    )

    # The result exceeds PIPE_BUF. Read a real prefix and close its only reader:
    # stdout cannot finish, but the already durable case must remain complete.
    target = workspace / "partial-stdout.json"
    before = time.time_ns() // 1_000_000
    with subprocess.Popen(
        case_command(target, names[:31], 10_000),
        cwd=workspace,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ) as process:
        fcntl.fcntl(process.stdout.fileno(), fcntl.F_SETPIPE_SZ, 4096)
        ready, _, _ = select.select([process.stdout], [], [], 15)
        require(bool(ready), "partial-output publisher produced no output")
        prefix = os.read(process.stdout.fileno(), 128)
        process.stdout.close()
        process.stdout = None
        _, stderr = process.communicate(timeout=15)
    (root / "partial.stdout.prefix").write_bytes(prefix)
    (root / "partial.stderr.txt").write_bytes(stderr)
    require(len(prefix) == 128, "stdout did not deliver the expected real prefix")
    require(process.returncode == 1, "partial stdout write was reported successful")
    try:
        json.loads(prefix)
    except json.JSONDecodeError:
        pass
    else:
        raise RuntimeError("stdout prefix was a complete replacement document")
    partial_hash = check_saved(target, before)
    partial = {
        "exit_code": process.returncode,
        "stdout_prefix_bytes": len(prefix),
        "complete_export_sha256": partial_hash,
        "delivery": "unknown",
    }
    retry = run_case(target, names=names[:31], timeout_ms=10_000)
    require(
        (retry["exit_code"], retry["decision"], retry["error_kind"])
        == (9, "unavailable", "storage-failure"),
        "retry overwrote the completed export",
    )
    require(digest(target) == partial_hash, "partial-output retry changed the export")
    for result in [cap_ok, cap_bad, retry]:
        require(
            result["http_attempts"] == 0, "publication qualification sent inference"
        )

    for result in [good, full, timely, late]:
        require(
            result["http_attempts"] == 0,
            "filesystem qualification sent a provider attempt",
        )
    require(digest(binary) == binary_hash, "binary changed during qualification")
    return {
        "schema_version": 1,
        "status": "passed",
        "binary_sha256": binary_hash,
        "fixture_root": str(root),
        "proof_class": "synthetic Linux CLI and filesystem",
        "quality_gate": "not-applicable",
        "cases_requested": 10,
        "cases_completed": 10,
        "real_enospc_observed": filled,
        "before_full": good,
        "full": full,
        "timely": timely,
        "late": late,
        "raced_targets": raced,
        "cap_success": cap_ok,
        "cap_overflow": cap_bad,
        "partial_stdout": partial,
        "partial_retry": retry,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("qualification requires Linux; no cases were run")
    binary = args.binary.resolve(strict=True)
    metadata = binary.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o022:
        parser.error(
            "binary must be a regular file without group/world write permissions"
        )
    if args.child:
        print(json.dumps(qualify(binary), indent=2))
        return
    with subprocess.Popen(
        [
            "/usr/bin/unshare",
            "--mount",
            "--propagation",
            "private",
            "/usr/bin/python3",
            "-B",
            str(Path(__file__).resolve()),
            "--child",
            "--binary",
            str(binary),
        ],
        env={"PATH": "/usr/bin:/bin"},
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    ) as process:
        try:
            stdout, stderr = process.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=5)
            raise RuntimeError(
                "qualification timed out; its owned process group was stopped"
            ) from None
    sys.stdout.buffer.write(stdout)
    sys.stderr.buffer.write(stderr)
    raise SystemExit(process.returncode)


if __name__ == "__main__":
    main()
