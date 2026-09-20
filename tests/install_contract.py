"""Installer boundary tests. Fixture executables test installer behavior, not sr.

Set SR_INSTALL_TEST_BINARY to a real release sr for the additional smoke test.
Scratch directories are deliberately retained under repository deletion policy.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"


def executable(version="0.1.0", bad=False):
    if bad:
        return b"#!/bin/sh\nexit 17\n"
    capabilities = json.dumps({"commands": [
        {"name": "rank", "status": "implemented"},
        {"name": "doctor", "status": "implemented"},
        {"name": "hook", "status": "planned"},
    ]})
    return (f"#!/bin/sh\ncase \"$1\" in\n"
            f"--version) echo 'sr {version}';;\n"
            f"capabilities) echo '{capabilities}';;\n"
            "demo) echo '{\"actionable\":false}';;\n"
            "*) exit 2;;\nesac\n").encode()


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="sr-install-contract-"))
        self.home = self.root / "home"
        self.home.mkdir()
        self.dest = self.root / "bin with 'quotes $literal; spaces"
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("TYPESAFE_", "SR_", "XDG_"))}
        self.env.update(HOME=str(self.home), SHELL="/bin/bash", NO_COLOR="1")
        # Airgap tests fail loudly if any network/build command is invoked.
        tools = self.root / "tools"
        tools.mkdir()
        for name in ("curl", "git", "cargo", "rch"):
            p = tools / name
            p.write_text("#!/bin/sh\necho unexpected-network-or-build >&2\nexit 77\n")
            p.chmod(0o755)
        self.env["PATH"] = str(tools) + os.pathsep + os.environ["PATH"]

    def archive(self, members=None, binary=None):
        path = self.root / ("archive-" + str(len(list(self.root.glob("*.gz")))) + ".tar.gz")
        members = members if members is not None else [("sr", binary or executable(), None)]
        with tarfile.open(path, "w:gz") as t:
            for name, data, kind in members:
                info = tarfile.TarInfo(name)
                info.mode = 0o755
                if kind:
                    info.type = kind
                    info.linkname = "/tmp/should-not-be-followed"
                else:
                    info.size = len(data)
                t.addfile(info, io.BytesIO(data) if not kind else None)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        path.with_name(path.name + ".sha256").write_text(f"{digest}  {path.name}\n")
        return path

    def run_install(self, archive, *args, success=True):
        p = subprocess.run(["bash", str(INSTALLER), "--offline", str(archive),
                            "--dest", str(self.dest), "--keep-temp", "--no-gum", *args],
                           env=self.env, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        self.assertEqual(p.returncode == 0, success, p.stderr.decode())
        self.assertNotIn(b"unexpected-network-or-build", p.stdout + p.stderr)
        self.assertFalse((self.dest / ".sr-install.lock").exists())
        return p

    def test_quiet_install_repeat_backup_and_modes(self):
        archive = self.archive()
        first = self.run_install(archive, "--quiet", "--no-configure", "--verify")
        self.assertEqual(first.stdout + first.stderr, b"")
        self.assertEqual((self.dest / "sr").stat().st_mode & 0o777, 0o755)
        self.run_install(archive, "--no-configure")
        self.assertFalse(list(self.dest.glob("sr.backup.*")))
        self.run_install(archive, "--force", "--no-configure")
        self.assertEqual(len(list(self.dest.glob("sr.backup.*"))), 1)

    def test_bad_digest_and_failed_probe_preserve_existing(self):
        good = self.archive()
        self.run_install(good, "--no-configure")
        before = (self.dest / "sr").read_bytes()
        for archive, flags in [(good, ["--sha256", "0" * 64]),
                               (self.archive(binary=executable(bad=True)), []),
                               (good, ["--version", "9.9.9"])]:
            self.run_install(archive, "--no-configure", *flags, success=False)
            self.assertEqual((self.dest / "sr").read_bytes(), before)

    def test_unsafe_archive_members_are_rejected_before_execution(self):
        for members in [
            [("../escape", b"bad", None), ("sr", executable(), None)],
            [("sr", b"", tarfile.SYMTYPE)],
            [("sr", b"", tarfile.LNKTYPE)],
            [("sr", executable(), None), ("sr", executable(), None)],
            [("sr", executable(), None), ("LICENSE", b"x" * 262145, None)],
            [("unexpected", b"bad", None)],
        ]:
            self.run_install(self.archive(members), "--no-configure", success=False)
            self.assertFalse((self.dest / "sr").exists())
        self.assertFalse((self.root / "escape").exists())

    def test_integrations_preserve_user_files_and_quote_path(self):
        (self.home / ".claude").mkdir()
        (self.home / ".codex").mkdir()
        rc = self.home / ".bashrc"
        rc.write_text("# user's existing config\n")
        archive = self.archive()
        self.run_install(archive, "--easy-mode")
        skill = self.home / ".codex/skills/skillranker/SKILL.md"
        self.assertIn("TYPESAFE_API_KEY", skill.read_text())
        skill.write_text("user customized skill\n")
        self.run_install(archive, "--easy-mode")
        self.assertEqual(skill.read_text(), "user customized skill\n")
        self.assertEqual(len(list(self.home.glob(".bashrc.sr-backup.*"))), 1)
        p = subprocess.run(["bash", "-c", 'source "$1"; command -v sr', "test", str(rc)],
                           env=self.env, capture_output=True, check=True, timeout=10)
        self.assertEqual(p.stdout.decode().strip(), str(self.dest / "sr"))
        self.assertFalse((self.home / ".claude/settings.json").exists())
        completion = self.home / ".local/share/bash-completion/completions/sr"
        self.assertIn("rank", completion.read_text())
        self.assertNotIn("hook", completion.read_text())

    def test_required_signature_cannot_silently_skip(self):
        archive = self.archive()
        key = self.root / "key.pub"
        key.write_text("not a valid trusted key")
        self.run_install(archive, "--cosign-key", str(key), success=False)
        self.assertFalse((self.dest / "sr").exists())

    def test_unsupported_platform_stops_before_acquisition_or_writes(self):
        uname = self.root / "tools/uname"
        for platform in ("Darwin", "FreeBSD"):
            uname.write_text(f"#!/bin/sh\ncase \"$1\" in -s) echo {platform};; -m) echo arm64;; esac\n")
            uname.chmod(0o755)
            for mode in ([], ["--from-source"], ["--offline", str(self.archive())]):
                p = subprocess.run(["bash", str(INSTALLER), "--dest", str(self.dest),
                                    "--keep-temp", *mode], env=self.env,
                                   capture_output=True, timeout=10)
                self.assertNotEqual(p.returncode, 0)
                self.assertIn(b"requires Linux", p.stderr)
                self.assertNotIn(b"unexpected-network-or-build", p.stderr)
                self.assertFalse(self.dest.exists())

    def test_optional_shell_failure_preserves_successful_install(self):
        target = self.home / "managed-shell-config"
        target.write_text("# externally managed shell configuration\n")
        (self.home / ".bashrc").symlink_to(target)
        p = self.run_install(self.archive(), "--easy-mode", "--no-configure")
        self.assertIn(b"Binary installed; optional configuration failed", p.stderr)
        self.assertTrue((self.home / ".bashrc").is_symlink())
        self.assertEqual(target.read_text(), "# externally managed shell configuration\n")
        version = subprocess.run([str(self.dest / "sr"), "--version"],
                                 capture_output=True, check=True, timeout=10)
        self.assertEqual(version.stdout.strip(), b"sr 0.1.0")

    def test_oversized_archive_and_ambiguous_checksum_are_rejected(self):
        archive = self.root / "oversized.tar.gz"
        with archive.open("wb") as f:
            f.truncate(200 * 1024 * 1024 + 1)
        self.run_install(archive, "--no-configure", success=False)
        self.assertFalse((self.dest / "sr").exists())
        archive = self.archive()
        sidecar = archive.with_name(archive.name + ".sha256")
        sidecar.write_text(sidecar.read_text() * 2)
        self.run_install(archive, "--no-configure", success=False)
        self.assertFalse((self.dest / "sr").exists())

    @unittest.skipUnless(shutil.which("cosign"), "cosign unavailable")
    def test_real_signature_accepts_key_and_rejects_tampered_artifact(self):
        archive = self.archive()
        env = dict(self.env, COSIGN_PASSWORD="")
        key = self.root / "test-key"
        for args in [
            ["cosign", "generate-key-pair", "--output-key-prefix", str(key)],
            ["cosign", "sign-blob", "--key", str(key)+".key", "--tlog-upload=false",
             "--bundle", str(archive)+".sigstore.json", str(archive)],
        ]:
            subprocess.run(args, env=env, capture_output=True, check=True, timeout=30)
        self.run_install(archive, "--cosign-key", str(key)+".pub", "--no-configure")
        before = (self.dest / "sr").read_bytes()
        # Attacker can replace the archive AND its checksum but not the signature.
        other = self.archive(binary=executable("0.2.0"))
        archive.write_bytes(other.read_bytes())
        archive.with_name(archive.name+".sha256").write_text(
            hashlib.sha256(archive.read_bytes()).hexdigest()+"  "+archive.name+"\n")
        self.run_install(archive, "--cosign-key", str(key)+".pub", "--no-configure", success=False)
        self.assertEqual((self.dest / "sr").read_bytes(), before)

    def test_lock_is_not_stolen_even_with_force(self):
        self.dest.mkdir()
        lock = self.dest / ".sr-install.lock"
        lock.mkdir()
        for pid in [str(os.getpid()), "99999999", "invalid"]:
            (lock / "pid").write_text(pid)
            p = subprocess.run(["bash", str(INSTALLER), "--offline", str(self.archive()),
                                "--dest", str(self.dest), "--force", "--keep-temp", "--quiet"],
                               env=self.env, capture_output=True, timeout=30)
            self.assertNotEqual(p.returncode, 0)
            self.assertEqual((lock / "pid").read_text(), pid)
            self.assertFalse((self.dest / "sr").exists())
            if pid == "99999999": self.assertIn(b"Stale install lock", p.stderr)

    def test_downgrade_requires_explicit_intent(self):
        self.run_install(self.archive(binary=executable("9.0.0")), "--no-configure")
        old = self.archive()
        self.run_install(old, "--no-configure", success=False)
        self.run_install(old, "--no-configure", "--version", "0.1.0")

    def test_unknown_flag_has_no_effects(self):
        p = subprocess.run(["bash", str(INSTALLER), "--desst", str(self.dest)],
                           env=self.env, capture_output=True, timeout=30)
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse(self.dest.exists())

    def test_remote_source_failure_never_runs_local_cargo(self):
        source = self.root / "source with spaces"
        source.mkdir()
        (source / "Cargo.lock").write_text("version = 4\n")
        (source / "rust-toolchain.toml").write_text('[toolchain]\nchannel="stable"\n')
        event = self.root / "remote-invoked"
        rch = self.root / "tools/rch"
        rch.write_text("#!/bin/sh\n"
                       'test "${RCH_REQUIRE_REMOTE:-}" = 1 || exit 90\n'
                       'test -z "${TYPESAFE_API_KEY+x}" || exit 91\n'
                       f'touch "{event}"\n'
                       'echo intentional-remote-failure >&2\nexit 23\n')
        p = subprocess.run(["bash", str(INSTALLER), "--source", str(source),
                            "--dest", str(self.dest), "--keep-temp", "--no-configure", "--quiet"],
                           env=dict(self.env, TYPESAFE_API_KEY="fixture-secret"),
                           capture_output=True, timeout=30)
        self.assertNotEqual(p.returncode, 0)
        self.assertTrue(event.exists())
        self.assertFalse((self.dest / "sr").exists())
        logs = list(self.dest.glob(".sr-install.*/build.log"))
        self.assertEqual(len(logs), 1)
        self.assertIn("intentional-remote-failure", logs[0].read_text())
        self.assertNotIn("unexpected-network-or-build", logs[0].read_text())

    @unittest.skipUnless(os.environ.get("SR_INSTALL_TEST_BINARY"), "real release binary not supplied")
    def test_real_release_binary(self):
        binary = Path(os.environ["SR_INSTALL_TEST_BINARY"]).read_bytes()
        self.run_install(self.archive(binary=binary), "--verify", "--no-configure")
        p = subprocess.run([str(self.dest / "sr"), "demo", "--case", "none", "--json"],
                           env=self.env, capture_output=True, check=True, timeout=10)
        result = json.loads(p.stdout)
        self.assertFalse(result["actionable"])
        self.assertEqual(hashlib.sha256(binary).digest(),
                         hashlib.sha256((self.dest / "sr").read_bytes()).digest())


if __name__ == "__main__":
    unittest.main()
