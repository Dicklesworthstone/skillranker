"""Offline, bounded process runner. No product, provider or native-harness claims."""

import argparse
import fcntl
import hashlib
import hmac
import math
import os
import platform
import selectors
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

# run.sh uses isolated Python; import only this checked-in sibling module.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import evidence as ev

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
FIXTURE = HERE / "fixtures/runner_child.py"
STOP = False
SAFE_ENV = {"PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C"}
SANDBOX = Path("/usr/bin/bwrap")
PRLIMIT = Path("/usr/bin/prlimit")


class SafeParser(argparse.ArgumentParser):
    def error(self, message):
        # argparse normally echoes unknown arguments, which may contain credentials.
        raise ev.InvalidEvidence("arguments")


def digest_file(path, limit=128 * 1024 * 1024):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        ev.require(stat.S_ISREG(info.st_mode) and info.st_size <= limit, "file-limit")
        digest = hashlib.sha256()
        size = 0
        while block := stream.read(65536):
            size += len(block)
            ev.require(size <= limit, "file-limit")
            digest.update(block)
        after = os.fstat(stream.fileno())
        ev.require((info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
                   == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns),
                   "file-changed")
        return digest.hexdigest()


def source_digest(root=ROOT):
    """Hash explicit build/test inputs, including uncommitted and untracked source.

    Operational Beads, docs, ignored credentials and build outputs are not source
    inputs. Never walk the operator home or execute Git status/fsmonitor helpers.
    """
    paths = [root / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    pending = [(root / name, 0) for name in ("src", "tests", "scripts")]
    entries = 0
    while pending:
        directory, depth = pending.pop()
        ev.require(depth <= 32 and not directory.is_symlink(), "source-depth")
        with os.scandir(directory) as children:
            for child in children:
                entries += 1
                ev.require(entries <= 10_000, "source-files")
                if child.name == "__pycache__" or child.name.endswith(".pyc"):
                    continue
                ev.require(not child.is_symlink(), "source-symlink")
                if child.is_dir(follow_symlinks=False):
                    pending.append((Path(child.path), depth + 1))
                else:
                    paths.append(Path(child.path))
    if (root / "build.rs").exists():
        paths.append(root / "build.rs")
    digest = hashlib.sha256()
    total = 0
    for path in sorted(paths):
        total += path.stat(follow_symlinks=False).st_size
        ev.require(total <= 64 * 1024 * 1024, "source-bytes")
        digest.update(ev.encode([str(path.relative_to(root)), digest_file(path, 8 * 1024 * 1024)]))
    return digest.hexdigest()


def identity(binary, fixture=FIXTURE):
    commit = subprocess.run(["/usr/bin/git", "-c", "core.fsmonitor=false", "rev-parse", "HEAD"],
                            cwd=ROOT, env=SAFE_ENV, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=3, check=True)
    return {"source_sha256": source_digest(), "binary_sha256": digest_file(binary),
            "runner_sha256": ev.sha(b"".join(bytes.fromhex(digest_file(HERE / name)) for name in
                                            ("run.sh", "runner.py", "evidence.py"))),
            "fixture_sha256": digest_file(fixture), "lock_sha256": digest_file(ROOT / "Cargo.lock"),
            "toolchain_sha256": digest_file(ROOT / "rust-toolchain.toml"),
            "git_commit": commit.stdout.decode("ascii").strip(), "platform": sys.platform,
            "architecture": platform.machine(), "python": platform.python_version(),
            "features": [], "binary_role": "fixture-interpreter"}


def sandbox_command(binary, fixture):
    args = [str(SANDBOX), "--unshare-pid", "--unshare-net", "--unshare-ipc", "--unshare-uts",
            "--die-with-parent", "--cap-drop", "ALL", "--clearenv",
            "--ro-bind", "/usr", "/usr", "--proc", "/proc", "--dev", "/dev"]
    for name in ("lib", "lib64", "bin", "sbin"):
        path = Path("/") / name
        if path.is_symlink():
            args += ["--symlink", os.readlink(path), str(path)]
        elif path.is_dir():
            args += ["--ro-bind", str(path), str(path)]
    for path in ("/tmp", "/work", "/home", "/state"):
        args += ["--size", "1048576", "--tmpfs", path]
    for path in ("/state/config", "/state/data", "/state/cache"):
        args += ["--dir", path]
    for key, value in {**SAFE_ENV, "HOME": "/home", "TMPDIR": "/tmp",
                       "XDG_CONFIG_HOME": "/state/config", "XDG_DATA_HOME": "/state/data",
                       "XDG_CACHE_HOME": "/state/cache"}.items():
        args += ["--setenv", key, value]
    args += ["--perms", "0555", "--ro-bind-data", str(binary), "/tested-binary",
             "--perms", "0444", "--ro-bind-data", str(fixture), "/fixture.py", "--chdir", "/work"]
    return args


def limited_command(command, timeout_ms):
    # prlimit execs in our owned process group. Avoid Python preexec_fn, which
    # would be unsafe if an embedding caller had started another thread.
    # bwrap's read-only interpreter copy needs up to 128 MiB; writable child
    # files remain confined to the four 1 MiB tmpfs mounts.
    cpu = math.ceil(timeout_ms / 1000) + 1
    return [str(PRLIMIT), "--core=0:0", "--fsize=134217728:134217728",
            "--as=536870912:536870912", f"--cpu={cpu}:{cpu}", "--", *command]


def kill_owned(process):
    if process.returncode is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    # Bubblewrap's private PID namespace also kills descendants that call setsid.
    process.wait(timeout=2)


def exited_without_reaping(process):
    # Keep the leader (even a zombie) reserved until killpg has finished. poll()
    # would reap it and permit PID/PGID reuse before the group signal.
    return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None


def invoke(command, descriptors, deadline, output_limit, timeout_ms):
    """Drain both pipes without retaining stderr; kill and reap on every failure."""
    started = time.monotonic()
    process = subprocess.Popen(limited_command(command, timeout_ms), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, cwd="/", env=SAFE_ENV, close_fds=True,
                               pass_fds=descriptors, start_new_session=True)
    counts = [0, 0]
    output = bytearray()
    reason = None
    try:
        with selectors.DefaultSelector() as poller:
            for index, pipe in enumerate((process.stdout, process.stderr)):
                os.set_blocking(pipe.fileno(), False)
                poller.register(pipe, selectors.EVENT_READ, index)
            while poller.get_map() or not exited_without_reaping(process):
                if STOP:
                    reason = "interrupted"
                    break
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    reason = "timeout"
                    break
                for key, _ in poller.select(min(remaining, 0.05)):
                    block = os.read(key.fd, 8192)
                    if not block:
                        poller.unregister(key.fileobj)
                        continue
                    counts[key.data] += len(block)
                    if sum(counts) > output_limit:
                        reason = "output-limit"
                        break
                    if key.data == 0:
                        output.extend(block)
                if reason:
                    break
        if reason is None and time.monotonic() >= deadline:
            reason = "timeout"
    finally:
        try:
            kill_owned(process)
        finally:
            process.stdout.close()
            process.stderr.close()
    # Bubblewrap propagates a killed namespace child as the shell-style 128+signal
    # status; direct termination of Bubblewrap itself has a negative return code.
    if (process.returncode < 0 or process.returncode >= 128) and reason is None:
        reason = "signal"
    records = []
    try:
        ev.require(output.endswith(b"\n"), "truncated-child")
        records = [ev.decode(line) for line in output.splitlines()]
    except ev.InvalidEvidence:
        if reason is None:
            reason = "protocol"
    finished = time.monotonic()
    # Cleanup and decoding belong to the case deadline too. A cancellation or
    # expired deadline in either phase must not produce a successful terminal.
    if reason is None:
        reason = "interrupted" if STOP else "timeout" if finished >= deadline else None
    elapsed = max(0, int((finished - started) * 1000))
    return records, process.returncode, reason, elapsed, counts


def open_input(path, expected_hash):
    """Seal the bytes we hashed before execution, eliminating path/ABA races."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        ev.require(stat.S_ISREG(info.st_mode) and info.st_size <= 128 * 1024 * 1024, "input-file")
        sealed = os.memfd_create("sr-runner-input", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
        try:
            digest, size = hashlib.sha256(), 0
            while block := stream.read(65536):
                size += len(block)
                ev.require(size <= 128 * 1024 * 1024, "input-limit")
                digest.update(block)
                view = memoryview(block)
                while view:
                    written = os.write(sealed, view)
                    ev.require(written > 0, "input-write")
                    view = view[written:]
            ev.require(hmac.compare_digest(digest.hexdigest(), expected_hash), "input-changed")
            fcntl.fcntl(sealed, fcntl.F_ADD_SEALS, fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW
                        | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
            os.lseek(sealed, 0, os.SEEK_SET)
            return sealed
        except BaseException:
            os.close(sealed)
            raise


def run(spec, binary, artifacts, selection=None, *, fixture=FIXTURE, run_id=None):
    global STOP
    STOP = False
    spec = ev.manifest(spec)
    binary = Path(binary).resolve(strict=True)
    fixture = Path(fixture).resolve(strict=True)
    before = identity(binary, fixture)
    run_id = "run-" + uuid.uuid4().hex if run_id is None else run_id
    ev.identifier(run_id)
    planned = [case["id"] for case in spec["cases"]]
    selected = planned if selection is None else selection
    ev.identifiers(selected)
    ev.require(set(selected) <= set(planned), "selection")
    # Select in declared order, regardless of the caller's flag order.
    selected = [case for case in planned if case in selected]
    artifacts = Path(artifacts)
    ev.require(artifacts.is_dir(), "artifacts-parent")
    directory = Path(tempfile.mkdtemp(prefix="sr-e2e-", dir=artifacts))
    os.chmod(directory, 0o700)
    events = []
    descriptor = os.open(directory / "events.jsonl", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    budget = 0

    def emit(event):
        nonlocal budget
        event = {"seq": len(events), "run_id": run_id, **event}
        data = ev.encode(event)
        budget += len(data)
        ev.require(budget <= ev.MAX_DOCUMENT, "artifact-limit")
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
        events.append(event)

    old_handlers = {}

    def interrupted(signum, frame):
        global STOP
        STOP = True

    for signum in (signal.SIGINT, signal.SIGTERM):
        old_handlers[signum] = signal.signal(signum, interrupted)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            supported = (sys.platform == "linux" and hasattr(os, "memfd_create")
                         and SANDBOX.is_file() and PRLIMIT.is_file())
            binary_fd = open_input(binary, before["binary_sha256"]) if supported else None
            try:
                fixture_fd = open_input(fixture, before["fixture_sha256"]) if supported else None
            except BaseException:
                if binary_fd is not None:
                    os.close(binary_fd)
                raise
            try:
                command = sandbox_command(binary_fd, fixture_fd)
                # Probe the SAME mounts, namespaces and interpreter used by cases.
                # Suppress all raw sandbox diagnostics. Unsupported hosts are blocked.
                try:
                    available = False
                    if supported:
                        probe = invoke(command + ["--", "/tested-binary", "-I", "-B", "-c", "print('{}')"],
                                       (binary_fd, fixture_fd), time.monotonic() + 2, 8192, 2000)
                        available = probe[1] == 0 and probe[2] is None and probe[0] == [{}]
                except (OSError, subprocess.SubprocessError):
                    available = False
                header = {"schema_version": ev.VERSION, "kind": "header", "suite": spec["suite"],
                          "tier": spec["tier"], "planned": planned, "selected": selected,
                          "manifest_sha256": ev.sha(ev.encode(spec)), "identity": before,
                          "network": "isolated" if available else "unavailable",
                          "artifact_limit": ev.MAX_DOCUMENT, "seed": 0}
                emit(header)
                deadline = time.monotonic() + spec["timeout_ms"] / 1000
                for case in spec["cases"]:
                    if case["id"] not in selected:
                        continue
                    reason = ("interrupted" if STOP else "sandbox-unavailable" if not available
                              else "aggregate-deadline" if time.monotonic() >= deadline else None)
                    result = {"kind": "case_result", "case": case["id"], "status": "blocked",
                              "reason": reason, "exit_code": None, "elapsed_ms": 0,
                              "stdout_bytes": 0, "stderr_bytes": 0, "assertions": [], "attempt": 1,
                              "observed": None, "fault_observed": False, "fault_witness": None}
                    if reason is None:
                        emit({"kind": "case_start", "case": case["id"], "step": "child-execution",
                              "expected": ev.expectations(case)})
                        try:
                            # bind-data consumes FDs; rewind for every independent sandbox.
                            os.lseek(binary_fd, 0, os.SEEK_SET)
                            os.lseek(fixture_fd, 0, os.SEEK_SET)
                            records, code, transport, elapsed, counts = invoke(
                                command + ["--", "/tested-binary", "-I", "-B", "/fixture.py",
                                           case["mode"], case["id"], run_id], (binary_fd, fixture_fd),
                                min(deadline, time.monotonic() + case["timeout_ms"] / 1000),
                                case["output_bytes"], case["timeout_ms"])
                            reason, assertions, observed = ev.classify(case, records, code, transport, run_id=run_id)
                            witness = ev.fault_witness(case, records, run_id)
                            result.update(exit_code=code, elapsed_ms=elapsed, stdout_bytes=counts[0],
                                          stderr_bytes=counts[1], assertions=assertions, observed=observed,
                                          fault_observed=witness is not None, fault_witness=witness)
                        except (OSError, subprocess.SubprocessError):
                            reason = "spawn"
                        result.update(status="passed" if reason == "matched" else "failed", reason=reason)
                    emit(result)
            finally:
                for descriptor in (binary_fd, fixture_fd):
                    if descriptor is not None:
                        os.close(descriptor)
        stable = identity(binary, fixture) == before
        summary = ev.summarize(events[0], events, stable)
        descriptor = os.open(directory / "summary.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(ev.encode(summary))
            stream.flush()
            os.fsync(stream.fileno())
        ev.validate(directory, spec, before, expected_run_id=run_id)
        return directory, summary
    finally:
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)


def main():
    parser = SafeParser(description=__doc__)
    parser.add_argument("--suite", required=True, choices=["runner-smoke", "runner-contract", "transport"])
    parser.add_argument("--binary", default=sys.executable,
                        help="Python interpreter for runner-mechanics fixtures; not a product binary")
    parser.add_argument("--artifacts", required=True, help="existing artifact parent directory")
    parser.add_argument("--case", action="append", dest="selection")
    try:
        args = parser.parse_args()
        binary = Path(args.binary).resolve(strict=True)
        if args.suite == "runner-contract":
            ev.require(args.selection is None, "contract-selection")
            import runner_contract
            directory, summary = runner_contract.run_contract(binary, args.artifacts)
        else:
            spec = ev.read_json(HERE / "suites" / (args.suite + ".json"))
            directory, summary = run(spec, binary, args.artifacts, args.selection)
        # Generated basename only: caller-supplied paths/arguments never become log text.
        print(ev.encode({"schema_version": ev.VERSION, "run": directory.name,
                         "runner_status": summary["runner_status"], "product_gate": "not-applicable"})
              .decode("ascii"), end="")
        return {"passed": 0, "partial": 3, "blocked": 4}.get(summary["runner_status"], 1)
    except (ev.InvalidEvidence, OSError, ValueError, subprocess.SubprocessError):
        print('{"schema_version":2,"runner_status":"incomplete","error":"runner-input-or-io"}',
              file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
