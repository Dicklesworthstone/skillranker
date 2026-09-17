"""Independent adversarial child for the runner-contract certification suite."""

import json
import os
import signal
import subprocess
import sys
import time

mode, case, run_id = sys.argv[1:]


def emit(kind, **fields):
    print(json.dumps({"schema_version": 2, "run_id": run_id, "case": case, "kind": kind, **fields}), flush=True)


if mode == "signal":
    os.kill(os.getpid(), signal.SIGKILL)
if mode in {"hang", "childhang"}:
    if mode == "childhang":
        subprocess.Popen([sys.executable, "-I", "-B", "-c", "import time; time.sleep(60)",
                          "sr-contract-descendant-" + run_id], start_new_session=True)
    emit("fault", id="descendant-started" if mode == "childhang" else "timeout-entered")
    time.sleep(60)
if mode == "flood":
    os.write(1, b"X" * 65536)
if mode == "badjson":
    print("{malformed", flush=True)
if mode == "truncated":
    sys.stdout.write('{"schema_version":1')
    sys.stdout.flush()
    sys.exit(0)
if mode == "foreigncase":
    case = "unplanned-case"
if mode == "secret":
    print("SYNTHETIC_CONTRACT_SECRET\x1b[31m\nRAW_PROVIDER_ERROR credential=value", file=sys.stderr)

passed = mode not in {"falseassert", "failthenpass"}
if mode == "pipeline":
    # Real failing producer plus successful consumer: observing only the last
    # exit would hide the error. No shell and no reimplementation of the runner.
    producer = subprocess.Popen(["/usr/bin/false"], stdout=subprocess.PIPE)
    try:
        consumer = subprocess.run(["/usr/bin/cat"], stdin=producer.stdout,
                                  stdout=subprocess.DEVNULL, timeout=2, check=False)
        producer.stdout.close()
        producer_code = producer.wait(timeout=2)
        passed = producer_code == 0 and consumer.returncode == 0
    finally:
        producer.stdout.close()
        if producer.poll() is None:
            producer.kill()
        producer.wait(timeout=2)

emit("assertion", id="behavior", passed=passed)
if mode in {"duplicate", "failthenpass"}:
    emit("assertion", id="behavior", passed=mode == "failthenpass")
if mode != "noresult":
    emit("result", outcome="refused" if mode in {"refuse", "wrongoutcome"} else "ok",
         effects=1 if mode == "effects" else 0, fault_reached=mode == "refuse")
sys.exit(7 if mode == "refuse" else 9 if mode == "badexit" else 0)
