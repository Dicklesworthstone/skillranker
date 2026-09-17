"""Real-process regression for cleanup after a pipe polling failure."""

import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import runner


class CleanupTests(unittest.TestCase):
    def test_reap_error_still_closes_both_pipes(self):
        processes = []
        popen = subprocess.Popen
        cleanup = runner.kill_owned

        def launch(*args, **kwargs):
            process = popen(*args, **kwargs)
            processes.append(process)
            return process

        def cleanup_error(process):
            cleanup(process)
            raise OSError("synthetic reap failure")

        class FailedSelector(selectors.DefaultSelector):
            def select(self, timeout=None):
                raise OSError("synthetic polling failure")

        with patch.object(runner.subprocess, "Popen", side_effect=launch), patch.object(
            runner, "kill_owned", side_effect=cleanup_error
        ), patch.object(
            runner.selectors, "DefaultSelector", FailedSelector
        ):
            with self.assertRaisesRegex(OSError, "synthetic reap failure"):
                runner.invoke(
                    [sys.executable, "-I", "-B", "-c", "print('{}')"],
                    (), time.monotonic() + 10, 8192, 10_000,
                )
        self.assertTrue(processes[0].stdout.closed)
        self.assertTrue(processes[0].stderr.closed)

    def test_poll_failure_after_leader_exit_terminates_descendant(self):
        directory = Path(tempfile.mkdtemp(prefix="sr-cleanup-test-"))
        pid_file = directory / "descendant.pid"
        processes = []
        popen = subprocess.Popen

        def launch(*args, **kwargs):
            process = popen(*args, **kwargs)
            processes.append(process)
            return process

        class FailedSelector(selectors.DefaultSelector):
            def select(self, timeout=None):
                deadline = time.monotonic() + 5
                while os.waitid(os.P_PID, processes[0].pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
                    if time.monotonic() >= deadline:
                        raise TimeoutError("leader did not exit")
                    time.sleep(0.01)
                raise OSError("synthetic polling failure")

        # The leader exits while its child retains both pipes and its process group.
        code = (
            "import os, pathlib, sys, time\n"
            "pid = os.fork()\n"
            "if pid == 0:\n"
            "    time.sleep(30)\n"
            "    os._exit(0)\n"
            "pathlib.Path(sys.argv[1]).write_text(str(pid))\n"
            "os._exit(0)\n"
        )
        try:
            with patch.object(runner.subprocess, "Popen", side_effect=launch), patch.object(
                runner.selectors, "DefaultSelector", FailedSelector
            ):
                with self.assertRaisesRegex(OSError, "synthetic polling failure"):
                    runner.invoke(
                        [sys.executable, "-I", "-B", "-c", code, str(pid_file)],
                        (), time.monotonic() + 10, 8192, 10_000,
                    )
            child = int(pid_file.read_text())
            deadline = time.monotonic() + 2
            alive = True
            while time.monotonic() < deadline:
                try:
                    # An orphan may briefly remain a zombie awaiting the host reaper.
                    state = Path(f"/proc/{child}/stat").read_text().rsplit(")", 1)[1].split()[0]
                    alive = state != "Z"
                except FileNotFoundError:
                    alive = False
                if not alive:
                    break
                time.sleep(0.01)
            self.assertFalse(alive, "descendant survived exceptional cleanup")
            self.assertTrue(processes[0].stdout.closed)
            self.assertTrue(processes[0].stderr.closed)
        finally:
            for process in processes:
                if process.returncode is None:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main(verbosity=2)
