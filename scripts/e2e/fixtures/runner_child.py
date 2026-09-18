"""Synthetic real-process fixture, never product or provider evidence."""

import json
import os
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

mode, case, run_id = sys.argv[1:]


def emit(kind, **fields):
    print(json.dumps({"schema_version": 2, "run_id": run_id, "case": case, "kind": kind, **fields}), flush=True)


passed = True
if mode == "signal":
    os.kill(os.getpid(), signal.SIGKILL)
elif mode in {"hang", "childhang"}:
    if mode == "childhang":
        # A descendant that changes its session must still die with the PID namespace.
        subprocess.Popen([sys.executable, "-I", "-B", "-c", "import time; time.sleep(60)",
                          "sr-runner-owned-descendant-" + case],
                         start_new_session=True)
    emit("fault", id="descendant-started" if mode == "childhang" else "timeout-entered")
    time.sleep(60)
elif mode == "flood":
    os.write(1, b"x" * 65536)
    os.write(2, b"y" * 65536)
elif mode == "badjson":
    print("{broken", flush=True)
elif mode == "secret":
    print("SYNTHETIC_SECRET_DO_NOT_LOG\x1b[31m\ncredential=value", file=sys.stderr, flush=True)
elif mode == "isolation":
    passed = (os.getcwd() == "/work" and os.environ.get("HOME") == "/home"
              and not any(key in os.environ for key in ("TYPESAFE_API_KEY", "HTTPS_PROXY", "PYTHONPATH"))
              and not Path("/data").exists() and not Path("/home/ubuntu").exists()
              and not Path("/work/effect").exists())
    Path("/work/effect").write_text("synthetic")
    # This namespace has only its down loopback interface and no external routes.
    passed = passed and socket.if_nameindex() == [(1, "lo")]
    with socket.socket() as sock:
        sock.settimeout(0.1)
        try:
            sock.connect(("192.0.2.1", 443))
        except OSError:
            pass
        else:
            passed = False
    passed = passed and all(Path(os.environ[name]).is_dir() for name in
                            ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "TMPDIR"))

CASE_ASSERTIONS = {
    "full-transport-suite": "p1_gate_passed",
    "consented-live-smoke": "smoke_valid",
    "stalled-dns": "safe_error_reporting",
    "handshake-timeout": "safe_error_reporting",
    "429-backoff": "safe_error_reporting",
    "success-twin": "safe_error_reporting",
    # Context suite (P3)
    "no-cross-session-fallthrough": "exact_session_bound",
    "bounded-normalized-parsing": "normalized_bounded",
    "partial-tail-deferred": "incremental_clean",
    "compaction-detected": "incremental_clean",
    "sibling-branch-isolation": "no_branch_confusion",
    "hook-prompt-overlaid-once": "authoritative_prompt",
    "cass-bounded-pipe": "cass_safe",
    "offline-cass-mock": "cass_safe",
    "loaded-evidence-extracted": "tool_association_exact",
    "visible-head-tail-truncation": "rendered_within_budget",
    "task-anchor-preserved": "anchor_intact",
    "dirty-paths-repo-relative": "bounded_project_signals",
    "minimal-profile-drops-tools": "profile_respected",
    "disclosure-receipt-matches-payload": "receipt_accurate",
    "corrupted-jsonl": "safe_context_rejection",
    "oversized-record": "safe_context_rejection",
    "missing-prompt": "safe_context_rejection",
    "full-context-suite": "p3_gate_passed",
}
assertion_id = CASE_ASSERTIONS.get(case, "behavior")
emit("assertion", id=assertion_id, passed=passed and mode != "failthenpass")
if mode in {"duplicate", "failthenpass"}:
    emit("assertion", id=assertion_id, passed=mode == "failthenpass")
if mode != "noresult":
    emit("result", outcome="refused" if mode == "refuse" else "ok", effects=0,
         fault_reached=mode == "refuse")
sys.exit(7 if mode == "refuse" else 9 if mode == "badexit" else 0)
