#!/usr/bin/env python3
"""Black-box tests against real Git repositories and native FUSE or NFS mounts.

On Linux, pass --require-fuse. On macOS, pass --require-nfs MountTests.
Native tests exercise the real filesystem through the same fixtures.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import json
import os
import pty
import select
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import termios
import time
import unittest

from support.git_invocations import git_invocations, record_git_invocations

ROOT = Path(__file__).resolve().parents[1]
PARSER = argparse.ArgumentParser(description=__doc__)
PARSER.add_argument("--binary", type=Path, default=ROOT / "target/debug/git-on")
PARSER.add_argument("--require-fuse", action="store_true")
PARSER.add_argument("--require-nfs", action="store_true", help="Require a real Darwin NFSv4.1 mount in mount tests")
PARSER.add_argument("--require-nfs-tmpdir", action="store_true", help="Require a Linux NFS-backed TMPDIR for cleanup tests")
PARSER.add_argument("--uv", action="store_true", help="Also run network-backed uv examples")
OPTIONS, UNITTEST_ARGS = PARSER.parse_known_args()
BINARY = OPTIONS.binary.resolve()
FUSE_AVAILABLE = Path("/dev/fuse").exists() and bool(shutil.which("fusermount3") or shutil.which("fusermount"))
MOUNT_AVAILABLE = FUSE_AVAILABLE or OPTIONS.require_nfs
MOUNT_ARGS = ["--backend=nfs"] if OPTIONS.require_nfs else ["--backend=fuse"]
fuse_only = unittest.skipUnless(FUSE_AVAILABLE and not OPTIONS.require_nfs, "requires the Linux FUSE interface")


def git(repo, *args, **kwargs):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, **kwargs).stdout.decode().strip()

def run_record(run):
    return run.parent / f".git-on-run-{os.geteuid():x}-{run.name}"

def linux_file_handle_available(path):
    import ctypes

    class FileHandle(ctypes.Structure):
        _fields_ = [("size", ctypes.c_uint), ("kind", ctypes.c_int),
                    ("bytes", ctypes.c_ubyte * 128)]

    lookup = getattr(ctypes.CDLL(None), "name_to_handle_at", None)
    if lookup is None:
        return False
    handle = FileHandle(size=128)
    mount = ctypes.c_int()
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        return lookup(fd, b"", ctypes.byref(handle), ctypes.byref(mount), 0x1000) == 0  # AT_EMPTY_PATH
    finally:
        os.close(fd)

@contextmanager
def controlling_terminal():
    master, slave = pty.openpty()
    terminal = os.ttyname(slave)

    def attach():
        os.setsid()
        descriptor = os.open(terminal, os.O_RDWR | os.O_NOCTTY)
        try:
            fcntl.ioctl(descriptor, termios.TIOCSCTTY, 0)
        finally:
            os.close(descriptor)

    try:
        yield attach
    finally:
        os.close(slave)
        os.close(master)


class RepositoryTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="git-on-test-")).resolve()
        self.addCleanup(self.remove_fixture)
        self.repo = self.root / "repository"
        self.repo.mkdir()
        git(self.repo, "init", "--quiet")
        git(self.repo, "config", "user.email", "test@example.invalid")
        git(self.repo, "config", "user.name", "Integration Test")
        self.put(".gitignore", "out/\n*.cache\n__pycache__/\n.venv/\n!keep.cache\n/only-root/\n**/.gitignore\n")
        self.put("tracked.txt", "committed\n")
        self.put("nested/.gitignore", "*.tmp\n!keep.tmp\n")
        self.put("nested/module.py", "VALUE = 42\n")
        self.put("plain/source.py", "VALUE = 1\n")
        self.put("out/protected.txt", "tracked despite ignore\n")
        self.put("subtree/.gitignore", "generated/\n")
        self.put("subtree/source.txt", "subtree source\n")
        (self.repo / "tracked-link").symlink_to("tracked.txt")
        git(self.repo, "add", "--force", "--all")
        git(self.repo, "add", "--force", "out/protected.txt")
        git(self.repo, "commit", "--quiet", "-m", "fixture")
        self.tree = git(self.repo, "rev-parse", "HEAD^{tree}")
        self.system_tmp = self.root / "system-tmp"
        self.system_tmp.mkdir()

    def remove_fixture(self):
        if OPTIONS.require_nfs:
            prefix = b" on " + os.fsencode(self.root) + b"/"
            mounts = subprocess.check_output(["/sbin/mount"]).splitlines()
            self.assertFalse(any(prefix in line for line in mounts),
                             f"preserving fixture containing a mount at {self.root}")
        shutil.rmtree(self.root)

    def put(self, name, content):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def environment(self, temp_root=None):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith("GIT_ON_")}
        env["TMPDIR"] = str(temp_root or self.system_tmp)
        env["XDG_DATA_HOME"] = str(self.root / "data")
        env["XDG_CACHE_HOME"] = str(self.root / "cache")
        env["GIT_CONFIG_GLOBAL"] = os.devnull
        env["GIT_CONFIG_NOSYSTEM"] = "1"
        return env

    def invoke(self, *command, tree="HEAD", input=None, timeout=60, extra_env=None):
        return self.invoke_argv([tree, "--", *map(str, command)],
                                input=input, timeout=timeout, extra_env=extra_env)

    def invoke_argv(self, args, input=None, timeout=60, extra_env=None, preexec_fn=None):
        before = set((self.system_tmp / "git-on").glob("??????"))
        argv = [str(BINARY), *args]
        env = dict(self.environment(), **(extra_env or {}))
        if OPTIONS.require_nfs:
            # Give an interrupted native test time to unmount its own share.
            # subprocess.run's timeout sends SIGKILL and can leave a hard mount.
            with subprocess.Popen(argv, cwd=self.repo, env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, preexec_fn=preexec_fn) as process:
                try:
                    stdout, stderr = process.communicate(input, timeout=timeout)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    try:
                        process.communicate(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                    raise
            result = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
        else:
            result = subprocess.run(argv, cwd=self.repo, env=env, input=input,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout,
                                    preexec_fn=preexec_fn)
        created = set((self.system_tmp / "git-on").glob("??????")) - before
        self.assertLessEqual(len(created), 1, created)
        return result, next(iter(created), None)

    def python(self, code, **kwargs):
        return self.invoke(sys.executable, "-c", code, **kwargs)

    def cleanup_run(self, run, timeout=15, *, umask=-1):
        return subprocess.run([str(BINARY), "--cleanup", str(run)],
                              cwd=self.repo, env=self.environment(),
                              capture_output=True, timeout=timeout, umask=umask)

    def race_cleanup(self, run, count=4):
        cleaners = [subprocess.Popen([str(BINARY), "--cleanup", str(run)], cwd=self.repo,
                                     env=self.environment(), stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE) for _ in range(count)]
        return [(cleaner, cleaner.communicate(timeout=20)) for cleaner in cleaners]

    def require_linux_nfs_backing(self):
        if sys.platform != "linux":
            self.skipTest("Linux NFS client behavior")
        storage = subprocess.check_output(["stat", "-f", "-c", "%T", self.system_tmp]).strip()
        if storage not in (b"nfs", b"nfs4"):
            if OPTIONS.require_nfs_tmpdir:
                self.fail("requires NFS-backed TMPDIR")
            self.skipTest("requires NFS-backed TMPDIR")

    @contextmanager
    def hold_cleanup_entry(self, path, symlink=False):
        if symlink:
            path.symlink_to("missing-target")
            flags = os.O_PATH | os.O_NOFOLLOW
        else:
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        fd = os.open(path, flags, 0o600)
        try:
            yield
        finally:
            os.close(fd)

    def require_linux_nfs_deferred_unlink(self, symlink=False):
        self.require_linux_nfs_backing()
        probe = self.root / f"unlink-probe-{symlink}"
        with self.hold_cleanup_entry(probe, symlink):
            probe.unlink()
            if not any(path.is_symlink() == symlink for path in self.root.glob(".nfs*")):
                if OPTIONS.require_nfs_tmpdir and not symlink:
                    self.fail("the Linux NFS client must defer unlink of the open file")
                self.skipTest("NFS preserves this unlinked entry without a placeholder")

    def assert_no_runtime_files(self):
        self.assertEqual(list(self.system_tmp.iterdir()), [])
        self.assertFalse((self.root / "data/git-on").exists())
        self.assertFalse((self.root / "cache/git-on").exists())
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))


class MountedRepositoryTest(RepositoryTest):
    def invoke_argv(self, args, **kwargs):
        return super().invoke_argv([*MOUNT_ARGS, *args], **kwargs)


class CliTests(RepositoryTest):
    @unittest.skipUnless(sys.platform == "darwin", "regression at the Darwin path limit")
    def test_terminal_paths_fit_a_long_tmpdir(self):
        temporary = self.system_tmp
        while len(os.fsencode(temporary)) + 200 < 986:
            temporary /= "x" * 99
            temporary.mkdir()
        temporary /= "x" * (985 - len(os.fsencode(temporary)))
        temporary.mkdir()
        with controlling_terminal() as attach:
            result, _ = self.invoke_argv([
                "HEAD:plain", "--backend=copy", "--write-policy=all", "--", sys.executable, "-c", "pass",
            ], extra_env={"TMPDIR": str(temporary)}, preexec_fn=attach)
        self.assert_success(result)
        self.assertEqual(list(temporary.iterdir()), [])

    @unittest.skipIf(os.geteuid() == 0, "requires ordinary directory permissions")
    def test_reports_failure_removing_empty_temporary_group(self):
        try:
            result, _ = self.invoke_argv([
                "HEAD", "--backend=copy", "--write-policy=all", "--", sys.executable, "-c",
                "import os; os.chmod(os.environ['TMPDIR'], 0o500)",
            ])
        finally:
            self.system_tmp.chmod(0o700)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b"remove temporary git-on group", result.stderr)
        self.assertEqual(list((self.system_tmp / "git-on").iterdir()), [])

    def test_failed_group_setup_leaves_tmpdir_usable(self):
        args = ["HEAD", "--backend=copy", "--write-policy=all", "--", sys.executable, "-c", "pass"]
        failed, _ = self.invoke_argv(args, preexec_fn=lambda: os.umask(0o100))
        self.assertEqual(failed.returncode, 125, failed.stderr)
        self.assert_no_runtime_files()
        result, _ = self.invoke_argv(args)
        self.assert_success(result)
        self.assert_no_runtime_files()

    def test_group_lock_does_not_delay_run(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        descriptor = os.open(group, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            result, _ = self.invoke_argv([
                "HEAD", "--backend=copy", "--write-policy=all", "--", sys.executable, "-c", "pass",
            ], timeout=5)
            self.assert_success(result)
            self.assert_no_runtime_files()
        finally:
            os.close(descriptor)

    def test_empty_tmpdir_uses_current_directory(self):
        if sys.platform == "linux":
            self.repo.chmod(self.repo.stat().st_mode | stat.S_ISGID)
        result, _ = self.invoke_argv([
            "HEAD", "--backend=copy", "--write-policy=all", "--",
            sys.executable, "-c", "from pathlib import Path; print(Path.cwd().parent.parent)",
        ], extra_env={"TMPDIR": ""})
        self.assert_success(result)
        self.assertEqual(Path(os.fsdecode(result.stdout).strip()), self.repo / "git-on")
        self.assertFalse((self.repo / "git-on").exists())

    def test_uses_private_temporary_group_when_name_is_a_file(self):
        occupied = self.system_tmp / "git-on"
        occupied.write_bytes(b"existing file")
        result, _ = self.invoke_argv([
            "HEAD", "--backend=copy", "--write-policy=all", "--", "/bin/cat", "tracked.txt",
        ])
        self.assert_success(result)
        self.assertEqual(result.stdout, (self.repo / "tracked.txt").read_bytes())
        self.assertEqual(occupied.read_bytes(), b"existing file")
        self.assertEqual(list(self.system_tmp.iterdir()), [occupied])

    def test_refuses_symlinked_temporary_group(self):
        target = self.root / "other-storage"
        target.mkdir()
        group = self.system_tmp / "git-on"
        group.symlink_to(target, target_is_directory=True)
        result, run = self.invoke_argv([
            "HEAD", "--backend=copy", "--write-policy=all", "--", sys.executable, "-c", "pass",
        ])
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b"must be a real directory", result.stderr)
        self.assertIsNone(run)
        self.assertTrue(group.is_symlink())
        self.assertEqual(list(target.iterdir()), [])

    def test_explicit_backend_selects_copy_and_unavailable_mount_does_not_fall_back(self):
        unavailable = "fuse" if sys.platform == "darwin" else "nfs"
        result, _ = self.invoke_argv([
            "HEAD", "--backend=" + unavailable, "--write-policy=all", "--", "/bin/echo", "must not run",
        ])
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertEqual(result.stdout, b"")
        result, _ = self.invoke_argv([
            "--backend=copy", "HEAD", "--write-policy=all", "--", "/bin/cat", "tracked.txt",
        ])
        self.assert_success(result)
        self.assertEqual(result.stdout, b"committed\n")
        self.assert_no_runtime_files()

    def test_forwarded_term_preserves_the_childs_handled_exit_status(self):
        for backend in ['--backend=copy', *(MOUNT_ARGS if MOUNT_AVAILABLE else [])]:
            for handled in (True,False):
                with self.subTest(backend=backend,handled=handled):
                    code="import signal,sys\n"
                    if handled:code+="signal.signal(signal.SIGTERM,lambda *args: sys.exit(42))\n"
                    code+="print('ready',flush=True)\nsignal.pause()\n"
                    env=self.environment()
                    runner=subprocess.Popen([str(BINARY),'HEAD',backend,'--write-policy=all','--',sys.executable,'-I','-S','-c',code],
                                            cwd=self.repo,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                    try:
                        ready,_,_=select.select([runner.stdout],[],[],10)
                        self.assertTrue(ready,'child did not become ready')
                        self.assertEqual(runner.stdout.readline(),b'ready\n')
                        runner.send_signal(signal.SIGTERM)
                        _,stderr=runner.communicate(timeout=10)
                        self.assertEqual(runner.returncode,42 if handled else -signal.SIGTERM,stderr)
                        self.assert_no_runtime_files()
                    finally:
                        if runner.poll() is None:runner.kill()
                        runner.communicate(timeout=10)


    def test_help_and_version(self):
        for arg in ("--help", "-h", "--version"):
            result = subprocess.run([str(BINARY), arg], capture_output=True, timeout=10)
            self.assert_success(result)
            self.assertTrue(b"git-on" in result.stdout or b"git on" in result.stdout)
            if arg != "--version":
                self.assertIn(b"Usage: git on [REPOSITORY] REV [OPTIONS] -- COMMAND [ARGS...]", result.stdout)
                self.assertIn(b"Arguments:", result.stdout)
                self.assertIn(b"Options:", result.stdout)
                self.assertNotIn(b"--keep", result.stdout)
                self.assertIn(b"--verify", result.stdout)
                self.assertIn(b"Local REV, or REPOSITORY REV", result.stdout)
                self.assertIn(b"native Git", result.stdout)
                self.assertIn(b"Runner options may appear anywhere before the first --", result.stdout)
                self.assertIn(b"first -- is required before every command", result.stdout)
        env = dict(self.environment(), PATH=str(BINARY.parent) + os.pathsep + os.environ["PATH"])
        result = subprocess.run(["git", "on", "-h"], cwd=self.repo, env=env,
                                capture_output=True, timeout=10)
        self.assert_success(result)
        self.assertIn(b"Usage: git on [REPOSITORY] REV [OPTIONS] -- COMMAND [ARGS...]", result.stdout)
        self.assertEqual(result.stderr, b"")

    def test_unknown_tree_never_executes(self):
        sentinel = self.root / "executed"
        result, _ = self.python(f"open({str(sentinel)!r}, 'w').close()", tree="missing-tree-ish")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(sentinel.exists())
        self.assert_no_runtime_files()

    def test_invalid_arguments_never_execute_or_create_runtime_files(self):
        for args in [[], ["--keep"], ["--keep", "HEAD"], ["--cleanup"],
                     ["--cleanup", str(self.root), "extra"], ["--unknown"]]:
            with self.subTest(args=args):
                result = subprocess.run([str(BINARY), *args], cwd=self.repo,
                                        env=self.environment(), capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assert_no_runtime_files()

    def test_explicit_option_errors_never_resolve_tree_or_execute(self):
        sentinel = self.root / "executed"
        command = [sys.executable, "-c", f"open({str(sentinel)!r}, 'w').close()"]
        for args in [
            ["-k"], ["--"], ["HEAD"], ["HEAD", "--"],
            ["HEAD", "--keep", "--"], ["HEAD", "-k", "--"],
            ["HEAD", ""], ["HEAD", "--", ""],
            ["--keep=1", "missing-tree", "--", *command],
            ["missing-tree", "--keep=1", "--", *command],
            ["repository", "missing-tree", "extra", "--", *command],
            ["repository", "-k", "missing-tree", "extra", "--", *command],
            ["repository", "missing-tree", "--verify", "extra", "--", *command],
            ["repository", "-k", "missing-tree", "--verify", "extra", "--", *command],
            ["--verify=true", "missing-tree", "--", *command],
            ["--", "HEAD", "--", *command],
        ]:
            with self.subTest(args=args):
                result, run = self.invoke_argv(args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(b"Usage:", result.stderr)
                self.assertIsNone(run)
                self.assertFalse(sentinel.exists())
                self.assert_no_runtime_files()

    def test_unknown_dash_operands_are_git_inputs_instead_of_runner_usage_errors(self):
        sentinel = self.root / "executed"
        command = [sys.executable, "-c", f"open({str(sentinel)!r}, 'w').close()"]
        for operands in (["--unknown", "missing-tree"], ["-kk", "missing-tree"],
                         ["missing-tree", "--unknown"],
                         ["missing-tree", "-kk"], ["-V", "missing-tree"],
                         ["--ver", "missing-tree"], ["missing-tree", "--help"]):
            with self.subTest(operands=operands):
                result, run = self.invoke_argv([*operands, "--", *command])
                self.assertEqual(result.returncode, 125, result.stderr)
                self.assertNotIn(b"Usage: git on", result.stderr)
                self.assertFalse(sentinel.exists())
                self.assertIsNone(run)
                self.assertEqual(list(self.system_tmp.iterdir()), [])
                self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

        revision = "--internal-service-memory"
        git(self.repo, "update-ref", "refs/tags/" + revision, "HEAD")
        self.assertEqual(git(self.repo, "rev-parse", "--verify", "--end-of-options", revision),
                         git(self.repo, "rev-parse", "HEAD"))
        result, _ = self.invoke_argv([revision, "--backend=copy", "--write-policy=all", "--", *command])
        self.assertEqual((result.returncode, result.stdout), (0, b""), result.stderr)
        self.assertTrue(sentinel.exists())
        self.assert_no_runtime_files()

    def test_empty_revision_is_rejected_by_git_before_command_execution(self):
        sentinel = self.root / "executed"
        result, run = self.invoke_argv([
            "", "--", sys.executable, "-c",
            f"open({str(sentinel)!r}, 'w').close()",
        ])
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertNotIn(b"Usage:", result.stderr)
        self.assertIn(b"fatal:", result.stderr)
        self.assertIsNone(run)
        self.assertFalse(sentinel.exists())
        self.assert_no_runtime_files()

    def test_option_looking_local_repository_is_read_as_a_path(self):
        (self.repo / "--source").symlink_to(self.repo, target_is_directory=True)
        expected = git(self.repo, "-C", "--source", "show", "HEAD:tracked.txt")
        result, run = self.invoke_argv([
            "--source", "HEAD", "--backend=copy", "--write-policy=all", "--",
            sys.executable, "-c", "from pathlib import Path; print(Path('tracked.txt').read_text(), end='')",
        ])
        self.assert_success(result)
        self.assertEqual(result.stdout.decode().strip(), expected)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_control_modes_preserve_first_token_precedence_and_literal_paths(self):
        for args in (["--help", "--nope"], ["-h", "--version", "--"],
                     ["--version", "--help", "junk"]):
            result, run = self.invoke_argv(args)
            self.assert_success(result)
            self.assertEqual(result.stderr, b"")
            self.assertIsNone(run)
            if args[0] == "--version":
                version = subprocess.run([str(BINARY), "--version"], capture_output=True, check=True).stdout
                self.assertEqual(result.stdout, version)
            else:
                self.assertIn(b"Options:", result.stdout)
            self.assert_no_runtime_files()
        for path in ("--", "--help", "--version", "-k", "", b"path-\xff"):
            result, run = self.invoke_argv(["--cleanup", path])
            self.assertEqual(result.returncode, 125, result.stderr)
            self.assertEqual(result.stdout, b"")
            self.assertIsNone(run)
            self.assert_no_runtime_files()

    def test_direct_command_syntax_is_rejected_before_execution(self):
        sentinel = self.root / "executed"
        command = [sys.executable, "-c", f"open({str(sentinel)!r}, 'w').close()"]
        for prefix in (["HEAD"], ["--keep", "HEAD"], ["-k", "HEAD"], ["HEAD:subtree"]):
            with self.subTest(prefix=prefix):
                result, run = self.invoke_argv([*prefix, *command])
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(b"missing -- separator", result.stderr)
                self.assertIsNone(run)
                self.assertFalse(sentinel.exists())
                self.assert_no_runtime_files()
        result, run = self.invoke_argv(["HEAD", *command, "--", "child-argument"])
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn(b"Usage:", result.stderr)
        self.assertIsNone(run)
        self.assertFalse(sentinel.exists())
        self.assert_no_runtime_files()

    def test_cleanup_rejects_unrelated_missing_and_symlinked_directories(self):
        unrelated = self.root / "unrelated"
        unrelated.mkdir()
        sentinel = unrelated / "sentinel"
        sentinel.write_text("preserve")
        alias = self.root / "alias"
        alias.symlink_to(unrelated, target_is_directory=True)
        for target in [unrelated, alias, self.root / "missing"]:
            with self.subTest(target=target):
                result = self.cleanup_run(target)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(sentinel.read_text(), "preserve")
                self.assertEqual(list(unrelated.iterdir()), [sentinel])
                self.assertTrue(alias.is_symlink())
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        for name, mode in [("g12345", 0o700), ("g54321", 0o1700)]:
            with self.subTest(mode=oct(mode)):
                lookalike = group / name
                lookalike.mkdir(mode=0o700)
                lookalike.chmod(mode)
                (lookalike / ".lock").touch()
                (lookalike / "sentinel").write_text("preserve")
                self.assertNotEqual(self.cleanup_run(lookalike).returncode, 0)
                self.assertEqual((lookalike / "sentinel").read_text(), "preserve")

    def test_cleanup_rejects_malformed_state_without_deleting_contents(self):
        run = self.system_tmp / "git-on-unrecognized"
        run.mkdir(mode=0o700)
        (run / ".lock").touch(mode=0o600)
        (run / ".state").write_text("this is not cleanup metadata")
        (run / "backing").mkdir()
        sentinel = run / "backing/valuable"
        sentinel.write_text("preserve")
        result = self.cleanup_run(run)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_text(), "preserve")

    def test_cleanup_recovers_an_interrupted_lock_publication(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        run = group / "g12345"
        run.mkdir(mode=0o700)
        marker = run / ".git-on-lock-new"
        marker.touch(mode=0o600)
        with marker.open("r+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            blocked = self.cleanup_run(run)
            self.assertNotEqual(blocked.returncode, 0)
            self.assertIn(b"active or locked", blocked.stderr)
        sentinel = run / "valuable"
        sentinel.write_bytes(b"keep")
        self.assertNotEqual(self.cleanup_run(run).returncode, 0)
        self.assertEqual(sentinel.read_bytes(), b"keep")
        self.assertTrue(marker.is_file())
        sentinel.unlink()
        recovered = self.cleanup_run(run)
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertFalse(run.exists())
        self.assertFalse(group.exists())

    @unittest.skipUnless(sys.platform == "darwin", "uses Darwin's immutable directory flag")
    def test_cleanup_can_retry_after_the_parent_refuses_directory_removal(self):
        for name in (".git-on-lock-new", ".git-on-lock-v2"):
            with self.subTest(marker=name), tempfile.TemporaryDirectory(dir=self.system_tmp) as sub:
                group = Path(sub) / "git-on"
                group.mkdir(mode=0o700)
                run = group / "g12345"
                run.mkdir(mode=0o700)
                marker = run / name
                marker.touch(mode=0o600)
                flags = group.stat().st_flags
                os.chflags(group, flags | stat.UF_IMMUTABLE)
                try:
                    denied = self.cleanup_run(run)
                    self.assertNotEqual(denied.returncode, 0, denied.stderr)
                finally:
                    os.chflags(group, flags)
                self.assertTrue(marker.is_file())
                self.assert_success(self.cleanup_run(run))
                self.assertFalse(run.exists())
                self.assertFalse(group.exists())

    @unittest.skipUnless(sys.platform == "darwin", "uses Darwin's immutable file flag")
    def test_cleanup_resumes_with_the_same_parked_marker(self):
        for orphaned, mode in ((False, 0o700), (True, 0o700), (True, 0o1770)):
            with self.subTest(orphaned=orphaned, mode=oct(mode)), tempfile.TemporaryDirectory(dir=self.system_tmp) as sub:
                group = Path(sub) / "git-on"
                group.mkdir(mode=0o700)
                group.chmod(mode)
                run = group / "g12345"
                run.mkdir(mode=0o700)
                marker = run / ".git-on-lock-v2"
                marker.touch(mode=0o600)
                inode = marker.stat().st_ino
                child = run / "protected"
                child.write_bytes(b"still here")
                flags = child.stat().st_flags
                os.chflags(child, flags | stat.UF_IMMUTABLE)
                try:
                    denied = self.cleanup_run(run)
                    self.assertNotEqual(denied.returncode, 0, denied.stderr)
                    self.assertEqual(child.read_bytes(), b"still here")
                finally:
                    os.chflags(child, flags)
                [parked] = [path for path in group.iterdir() if path.is_file()]
                self.assertEqual(parked.stat().st_ino, inode)
                if orphaned:
                    child.unlink()
                    run.rmdir()
                self.assert_success(self.cleanup_run(run))
                self.assertFalse(run.exists())
                self.assertFalse(parked.exists())
                self.assertEqual(group.exists(), mode == 0o1770)

    @unittest.skipUnless(sys.platform == "darwin", "uses Darwin's immutable file flag")
    def test_flat_legacy_cleanup_can_retry_after_a_failed_unlink(self):
        for blocked in ("payload", ".state", "parent"):
            with self.subTest(blocked=blocked):
                run = self.system_tmp / ("legacy-flat-" + blocked.lstrip("."))
                run.mkdir(mode=0o700)
                lock = run / ".lock"
                lock.touch(mode=0o600)
                inode = lock.stat().st_ino
                state = run / ".state"
                record = json.dumps({"format": "git-on-temp-v1", "id": "legacy-fixture"})
                state.write_text(record)
                (run / "payload").write_text("keep")
                if blocked == "parent":
                    mode = stat.S_IMODE(self.system_tmp.stat().st_mode)
                    self.system_tmp.chmod(mode & ~0o200)
                else:
                    protected = run / blocked
                    flags = protected.stat().st_flags
                    os.chflags(protected, flags | stat.UF_IMMUTABLE)
                try:
                    failed = self.cleanup_run(run, umask=0o200 if blocked == "parent" else -1)
                    self.assertNotEqual(failed.returncode, 0, failed.stderr)
                    if blocked == "parent":
                        self.assertEqual(lock.stat().st_ino, inode)
                        self.assertEqual(state.read_text(), record)
                    else:
                        [parked] = list(self.system_tmp.glob(".git-on-flat-*"))
                        self.assertFalse(lock.exists())
                        self.assertEqual(parked.stat().st_ino, inode)
                    if blocked == ".state":
                        self.assertEqual(state.read_text(), record)
                    if blocked == "payload":
                        self.assertEqual(protected.read_text(), "keep")
                finally:
                    if blocked == "parent":
                        self.system_tmp.chmod(mode)
                    else:
                        os.chflags(protected, flags)
                self.assert_success(self.cleanup_run(run))
                self.assertFalse(run.exists())
                self.assertFalse(list(self.system_tmp.glob(".git-on-flat-*")))

    @unittest.skipUnless(sys.platform == "darwin", "regression at the Darwin path limit")
    def test_cleanup_accepts_long_run_path_with_private_lock(self):
        parent = self.system_tmp.resolve()
        while len(os.fsencode(parent)) < 1000:
            remaining = 1000 - len(os.fsencode(parent))
            step = min(81, remaining)
            if remaining - step == 1:
                step -= 1
            parent /= "d" * (step - 1)
            parent.mkdir()
        group = parent / "git-on"
        group.mkdir(mode=0o700)
        run = group / "g12345"
        run.mkdir(mode=0o700)
        directory = os.open(run, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fd = os.open(".git-on-lock-v2", os.O_CREAT | os.O_EXCL | os.O_WRONLY,
                         0o600, dir_fd=directory)
            os.close(fd)
        finally:
            os.close(directory)
        self.assertEqual(len(os.fsencode(run)), 1014)
        self.assert_success(self.cleanup_run(run))
        self.assertFalse(run.exists())

        group.mkdir(mode=0o700)
        record = ".git-on-remove-g12345-ready-11111111-1111-1111-1111-111111111111-retry"
        group_fd = os.open(group, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.mkdir(record, mode=0o700, dir_fd=group_fd)
            record_fd = os.open(record, os.O_RDONLY | os.O_DIRECTORY, dir_fd=group_fd)
            try:
                fd = os.open(".nfs0123456789abcdef00000001", os.O_CREAT | os.O_EXCL | os.O_WRONLY,
                             0o600, dir_fd=record_fd)
                os.close(fd)
            finally:
                os.close(record_fd)
        finally:
            os.close(group_fd)
        self.assert_success(self.cleanup_run(run))
        self.assertFalse(group.exists())

    def test_cleanup_respects_an_older_starting_run(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        run = group / "g12345"
        run.mkdir(mode=0o700)
        marker = run / ".git-on-lock-new"
        marker.touch(mode=0o600)
        directory = os.open(run, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(directory, fcntl.LOCK_EX | fcntl.LOCK_NB)
            denied = self.cleanup_run(run)
            self.assertNotEqual(denied.returncode, 0, denied.stderr)
            self.assertIn(b"active or locked", denied.stderr)
            self.assertTrue(marker.exists())
        finally:
            os.close(directory)
        self.assert_success(self.cleanup_run(run))
        self.assertFalse(group.exists())

    def test_failed_startup_with_an_unusable_umask_removes_its_record(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        result = subprocess.run(
            [str(BINARY), "HEAD", "--backend=copy", "--write-policy=all", "--", "/bin/true"],
            cwd=self.repo, env=self.environment(), capture_output=True, timeout=10, umask=0o100,
        )
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assert_no_runtime_files()

    def test_cleanup_reports_an_unexpected_retirement_file(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        run = group / "g12345"
        self.assertNotEqual(self.cleanup_run(run).returncode, 0)
        self.assertTrue(group.is_dir())
        retry_record = group / ".git-on-remove-g12345-ready-11111111-1111-1111-1111-111111111111-retry"
        retry_record.mkdir(mode=0o700)
        retry_record.chmod(0o2700)
        placeholder = retry_record / ".nfs0123456789abcdef00000001"
        placeholder.write_bytes(b"unrecognized contents")
        placeholder.chmod(0o600)
        short_name = retry_record / ".nfs1"
        short_name.touch(mode=0o600)
        unrelated = group / "unrelated"
        unrelated.touch()
        deferred = self.cleanup_run(run)
        self.assertNotEqual(deferred.returncode, 0)
        self.assertIn(b"Manual recovery is required", deferred.stderr)
        self.assertIn(os.fsencode(retry_record.resolve()), deferred.stderr)
        self.assertEqual(placeholder.read_bytes(), b"unrecognized contents")
        self.assertTrue(short_name.is_file())
        self.assertTrue(retry_record.is_dir())
        self.assertTrue(unrelated.is_file())
        placeholder.unlink()
        short_name.unlink()
        unrelated.unlink()
        self.assert_success(self.cleanup_run(run))
        self.assertFalse(group.exists())

    def test_cleanup_reclaims_an_empty_placeholder_after_a_client_crash(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        run = group / "g12345"
        placeholders = (".nfs0123456789abcdef00000001", ".nfs.20051026.0001")
        for last, placeholder in enumerate(placeholders, start=1):
            name = f".git-on-remove-g12345-ready-11111111-1111-1111-1111-11111111111{last}-retry"
            retry_record = group / name
            retry_record.mkdir(mode=0o700)
            (retry_record / placeholder).touch(mode=0o600)
        self.assert_success(self.cleanup_run(run))
        self.assertFalse(group.exists())

    def test_legacy_cleanup_can_retry_after_masked_retirement_directory(self):
        self.require_linux_nfs_backing()
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        run = group / "g12345"
        marker = group / ".git-on-remove-g12345-ready-11111111-1111-1111-1111-111111111111"
        marker.touch(mode=0o600)
        failed = self.cleanup_run(run, umask=0o100)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(list(group.iterdir()), [marker])
        self.assert_success(self.cleanup_run(run))
        self.assertFalse(group.exists())

    def test_concurrent_cleanup_removes_only_the_orphaned_record(self):
        group = self.system_tmp / "git-on"
        group.mkdir(mode=0o700)
        run = group / "r12345"
        run_record(run).mkdir(mode=0o700)
        other = run_record(group / "r12346")
        other.mkdir(mode=0o700)
        results = self.race_cleanup(run, count=2)
        # Concurrent rmdir calls can all succeed on the same empty directory.
        self.assertTrue(any(cleaner.returncode == 0 for cleaner, _ in results), results)
        self.assertEqual(list(group.iterdir()), [other])

    def test_copy_status_when_nfs_defers_removing_run_contents(self):
        self.require_linux_nfs_deferred_unlink()
        for closed_stderr, symlink in ((False, False), (True, False), (False, True)):
            with self.subTest(closed_stderr=closed_stderr, symlink=symlink):
                if symlink:
                    self.require_linux_nfs_deferred_unlink(symlink=True)
                self.check_copy_status_with_held_nfs_file(closed_stderr, symlink)

    def check_copy_status_with_held_nfs_file(self, closed_stderr, symlink):
        gate = self.root / f"finish-{closed_stderr}-{symlink}"
        command = """
import os, sys, time
print(os.getcwd(), flush=True)
deadline = time.monotonic() + 10
while not os.path.exists(sys.argv[1]):
    if time.monotonic() >= deadline:
        raise TimeoutError('parent did not release the command')
    time.sleep(0.01)
raise SystemExit(23)
"""
        runner = subprocess.Popen(
            [str(BINARY), "HEAD", "--backend=copy", "--write-policy=all", "--",
             sys.executable, "-c", command, str(gate)],
            cwd=self.repo, env=self.environment(), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            self.assertTrue(select.select([runner.stdout], [], [], 10)[0], "copy did not start")
            line = runner.stdout.readline().strip()
            self.assertTrue(line, "copy exited before it could report its working directory")
            run = Path(os.fsdecode(line)).parent
            self.assertEqual(run.parent, self.system_tmp / "git-on")
            [lock] = list(run_record(run).glob(".git-on-lock-v2-*"))
            can_recover = not lock.name.startswith(".git-on-lock-v2-u")
            if OPTIONS.require_nfs_tmpdir or linux_file_handle_available(run):
                self.assertTrue(can_recover, "the writable marker must record the available identity")
            inode = lock.stat().st_ino
            directory = run
            if closed_stderr:
                directory = run / "nested"
                directory.mkdir()
            with self.hold_cleanup_entry(directory / "open-runtime-file", symlink):
                saved_mode = stat.S_IMODE(self.system_tmp.stat().st_mode)
                self.system_tmp.chmod(0o500)
                try:
                    if closed_stderr:
                        runner.stderr.close()
                        runner.stderr = None
                    gate.touch()
                    _, stderr = runner.communicate(timeout=10)
                finally:
                    self.system_tmp.chmod(saved_mode)
                self.assertEqual(runner.returncode, 23, stderr)
                if not closed_stderr:
                    self.assertIn(b"cleanup is pending", stderr)
                self.assertTrue(any(path.is_symlink() == symlink for path in directory.glob(".nfs*")))
                self.assertEqual(lock.stat().st_ino, inode)
                retry = self.cleanup_run(run)
                self.assertNotEqual(retry.returncode, 0, retry)
                if can_recover:
                    self.assertIn(b"cleanup is pending", retry.stderr)
                else:
                    if not closed_stderr:
                        self.assertIn(b"Manual recovery is required", stderr)
                    self.assertRegex(
                        retry.stderr, rb"persistent run directory identity|does not match its ownership record"
                    )
                    self.assertTrue(run.is_dir())
                    self.assertEqual(lock.stat().st_ino, inode)
            deadline = time.monotonic() + 3
            if not can_recover:
                # The fixture owns the directory and can remove it after the
                # NFS client releases the held file.
                while any(directory.glob(".nfs*")) and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertEqual(list(directory.glob(".nfs*")), [])
                return
            while True:
                retry = self.cleanup_run(run)
                if retry.returncode == 0 or time.monotonic() >= deadline:
                    break
                time.sleep(0.02)
            self.assert_success(retry)
            self.assert_no_runtime_files()
        finally:
            gate.touch()
            if runner.poll() is None:
                runner.kill()
                runner.communicate(timeout=10)

@unittest.skipUnless(sys.platform == "linux", "Linux mount namespaces")
class MountNamespaceTests(RepositoryTest):
    def test_cleanup_preserves_record_with_a_mounted_subdirectory(self):
        from filesystem_quota import private_mount_namespace

        prefix, uid, gid = private_mount_namespace(self)
        driver = r"""
import os, select, subprocess, sys
from pathlib import Path

binary, temporary, uid, gid = sys.argv[1:]
uid, gid = int(uid), int(gid)
os.environ.update(TMPDIR=temporary, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
command = [binary]
if uid:
    command = ["setpriv", "--reuid", str(uid), "--regid", str(gid), "--clear-groups", binary]
group = Path(temporary) / "git-on"
for run_present in (False, True):
    if run_present:
        child = "import os, sys; print(os.getcwd(), flush=True); sys.stdin.buffer.read(1)"
        with subprocess.Popen([*command, "HEAD", "--backend=copy", "--write-policy=all", "--",
                               sys.executable, "-c", child], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE) as runner:
            try:
                assert select.select([runner.stdout], [], [], 10)[0], "copy did not start"
                cwd = runner.stdout.readline().strip()
                assert cwd, runner.stderr.read().decode(errors="replace")
                run = Path(os.fsdecode(cwd)).parent
                assert run.parent == group, run
                runner.kill()
            finally:
                try:
                    runner.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    runner.kill()
                    runner.communicate(timeout=5)
                    raise
        record = group / f".git-on-run-{uid:x}-{run.name}"
        [marker] = record.glob(".git-on-lock-v2-*")
        if marker.name.startswith(".git-on-lock-v2-u"):
            # Recovery on this filesystem stops at the identity check.
            # The orphan case above exercises the mount guard independently.
            result = subprocess.run([*command, "--cleanup", run], capture_output=True, timeout=10)
            assert result.returncode and b"persistent run directory identity" in result.stderr, result
            assert marker.is_file() and run.is_dir(), result
            continue
    else:
        group.mkdir(mode=0o700)
        run = group / "r12345"
        record = group / f".git-on-run-{uid:x}-{run.name}"
        record.mkdir(mode=0o700)
        marker = record / ".git-on-lock-v2"
        marker.touch(mode=0o600)
        if uid:
            for path in (group, record, marker):
                os.chown(path, uid, gid)
    volume = record / "volume"
    volume.mkdir()
    subprocess.run(["mount", "--bind", volume, volume], check=True)
    try:
        result = subprocess.run([*command, "--cleanup", run], capture_output=True, timeout=10)
        assert marker.is_file() and run.exists() == run_present, (run_present, result.stderr)
        assert result.returncode and b"a mount remains below" in result.stderr, result
    finally:
        subprocess.run(["umount", volume], check=True)
        volume.rmdir()
    subprocess.run([*command, "--cleanup", run], check=True, timeout=10)
"""
        environment = self.environment()
        # The namespace helper can invoke sudo, which drops TMPDIR.
        environment.pop("TMPDIR")
        result = subprocess.run(
            [*prefix, sys.executable, "-c", driver, str(BINARY), str(self.system_tmp),
             str(uid), str(gid)], cwd=self.repo, env=environment,
            capture_output=True, timeout=45,
        )
        self.assert_success(result)


@unittest.skipUnless(MOUNT_AVAILABLE, "real FUSE or NFS mount required")
class MountTests(MountedRepositoryTest):
    def serve_repository(self):
        from remote import Fixture

        server = Fixture(self.repo / '.git')
        self.addCleanup(server.close)
        return server.https(), server.env['GIT_SSL_CAINFO']

    def lazy_remote_stat_fixture(self):
        size=7*1024*1024
        (self.repo/'large').write_bytes(b'x'*size)
        git(self.repo,'add','large');git(self.repo,'commit','-qm','lazy remote decode disk budget')
        return self.serve_repository()

    @fuse_only
    def test_lazy_remote_stat_keeps_decoded_payload_in_memory(self):
        source, ca = self.lazy_remote_stat_fixture()
        def write_bytes():
            return int(dict(line.split(': ',1) for line in Path('/proc/self/io').read_text().splitlines())['write_bytes'])
        before=write_bytes()
        probe=self.system_tmp/'accounting-probe';probe.write_bytes(b'x'*65536);probe.unlink()
        if write_bytes()-before<65536:
            self.skipTest('temporary filesystem does not account block writes; tmpfs quota is checked separately')
        # /proc accounts disk writes by every runner thread and reaped child.
        # Socket writes are excluded. A complete decoder
        # staging copy followed by raw publication exceeds this one-copy budget
        # even if both files disappear before stat returns to the command.
        command=[sys.executable,'-I','-S','-c',r'''
import os
from pathlib import Path
def written():
 return int(dict(line.split(': ',1) for line in Path(f'/proc/{os.getppid()}/io').read_text().splitlines())['write_bytes'])
before=written()
size=Path('large').stat().st_size
assert size==7*1024*1024,size
count=written()-before
assert count<1024*1024,('stat wrote decoded payloads',count,size)
print(size)
''']
        env=dict(self.environment(), GIT_CONFIG_COUNT='0', GIT_SSL_CAINFO=ca)
        clone=self.root/'clone'
        result=subprocess.run(['git','clone','--no-local','--quiet',str(self.repo),str(clone)],
                              env=env,capture_output=True,timeout=20)
        self.assert_success(result)
        direct=subprocess.run(command,cwd=clone,env=env,capture_output=True,timeout=20)
        self.assert_success(direct)
        result=subprocess.run([str(BINARY),source,'HEAD','--write-policy=none','--',*command],
                              cwd=self.repo,env=env,capture_output=True,timeout=30)
        self.assert_success(result);self.assertEqual(result.stdout,direct.stdout)
        self.assert_no_runtime_files()

    @fuse_only
    def test_lazy_remote_stat_succeeds_on_nine_mib_tmpfs(self):
        from filesystem_quota import compare_on_tmpfs
        source, ca = self.lazy_remote_stat_fixture()
        command = ['stat', '-c', '%s', 'large']
        compare_on_tmpfs(self,self.root,BINARY,self.repo,os.environ['PATH'],command,
                         source=source,write_policy='none',tls_ca=ca)
        self.assert_no_runtime_files()

    source_mutations=('read','write','truncate','read-write','read-truncate','read-rewrite','read-truncate-extend','read-replace')

    def add_spill_blobs(self, directory):
        spill=directory/'spill'
        spill.mkdir()
        # Load one more than the hot-cache size so the spill has completed.
        for index in range(9):
            (spill/str(index)).write_bytes(bytes([index])*(64*1024+1))

    def writable_open_fixture(self,mutation='read',source='local',spill=False):
        (self.repo/'large').write_bytes(b'x'*(6*1024*1024))
        if spill:self.add_spill_blobs(self.repo)
        git(self.repo,'add','.');git(self.repo,'commit','-qm','writable open disk budget')
        from filesystem_quota import STORAGE_OBSERVATION
        return [sys.executable,'-I','-S','-c',STORAGE_OBSERVATION+r'''
import sys
mutation=sys.argv[-1]
source=Path('large');before=source.stat()
for flags in (os.O_RDWR,os.O_WRONLY|os.O_APPEND):
 fd=os.open(source,flags)
 assert os.write(fd,b'')==0
 os.fsync(fd);os.close(fd)
after=source.stat()
assert (after.st_size,after.st_mtime_ns)==(before.st_size,before.st_mtime_ns)
# Darwin can write AppleDouble metadata when opening the file.
if sys.platform!='darwin':assert after.st_ctime_ns==before.st_ctime_ns
fd=os.open(source,os.O_RDWR);os.ftruncate(fd,before.st_size);os.close(fd)
if root!=Path.cwd():assert allocated()<(9 if sys.argv[1]=='remote' else 1)*1024*1024,('open copied payload',allocated())
expected=b'x'*(6*1024*1024)
if mutation!='read':
 fd=os.open(source,os.O_RDWR)
 if mutation.startswith('read-'):assert os.pread(fd,1,0)==b'x'
 if Path('spill').is_dir():
  for index in range(9):
   with open(Path('spill')/str(index),'rb') as other:assert other.read(1)==bytes([index])
  if root!=Path.cwd():
   assert any(raw.stat().st_size==len(expected) for raw in root.glob('raw-objects-*/*'))
 if mutation in ('write','read-write'):os.pwrite(fd,b'y',0);expected=b'y'+expected[1:]
 elif mutation=='read-rewrite':
  expected=b'y'*len(expected);assert os.pwrite(fd,expected,0)==len(expected)
 elif mutation=='read-truncate-extend':
  os.ftruncate(fd,1024);tail=b'y'*(len(expected)-1024)
  assert os.pwrite(fd,tail,1024)==len(tail);expected=expected[:1024]+tail
 elif mutation=='read-replace':
  os.close(fd);fd=None;source.unlink();expected=b'y'*len(expected);source.write_bytes(expected)
 else:os.ftruncate(fd,1024);expected=expected[:1024]
 if fd is not None:os.close(fd)
assert source.read_bytes()==expected
budget=14 if Path('spill').is_dir() else 9
assert allocated()<budget*1024*1024,('content mutation disk allocation',allocated())
print('writable lifecycle verified')
''',source,mutation]

    def test_writable_open_does_not_materialize_source(self):
        template=self.writable_open_fixture()
        for mutation in self.source_mutations:
            with self.subTest(mutation=mutation):
                command=[*template[:-1],mutation]
                clone=self.root/f'clone-{mutation}'
                result=subprocess.run(['git','clone','--no-local','--quiet',str(self.repo),str(clone)],
                                      env=self.environment(),capture_output=True,timeout=20)
                self.assert_success(result)
                direct=subprocess.run(command,cwd=clone,env=self.environment(),capture_output=True,timeout=20)
                self.assert_success(direct)
                result,_=self.invoke_argv(['HEAD','--write-policy=all','--',*command])
                self.assert_success(result);self.assertEqual(result.stdout,direct.stdout)
                self.assert_no_runtime_files()

    @fuse_only
    def test_spilled_copy_up_fits_two_checkouts(self):
        from filesystem_quota import compare_on_tmpfs
        command=self.writable_open_fixture(spill=True)
        compare_on_tmpfs(self,self.root,BINARY,self.repo,os.environ['PATH'],[*command[:-1],'read-write'],
                         size_mib=14,checkout_copies=2)
        self.assert_no_runtime_files()

    def test_remote_mutations_fit_checkout_disk_budget(self):
        template=self.writable_open_fixture(source='remote')
        source, ca = self.serve_repository()
        config = {'GIT_CONFIG_COUNT': '0', 'GIT_SSL_CAINFO': ca}
        env = dict(self.environment(), **config)
        for mutation in ('read-write','read-rewrite','read-truncate-extend','read-replace'):
            with self.subTest(mutation=mutation):
                clone=self.root/f'clone-{mutation}'
                command=[*template[:-1],mutation]
                result=subprocess.run(['git','clone','--no-local','--quiet',str(self.repo),str(clone)],
                                      env=env,capture_output=True,timeout=20)
                self.assert_success(result)
                direct=subprocess.run(command,cwd=clone,env=env,capture_output=True,timeout=20)
                self.assert_success(direct)
                result,_=self.invoke_argv([source,'HEAD','--write-policy=all','--',*command],extra_env=config)
                self.assert_success(result);self.assertEqual(result.stdout,direct.stdout)
                self.assert_no_runtime_files()

    @fuse_only
    def test_remote_rewrite_and_replacement_succeed_on_nine_mib_tmpfs(self):
        from filesystem_quota import compare_on_tmpfs
        template=self.writable_open_fixture(source='remote')
        source, ca = self.serve_repository()
        for mutation in ('read-rewrite', 'read-replace'):
            with self.subTest(mutation=mutation):
                compare_on_tmpfs(self, self.root, BINARY, self.repo, os.environ['PATH'],
                                 [*template[:-1], mutation], source=source, tls_ca=ca)
        self.assert_no_runtime_files()

    def shared_source_fixture(self):
        for directory in ('left','right'):
            (self.repo/directory).mkdir()
            (self.repo/directory/'large').write_bytes(b'x'*(6*1024*1024))
        git(self.repo,'add','left','right');git(self.repo,'commit','-qm','shared source ownership')
        self.assertEqual(git(self.repo,'rev-parse','HEAD:left'),git(self.repo,'rev-parse','HEAD:right'))
        from filesystem_quota import STORAGE_OBSERVATION
        return [sys.executable,'-I','-S','-c',STORAGE_OBSERVATION+r'''
size=6*1024*1024
first=Path('left/large');second=Path('right/large')
assert first.read_bytes()==b'x'*size
# The second source inode has not been looked up yet. Its equal tree is shared.
with first.open('r+b',buffering=0) as output:
 assert output.write(b'y'*size)==size
assert allocated()<16*1024*1024,('first independent source copy',allocated())
assert second.read_bytes()==b'x'*size
os.link(second,'alias')
reader=os.open('alias',os.O_RDONLY)
second.unlink()
with open('alias','r+b',buffering=0) as output:
 assert output.write(b'z'*size)==size
assert os.pread(reader,size,0)==b'z'*size
assert allocated()<16*1024*1024,('last original source duplicated',allocated())
Path('alias').unlink()
assert os.fstat(reader).st_nlink==0
assert os.pread(reader,size,0)==b'z'*size
os.close(reader)
assert first.read_bytes()==b'y'*size
assert allocated()<9*1024*1024,('unlinked payload retained',allocated())
print('independent aliases and unlinked reader verified')
''']

    def test_unopened_aliases_and_unlinked_readers_share_only_needed_sources(self):
        command=self.shared_source_fixture()
        git(self.repo,'config','uploadpack.allowFilter','true')
        git(self.repo,'config','uploadpack.allowAnySHA1InWant','true')
        clone=self.root/'clone'
        result=subprocess.run(['git','clone','--no-local','--quiet',str(self.repo),str(clone)],
                              env=self.environment(),capture_output=True,timeout=20)
        self.assert_success(result)
        direct=subprocess.run(command,cwd=clone,env=self.environment(),capture_output=True,timeout=20)
        self.assert_success(direct)
        for source in (None,self.repo.as_uri()):
            with self.subTest(source=source):
                operands=[source,'HEAD'] if source else ['HEAD']
                result,_=self.invoke_argv([*operands,'--write-policy=all','--',*command])
                self.assert_success(result);self.assertEqual(result.stdout,direct.stdout)
                self.assert_no_runtime_files()

    def unlink_fault(self, variable='GIT_ON_TEST_UNLINK_FAULT'):
        compiler=shutil.which('cc')
        self.assertIsNotNone(compiler,'C compiler is required for filesystem fault injection')
        library=self.root/'transfer-fault.so'
        if not library.is_file():
            subprocess.run([compiler,'-shared','-fPIC','-o',str(library),
                            str(ROOT/'tests/support/transfer_fault.c'),'-ldl'],check=True,capture_output=True)
        return {'LD_PRELOAD':str(library),variable:'1'}

    @fuse_only
    def test_failed_copy_up_can_retry_after_cleanup_error(self):
        (self.repo/'large').write_bytes(b'x'*(128*1024))
        git(self.repo,'add','large');git(self.repo,'commit','-qm','copy-up rollback')
        env=self.unlink_fault('GIT_ON_TEST_COPY_ROLLBACK_FAULT')
        command=[sys.executable,'-I','-S','-c',r'''
import errno,os,resource
fd=os.open('large',os.O_RDWR)
assert os.pread(fd,2,0)==b'xx'
runner=os.getppid()
limits=resource.prlimit(runner,resource.RLIMIT_FSIZE)
try:
 resource.prlimit(runner,resource.RLIMIT_FSIZE,(3,limits[1]))
 try:os.pwrite(fd,b'y',0)
 except OSError as error:assert error.errno==errno.EFBIG,error
 else:raise AssertionError('copy-up unexpectedly succeeded')
finally:
 resource.prlimit(runner,resource.RLIMIT_FSIZE,limits)
assert os.pread(fd,2,0)==b'xx'
assert os.pwrite(fd,b'z',0)==1
assert os.pread(fd,2,0)==b'zx'
os.close(fd)
''']
        result,_=self.invoke_argv(['HEAD','--write-policy=all','--',*command],extra_env=env,
                                  preexec_fn=lambda: signal.signal(signal.SIGXFSZ, signal.SIG_IGN))
        self.assert_success(result)
        self.assert_no_runtime_files()

    def test_copy_up_and_reads_of_the_immutable_source(self):
        data=b'x'*(3*65536+17)
        for name in ('large','original'):
            (self.repo/name).write_bytes(data)
        self.add_spill_blobs(self.repo)
        git(self.repo,'add','.');git(self.repo,'commit','-qm','copy-up integrity')
        oid=git(self.repo,'rev-parse','HEAD:large')
        url, ca = self.serve_repository()
        command=[sys.executable,'-I','-S','-c',r'''
import errno,os,sys
from pathlib import Path
fd=os.open('large',os.O_RDWR)
assert os.pread(fd,1,0)==b'x'
root=Path.cwd().parent
raw=next(root.glob('raw-objects-*'))/sys.argv[1]
assert not raw.exists()
for index in range(9):
 with open(Path('spill')/str(index),'rb') as other:assert other.read(1)==bytes([index])
with raw.open('r+b',buffering=0) as original:
 assert os.pwrite(original.fileno(),b'!',2*65536+7)==1
assert os.pwrite(fd,b'y',0)==1
os.fsync(fd)
assert os.pread(fd,2,0)==b'yx'
assert os.pread(fd,1,2*65536+7)==b'!'
with raw.open('rb') as original:
 assert original.read(2)==b'xx'
os.close(fd)
fd=os.open('original',os.O_RDONLY)
try:
 os.pread(fd,1,2*65536+7)
except OSError as error:assert error.errno==errno.EIO,error
else:raise AssertionError('immutable read accepted a changed block')
os.close(fd)
print('private write and immutable read verified')
''',oid]
        for source in ([], [url]):
            with self.subTest(source=source):
                result,_=self.invoke_argv([*source, 'HEAD', '--write-policy=all', '--', *command],
                                          extra_env={'GIT_CONFIG_COUNT': '0', 'GIT_SSL_CAINFO': ca})
                self.assertEqual(result.returncode,125 if source else 0,result.stderr)
                self.assertEqual(result.stdout,b'private write and immutable read verified\n')
                self.assert_no_runtime_files()

    @fuse_only
    def test_unlinked_source_survives_backing_unlink_failure_without_redecode(self):
        # Fail before installing the private backing. The verified cache still
        # serves the open inode and allows a retry after removing the Git object.
        env=self.unlink_fault()
        (self.repo/'large').write_bytes(b'x'*(6*1024*1024))
        git(self.repo,'add','large');git(self.repo,'commit','-qm','source transfer failure')
        oid=git(self.repo,'rev-parse','HEAD:large')
        loose=self.repo/'.git/objects'/oid[:2]/oid[2:]
        from filesystem_quota import STORAGE_OBSERVATION
        command=[sys.executable,'-I','-S','-c',STORAGE_OBSERVATION+r'''
import errno,sys
from pathlib import Path
fd=os.open('large',os.O_RDWR)
assert os.pread(fd,1,0)==b'x'
Path('large').unlink()
try:os.pwrite(fd,b'y',0)
except OSError as error:assert error.errno==errno.EACCES,error
else:raise AssertionError('backing unlink fault was not exercised')
Path(sys.argv[1]).unlink()
assert os.pread(fd,2,0)==b'xx','lost prepared source after failed unlink'
assert os.pwrite(fd,b'z',0)==1
assert os.pread(fd,2,0)==b'zx'
os.close(fd)
Path('replacement').write_bytes(b'z'*(6*1024*1024))
assert allocated()<9*1024*1024,('failed unlink retained released payload',allocated())
print('source retained after backing unlink failure')
''',str(loose)]
        result,_=self.invoke_argv(['HEAD','--write-policy=all','--',*command],extra_env=env)
        self.assert_success(result);self.assert_no_runtime_files()

    @fuse_only
    def test_shared_sources_succeed_on_sixteen_mib_tmpfs(self):
        from filesystem_quota import compare_on_tmpfs
        compare_on_tmpfs(self,self.root,BINARY,self.repo,os.environ['PATH'],self.shared_source_fixture(),size_mib=16)
        self.assert_no_runtime_files()

    def source_metadata_fixture(self):
        (self.repo/'large').write_bytes(b'x'*(6*1024*1024))
        git(self.repo,'add','large');git(self.repo,'commit','-qm','metadata disk budget')
        from filesystem_quota import STORAGE_OBSERVATION
        code=STORAGE_OBSERVATION+r'''
source=Path('large')
reader=os.open(source,os.O_RDONLY)
os.chmod(source,0o640)
os.utime(source,ns=(1000000000,2000000000))
os.link(source,'alias');source.rename('moved')
assert Path('moved').stat().st_mode & 0o777==0o640
assert Path('alias').stat().st_mtime_ns==2000000000
budget=(9 if root==Path.cwd() else 1)*1024*1024
assert allocated()<budget,('metadata copied payload',allocated())
assert Path('alias').read_bytes()==b'x'*(6*1024*1024)
Path('alias').unlink();Path('moved').unlink()
assert os.fstat(reader).st_nlink==0
os.fchmod(reader,0o600)
assert os.pread(reader,1,6*1024*1024-1)==b'x'
os.close(reader)
assert allocated()<9*1024*1024,('metadata disk allocation',allocated())
print('metadata and unlinked reader verified')
'''
        return [sys.executable,'-I','-S','-c',code]

    def test_source_metadata_changes_fit_checkout_disk_budget(self):
        command=self.source_metadata_fixture()
        clone=self.root/'clone'
        result=subprocess.run(['git','clone','--no-local','--quiet',str(self.repo),str(clone)],
                              env=self.environment(),capture_output=True,timeout=20)
        self.assert_success(result)
        # A checkout already owns its one payload copy before chmod.
        direct=subprocess.run(command,
                              cwd=clone,env=self.environment(),capture_output=True,timeout=20)
        self.assert_success(direct)
        result,_=self.invoke_argv(['HEAD','--write-policy=all','--',*command])
        self.assert_success(result);self.assertEqual(result.stdout,direct.stdout)
        self.assert_no_runtime_files()

    @fuse_only
    def test_source_metadata_changes_succeed_on_nine_mib_tmpfs(self):
        from filesystem_quota import compare_on_tmpfs
        compare_on_tmpfs(self,self.root,BINARY,self.repo,os.environ['PATH'],self.source_metadata_fixture())
        self.assert_no_runtime_files()

    def test_readonly_directory_pages_preserve_partial_seek_reopen_and_empty(self):
        from source_cache import directory_child_code,directory_names
        count=640
        payload=git(self.repo,'hash-object','-w','--stdin',input=b'unrequested directory payload')
        empty=git(self.repo,'mktree',input=b'')
        paged=git(self.repo,'mktree',input=b''.join(f'100644 blob {payload}\t{name}\n'.encode() for name in directory_names(count)))
        tree=git(self.repo,'mktree',input=f'040000 tree {empty}\tempty\n040000 tree {paged}\tpaged\n'.encode())
        # A listing must not read leaf headers/content, even on a cold mount.
        (self.repo/'.git/objects'/payload[:2]/payload[2:]).unlink()
        result,run=self.invoke_argv([tree,'--write-policy=none','--',sys.executable,'-I','-S','-c',directory_child_code(count)])
        self.assert_success(result);self.assertIsNone(run);self.assert_no_runtime_files()
        self.assertEqual([json.loads(row) for row in result.stdout.splitlines()],
                         [dict(completed=phase,directory_entries=count) for phase in ('first','repeat')])

    def test_readonly_stateless_large_reads_dup_mmap_and_sync(self):
        size=17*1024*1024
        (self.repo/'large-readonly').write_bytes(bytes(range(256))*(size//256))
        git(self.repo,'add','large-readonly');git(self.repo,'commit','-qm','readonly file lifecycle')
        code=r'''
import errno,mmap,os
size=17*1024*1024
pattern=bytes(range(256))*16
fds=[os.open('large-readonly',os.O_RDONLY) for _ in range(3)]
fds.append(os.dup(fds[0]))
try:
 for fd in fds:
  assert os.fstat(fd).st_size==size
  for offset in (size-4096,0,65536,0):assert os.pread(fd,4096,offset)==pattern
  os.fsync(fd);getattr(os, "fdatasync", os.fsync)(fd)
 with mmap.mmap(fds[0],0,access=mmap.ACCESS_READ) as view:
  assert view[:4096]==pattern and view[-4096:]==pattern
 os.close(fds.pop())
 assert os.pread(fds[0],4096,0)==pattern
 for flags in (os.O_WRONLY,os.O_RDWR,os.O_RDONLY|os.O_TRUNC):
  try:os.open('large-readonly',flags)
  except OSError as error:assert error.errno in (errno.EROFS,errno.EACCES),error
  else:raise AssertionError('readonly source opened for mutation')
finally:
 for fd in fds:os.close(fd)
with open('large-readonly','rb') as reopened:assert reopened.read(4096)==pattern
'''
        result,run=self.invoke_argv(['HEAD','--write-policy=none','--',sys.executable,'-I','-S','-c',code])
        self.assert_success(result);self.assertIsNone(run);self.assert_no_runtime_files()

    def test_all_source_cache_is_coherent_across_copy_up_mmap_and_namespace_changes(self):
        data=b'a'*8192
        (self.repo/'cached-source').write_bytes(data)
        git(self.repo,'add','cached-source');git(self.repo,'commit','-qm','cache coherence fixture')
        code=r'''
import mmap,os
from pathlib import Path
p=Path('cached-source')
reader=os.open(p,os.O_RDONLY)
duplicate=os.dup(reader);os.close(duplicate)
os.fsync(reader);getattr(os, "fdatasync", os.fsync)(reader)
mapping=mmap.mmap(reader,8192,access=mmap.ACCESS_READ)
assert mapping[:]==b'a'*8192
assert p.read_bytes()==b'a'*8192  # warm pages, close and reopen
writer=os.open(p,os.O_RDWR)
try:
 os.pwrite(writer,b'b'*8192,0)
 duplicate=os.dup(reader);os.close(duplicate)
 os.fsync(reader);getattr(os, "fdatasync", os.fsync)(reader)
 assert os.pread(reader,8192,0)==b'b'*8192 and mapping[:]==b'b'*8192
 assert p.read_bytes()==b'b'*8192
 os.pwrite(writer,b'tail',8192)
 assert os.fstat(reader).st_size==8196 and p.read_bytes()==b'b'*8192+b'tail'
 os.ftruncate(writer,4096)
 assert os.fstat(reader).st_size==4096 and os.pread(reader,8192,0)==b'b'*4096
 os.ftruncate(writer,8192)
 assert mapping[:]==b'b'*4096+b'\0'*4096
 with mmap.mmap(writer,8192,access=mmap.ACCESS_WRITE) as writable:
  writable[32:36]=b'mmap';writable.flush()
 assert mapping[32:36]==b'mmap' and os.pread(reader,4,32)==b'mmap'
 os.chmod(p,0o600)
 os.link(p,'cached-alias');p.rename('cached-moved')
 p=Path('cached-moved')
 assert Path('cached-alias').read_bytes()==p.read_bytes()
 Path('replacement.tmp').write_bytes(b'replacement')
 os.replace('replacement.tmp',p)
 assert p.read_bytes()==b'replacement' and mapping[32:36]==b'mmap'
 Path('cached-alias').unlink()
 os.pwrite(writer,b'last',0)
 assert os.pread(reader,4,0)==b'last' and mapping[:4]==b'last'
 assert os.fstat(reader).st_nlink==0
finally:
 mapping.close();os.close(writer);os.close(reader)
# O_TRUNC must invalidate untouched source pages kept by an earlier readonly open.
p=Path('tracked.txt')
assert p.read_bytes()==b'committed\n'
with p.open('wb') as f:f.write(b'new')
assert p.read_bytes()==b'new'
'''
        result,_=self.invoke_argv(['HEAD','--write-policy=all','--',sys.executable,'-c',code])
        self.assert_success(result)
        self.assertEqual((self.repo/'cached-source').read_bytes(),data)
        self.assert_no_runtime_files()

    def test_local_unverified_run_requires_native_git_for_revision_resolution(self):
        programs = self.root / "fuse-only"
        programs.mkdir()
        for name in ("fusermount3", "fusermount"):
            helper = shutil.which(name)
            if helper:
                (programs / name).symlink_to(helper)
        self.assertIsNone(shutil.which("git", path=str(programs)))
        for revision, path, expected in (("HEAD", "tracked.txt", b"committed\n"),
                                         ("HEAD:subtree", "source.txt", b"subtree source\n")):
            with self.subTest(revision=revision):
                result, run = self.python(
                    f'import sys; sys.stdout.buffer.write(open({path!r},"rb").read())',
                    tree=revision, extra_env={"PATH": str(programs)})
                self.assertEqual(result.returncode, 125, result.stderr)
                self.assertIn(b"native Git", result.stderr)
                self.assertIn(b"PATH", result.stderr)
                self.assertEqual(result.stdout, b"")
                self.assertIsNone(run)
                self.assert_no_runtime_files()

    def test_native_git_abbreviation_reflog_and_non_utf8_subtree_are_mounted(self):
        self.put("history.txt", "second commit\n")
        git(self.repo, "add", "history.txt")
        git(self.repo, "commit", "--quiet", "-m", "native revision syntax")
        parent = git(self.repo, "rev-parse", "HEAD^")
        for revision in (parent[:12], "HEAD^", "HEAD~1", "HEAD@{1}"):
            with self.subTest(revision=revision):
                expected_tree = git(self.repo, "rev-parse", revision + "^{tree}")
                result, run = self.python(
                    "from pathlib import Path; import os; "
                    "assert Path('tracked.txt').read_text() == 'committed\\n'; "
                    "assert not Path('history.txt').exists(); "
                    f"assert os.environ['GIT_ON_TREE_OID'] == {expected_tree!r}",
                    tree=revision,
                )
                self.assert_success(result)
                self.assertIsNone(run)
                self.assert_no_runtime_files()
        raw_name = b"tree-\xff"
        blob = git(self.repo, "hash-object", "-w", "--stdin", input=b"non-UTF8 directory\n")
        git(self.repo, "update-index", "-z", "--index-info",
            input=b"100644 " + blob.encode() + b"\t" + raw_name + b"/value\0")
        git(self.repo, "commit", "--quiet", "-m", "non-UTF8 tree path")
        result, run = self.invoke_argv([
            b"HEAD:" + raw_name, "--", sys.executable, "-c",
            "from pathlib import Path; assert Path('value').read_text() == 'non-UTF8 directory\\n'",
        ])
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_two_operands_open_local_repository_without_creating_refs(self):
        branch = git(self.repo, "symbolic-ref", "--short", "HEAD")
        before = git(self.repo, "for-each-ref", "--format=%(refname) %(objectname)")
        alias_name = b"repository space" if sys.platform == "darwin" else b"repository-\xff"
        raw_alias = self.root / os.fsdecode(alias_name)
        raw_alias.symlink_to(self.repo, target_is_directory=True)
        for source in (str(self.repo), ".", str(raw_alias)):
            revision = branch
            with self.subTest(source=source, revision=revision):
                result, run = self.invoke_argv([
                    source, revision, "--", sys.executable, "-c",
                    "from pathlib import Path; import os; "
                    "assert Path('tracked.txt').read_text() == 'committed\\n'; "
                    "assert not Path('.git').exists(); "
                    f"assert os.environ['GIT_ON_TREE_OID'] == {self.tree!r}",
                ])
                self.assert_success(result)
                self.assertIsNone(run)
                self.assertEqual(list(self.system_tmp.iterdir()), [])
                self.assertEqual(git(self.repo, "for-each-ref", "--format=%(refname) %(objectname)"), before)

    def test_snapshot_ignores_dirty_index_worktree_and_untracked_files(self):
        self.put("tracked.txt", "staged\n")
        git(self.repo, "add", "tracked.txt")
        self.put("tracked.txt", "dirty\n")
        self.put("untracked.txt", "not in snapshot")
        result, run = self.python("from pathlib import Path; assert Path('tracked.txt').read_text() == 'committed\\n'; assert not Path('untracked.txt').exists(); assert not Path('.git').exists(); print('snapshot')")
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_no_runtime_files()
        self.assertEqual((self.repo / "tracked.txt").read_text(), "dirty\n")

    def test_executable_mode_symlinks_and_repository_discovery(self):
        self.put("executable.sh", "#!/bin/sh\nprintf 'executable\\n'\n")
        (self.repo / "executable.sh").chmod(0o755)
        git(self.repo, "add", "executable.sh")
        git(self.repo, "commit", "--quiet", "-m", "executable fixture")
        code = "from pathlib import Path; import os,subprocess; assert os.access('executable.sh',os.X_OK); assert Path('tracked-link').is_symlink(); assert Path('tracked-link').read_text() == 'committed\\n'; subprocess.run(['./executable.sh'],check=True)"
        result = subprocess.run([str(BINARY), *MOUNT_ARGS, "HEAD", "--", sys.executable, "-c", code], cwd=self.repo / "nested", env=self.environment(), capture_output=True, timeout=30)
        self.assert_success(result)
        self.assertEqual(result.stdout, b"executable\n")

    def test_raw_tree_and_subtree_root(self):
        for tree in (self.tree, "HEAD:subtree"):
            source = "tracked.txt" if tree == self.tree else "source.txt"
            expected_oid = git(self.repo, "rev-parse", tree)
            result, run = self.python(f"from pathlib import Path; import os; assert Path({source!r}).is_file(); assert os.environ['GIT_ON_TREE_OID'] == {expected_oid!r}; print('ok')", tree=tree)
            self.assert_success(result)
            self.assertIsNone(run)
            self.assert_no_runtime_files()
        result, _ = self.python("from pathlib import Path; Path('generated').mkdir(); Path('generated/result').write_text('ok')", tree="HEAD:subtree")
        self.assert_success(result)

    def test_no_shell_inherited_stdin_and_exact_binary_stdio(self):
        arguments = ["$(touch nope)", "a b", "; exit 5", "--keep", "-k", "--cleanup",
                     "--help", "--version", "--unknown", "--", ""]
        code = f"import os,sys; assert sys.argv[1:] == {arguments!r}; assert sys.stdin.buffer.read() == b'input\\x00bytes'; os.write(1,b'out\\x00\\xff\\n'); os.write(2,b'err\\x00\\xfe\\n')"
        prefix = ["HEAD", "--"]
        result, run = self.invoke_argv([*prefix, sys.executable, "-c", code, *arguments],
                                       input=b"input\x00bytes")
        self.assert_success(result)
        self.assertEqual(result.stdout, b"out\x00\xff\n")
        self.assertEqual(result.stderr, b"err\x00\xfe\n")
        self.assertIsNone(run)
        self.assertFalse((self.repo / "nope").exists())
        self.assert_no_runtime_files()

    def test_explicit_boundary_allows_dash_named_executable(self):
        executable = self.root / "--keep"
        executable.symlink_to(sys.executable)
        env = {"PATH": str(self.root) + os.pathsep + os.environ.get("PATH", "")}
        prefix = ["HEAD", "--"]
        result, run = self.invoke_argv([*prefix, "--keep", "-c", "print('literal-command')"],
                                       extra_env=env)
        self.assert_success(result)
        self.assertEqual(result.stdout, b"literal-command\n")
        self.assertEqual(result.stderr, b"")
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_stdio_inherits_regular_file_descriptions_and_offsets(self):
        input_path, output_path = self.root / "stdin", self.root / "combined"
        input_path.write_bytes(b"skip-input\x00bytes")
        output_path.write_bytes(b"prefix:")
        with input_path.open("rb") as source, output_path.open("r+b") as output:
            source.seek(5)
            output.seek(7)
            expected = [(os.fstat(stream.fileno()).st_dev, os.fstat(stream.fileno()).st_ino)
                        for stream in (source, output, output)]
            code = r"""
import json, os, stat, sys
for fd, expected in enumerate(json.loads(sys.argv[1])):
    actual = os.fstat(fd)
    assert stat.S_ISREG(actual.st_mode), (fd, actual.st_mode)
    assert [actual.st_dev, actual.st_ino] == expected, fd
assert os.read(0, 100) == b'input\x00bytes'
assert os.lseek(1, 0, os.SEEK_CUR) == 7
os.write(1, b'one')
assert os.lseek(2, 0, os.SEEK_CUR) == 10
os.write(2, b'two')
os.lseek(1, 0, os.SEEK_SET)
os.write(1, b'P')
"""
            result = subprocess.run([str(BINARY), "HEAD", "--", sys.executable, "-c", code,
                                     json.dumps(expected)], cwd=self.repo,
                                    env=self.environment(), stdin=source,
                                    stdout=output, stderr=output, timeout=30)
            self.assertEqual(result.returncode, 0, output_path.read_bytes())
            self.assertEqual(source.tell(), len(b"skip-input\x00bytes"))
            self.assertEqual(output.tell(), 1)
        self.assertEqual(output_path.read_bytes(), b"Prefix:onetwo")
        self.assert_no_runtime_files()

    def test_broken_stdout_pipe_preserves_child_sigpipe_status(self):
        read_fd, write_fd = os.pipe()
        os.close(read_fd)
        try:
            result = subprocess.run([str(BINARY), "HEAD", "--", sys.executable, "-c",
                                     "import os,signal; signal.signal(signal.SIGPIPE,signal.SIG_DFL); os.write(1,b'broken')"],
                                    cwd=self.repo, env=self.environment(), stdout=write_fd,
                                    stderr=subprocess.PIPE, timeout=30)
        finally:
            os.close(write_fd)
        self.assertEqual(result.returncode, -signal.SIGPIPE)
        self.assertEqual(result.stderr, b"")
        self.assert_no_runtime_files()

    def test_large_stdout_and_stderr_do_not_deadlock(self):
        result, run = self.python("import os; [(os.write(1,b'x'*65536),os.write(2,b'y'*65536)) for _ in range(40)]")
        self.assert_success(result)
        self.assertEqual(result.stdout, b"x" * (40 * 65536))
        self.assertEqual(result.stderr, b"y" * (40 * 65536))
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_immutable_operations_and_ignored_artifacts(self):
        result, run = self.python('''
import os
from pathlib import Path

def denied(action):
    try: action()
    except OSError: return
    raise AssertionError('immutable operation unexpectedly succeeded')

for target in ['tracked.txt', 'out/protected.txt', '.gitignore', 'tracked-link']:
    denied(lambda: Path(target).write_text('overwrite'))
    denied(lambda: os.unlink(target))
    denied(lambda: os.rename(target, 'out/moved'))
denied(lambda: os.chmod('tracked.txt', 0o600))
denied(lambda: os.utime('tracked.txt', (0, 0)))
denied(lambda: os.truncate('tracked.txt', 0))
denied(lambda: os.close(os.open('tracked.txt', os.O_RDONLY | os.O_TRUNC)))
denied(lambda: os.rmdir('nested'))
denied(lambda: Path('not-ignored.txt').write_text('no'))
denied(lambda: Path('keep.cache').write_text('no'))
denied(lambda: Path('nested/keep.tmp').write_text('no'))
denied(lambda: Path('nested/not-ignored.txt').write_text('no'))
Path('nested/allowed.tmp').write_text('nested')
Path('okay.cache').write_text('cache')
Path('only-root').mkdir()
Path('only-root/value').write_text('root')
denied(lambda: Path('nested/only-root').mkdir())
Path('out/new').mkdir()
Path('out/new/value').write_text('first')
with open('out/new/value', 'a') as f: f.write(' second')
os.chmod('out/new/value', 0o600)
os.rename('out/new/value', 'out/new/result')
denied(lambda: os.rename('out/new', 'plain/new'))
Path('out/replacement').write_text('replace')
denied(lambda: os.replace('out/replacement', 'tracked.txt'))
denied(lambda: os.link('out/replacement', 'not-ignored-hardlink'))
Path('out/trash').write_text('delete')
os.unlink('out/trash')
assert Path('tracked.txt').read_text() == 'committed\\n'
assert Path('nested/allowed.tmp').read_text() == 'nested'
assert not Path('out/trash').exists()
assert Path('out/new/result').read_text() == 'first second'
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_no_runtime_files()
        self.assertFalse((self.repo / "out/new").exists())

    def test_file_operation_matrix_preserves_tracked_bytes(self):
        self.put("protected.cache", "tracked cache\n")
        self.put("nested/protected.tmp", "tracked nested\n")
        git(self.repo, "add", "--force", "protected.cache", "nested/protected.tmp")
        git(self.repo, "commit", "--quiet", "-m", "tracked ignore precedence")
        result, _ = self.python(r'''
import errno, os, stat, sys
from pathlib import Path

def denied(label, action, codes=(errno.EROFS, errno.EACCES, errno.EPERM)):
    try: action()
    except OSError as error:
        assert error.errno in codes, (label, error)
    else: raise AssertionError(label + ' unexpectedly succeeded')

def opened(path, flags):
    fd = os.open(path, flags, 0o600)
    os.close(fd)

for path in ['tracked.txt', 'out/protected.txt', 'protected.cache',
             'nested/protected.tmp', '.gitignore']:
    original = Path(path).read_bytes()
    before = os.stat(path)
    for label, operation in [
        ('write', lambda: Path(path).write_bytes(b'bad')),
        ('append', lambda: opened(path, os.O_WRONLY | os.O_APPEND)),
        ('read-write', lambda: opened(path, os.O_RDWR)),
        ('truncate-open', lambda: opened(path, os.O_WRONLY | os.O_TRUNC)),
        ('read-only-truncate', lambda: opened(path, os.O_RDONLY | os.O_TRUNC)),
        ('truncate', lambda: os.truncate(path, 1)),
        ('chmod', lambda: os.chmod(path, 0o600)),
        # Darwin can satisfy chown-to-self without contacting the filesystem.
        ('chown', lambda: os.chown(path, os.getuid() + (sys.platform == 'darwin'), os.getgid())),
        ('utime', lambda: os.utime(path, ns=(123000000000, 456000000000))),
        ('unlink', lambda: os.unlink(path)),
        ('rename', lambda: os.rename(path, 'out/stolen')),
        ('hardlink', lambda: os.link(path, 'out/stolen-hardlink')),
    ]:
        denied(path + ':' + label, operation)
        assert Path(path).read_bytes() == original, (path, label)
    after = os.stat(path)
    assert (after.st_mode, after.st_size, after.st_mtime_ns) == (before.st_mode, before.st_size, before.st_mtime_ns)

for path in ['out/matrix', 'matrix.cache', 'nested/matrix.tmp']:
    opened(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
    assert Path(path).read_bytes() == b''
    denied(path + ':exclusive-create', lambda: opened(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL), (errno.EEXIST,))
    fd = os.open(path, os.O_RDWR)
    os.write(fd, b'abcdef')
    assert os.pread(fd, 6, 0) == b'abcdef'
    os.pwrite(fd, b'XY', 2)
    assert os.pread(fd, 6, 0) == b'abXYef'
    os.ftruncate(fd, 3)
    assert os.fstat(fd).st_size == 3
    os.ftruncate(fd, 6)
    assert os.pread(fd, 6, 0) == b'abX\0\0\0'
    os.fchmod(fd, 0o640)
    os.fsync(fd)
    os.close(fd)
    fd = os.open(path, os.O_WRONLY | os.O_APPEND)
    os.lseek(fd, 0, os.SEEK_SET)
    os.write(fd, b'!')
    os.close(fd)
    assert Path(path).read_bytes() == b'abX\0\0\0!'
    os.truncate(path, 2)
    assert Path(path).read_bytes() == b'ab'
    os.utime(path, ns=(123000000000, 456000000000))
    os.chown(path, os.getuid(), os.getgid())
    assert stat.S_IMODE(os.stat(path).st_mode) == 0o640
    assert os.stat(path).st_mtime_ns == 456000000000
    opened(path, os.O_WRONLY | os.O_TRUNC)
    assert Path(path).read_bytes() == b''
    os.unlink(path)
    assert not Path(path).exists()

for path in ['not-ignored.txt', 'keep.cache', 'nested/keep.tmp', 'nested/plain.txt']:
    for flags in [os.O_WRONLY | os.O_CREAT, os.O_RDWR | os.O_CREAT,
                  os.O_WRONLY | os.O_CREAT | os.O_EXCL,
                  os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
                  os.O_WRONLY | os.O_CREAT | os.O_APPEND]:
        denied(path + ':create', lambda: opened(path, flags))
        assert not os.path.lexists(path)
''')
        self.assert_success(result)
        self.assertEqual((self.repo / "protected.cache").read_text(), "tracked cache\n")
        self.assertFalse((self.repo / "out/matrix").exists())

    def test_directory_and_rename_operation_matrix_is_atomic(self):
        result, _ = self.python(r'''
import errno, os, stat
from pathlib import Path

def denied(label, action, codes=(errno.EROFS, errno.EACCES, errno.EPERM)):
    try: action()
    except OSError as error: assert error.errno in codes, (label, error)
    else: raise AssertionError(label + ' unexpectedly succeeded')

for path in ['nested', 'plain', 'out']:
    denied(path + ':chmod', lambda: os.chmod(path, 0o700))
    denied(path + ':utime', lambda: os.utime(path, (0, 0)))
    denied(path + ':rename', lambda: os.rename(path, 'moved.cache'))
    denied(path + ':rmdir', lambda: os.rmdir(path), (errno.EROFS, errno.EACCES, errno.EPERM, errno.ENOTEMPTY))
for path in ['forbidden-dir', 'plain/forbidden', 'keep.cache']:
    denied(path + ':mkdir', lambda: os.mkdir(path))
    assert not Path(path).exists()

Path('out/tree/sub').mkdir(parents=True)
Path('out/tree/sub/value').write_text('payload')
os.chmod('out/tree', 0o700)
os.utime('out/tree', ns=(1000000000, 2000000000))
assert stat.S_IMODE(os.stat('out/tree').st_mode) == 0o700
assert os.stat('out/tree').st_mtime_ns == 2000000000
denied('nonempty-rmdir', lambda: os.rmdir('out/tree'), (errno.ENOTEMPTY, errno.EEXIST))
denied('directory-cycle', lambda: os.rename('out/tree', 'out/tree/sub/cycle'), (errno.EINVAL,))
denied('directory-policy-boundary', lambda: os.rename('out/tree', 'plain/moved'))
assert Path('out/tree/sub/value').read_text() == 'payload'
assert not Path('plain/moved').exists()
os.rename('out/tree', 'out/renamed')
assert not Path('out/tree').exists()
assert Path('out/renamed/sub/value').read_text() == 'payload'
Path('out/empty-destination').mkdir()
os.replace('out/renamed', 'out/empty-destination')
assert Path('out/empty-destination/sub/value').read_text() == 'payload'
Path('out/nonempty').mkdir()
Path('out/nonempty/keep').write_text('keep')
denied('replace-nonempty-directory', lambda: os.replace('out/empty-destination', 'out/nonempty'), (errno.ENOTEMPTY, errno.EEXIST))
assert Path('out/nonempty/keep').read_text() == 'keep'
assert Path('out/empty-destination/sub/value').read_text() == 'payload'

Path('out/source').write_text('source')
Path('out/destination').write_text('destination')
old_fd = os.open('out/destination', os.O_RDONLY)
os.replace('out/source', 'out/destination')
assert os.read(old_fd, 32) == b'destination'
os.close(old_fd)
assert Path('out/destination').read_text() == 'source'
assert not Path('out/source').exists()
for target in ['tracked.txt', 'out/protected.txt', '.gitignore', 'tracked-link']:
    original = Path(target).read_bytes()
    denied('replace-tracked:' + target, lambda: os.replace('out/destination', target))
    assert Path(target).read_bytes() == original
    assert Path('out/destination').read_text() == 'source'
for target in ['forbidden', 'keep.cache', 'nested/keep.tmp']:
    denied('rename-disallowed:' + target, lambda: os.rename('out/destination', target))
    assert not os.path.lexists(target)
    assert Path('out/destination').read_text() == 'source'
os.rename('out/destination', 'renamed.cache')
assert Path('renamed.cache').read_text() == 'source'
os.unlink('renamed.cache')
os.unlink('out/empty-destination/sub/value')
os.rmdir('out/empty-destination/sub')
os.rmdir('out/empty-destination')
assert not Path('out/empty-destination').exists()
''')
        self.assert_success(result)

    def test_symlink_and_hardlink_lifecycle_matrix(self):
        result, _ = self.python(r'''
import errno, os
from pathlib import Path

def denied(label, action, codes=(errno.EROFS, errno.EACCES, errno.EPERM, errno.EEXIST)):
    try: action()
    except OSError as error: assert error.errno in codes, (label, error)
    else: raise AssertionError(label + ' unexpectedly succeeded')

Path('out/owned').write_text('original')
for path in ['forbidden-link', 'keep.cache', 'nested/keep.tmp', 'tracked.txt', 'out/protected.txt']:
    denied('symlink:' + path, lambda: os.symlink('out/owned', path))
    denied('hardlink:' + path, lambda: os.link('out/owned', path))
for source in ['tracked.txt', 'out/protected.txt', '.gitignore', 'tracked-link']:
    denied('hardlink-tracked:' + source, lambda: os.link(source, 'out/hard-protected'))

os.symlink('owned', 'out/soft')
assert os.readlink('out/soft') == 'owned'
assert Path('out/soft').read_text() == 'original'
Path('out/soft').write_text('changed')
assert Path('out/owned').read_text() == 'changed'
os.rename('out/soft', 'out/renamed-soft')
assert os.readlink('out/renamed-soft') == 'owned'
os.unlink('out/renamed-soft')
assert Path('out/owned').read_text() == 'changed'
os.symlink('../tracked.txt', 'out/protected-soft')
assert Path('out/protected-soft').read_text() == 'committed\n'
denied('write-through-protected-link', lambda: Path('out/protected-soft').write_text('bad'))
denied('truncate-through-protected-link', lambda: os.truncate('out/protected-soft', 0))
os.unlink('out/protected-soft')
assert Path('tracked.txt').read_text() == 'committed\n'
os.symlink('../forbidden-target', 'out/dangling')
denied('create-through-policy-link', lambda: Path('out/dangling').write_text('bad'))
assert not Path('forbidden-target').exists()
os.unlink('out/dangling')

os.link('out/owned', 'out/hard')
assert os.stat('out/owned').st_ino == os.stat('out/hard').st_ino
assert os.stat('out/owned').st_nlink == 2
Path('out/hard').write_text('linked')
assert Path('out/owned').read_text() == 'linked'
fd = os.open('out/owned', os.O_RDWR)
os.unlink('out/owned')
assert os.stat('out/hard').st_nlink == 1
os.rename('out/hard', 'out/final-hard')
os.unlink('out/final-hard')
assert os.fstat(fd).st_nlink == 0
assert os.pread(fd, 6, 0) == b'linked'
os.pwrite(fd, b'open', 0)
os.ftruncate(fd, 4)
assert os.pread(fd, 10, 0) == b'open'
os.close(fd)
assert not Path('out/final-hard').exists()
''')
        self.assert_success(result)

    @unittest.skipUnless(sys.platform == 'linux', 'Linux helper process inspection')
    def test_cancel_hung_unmount_stops_helper_and_releases_run_for_cleanup(self):
        import shlex
        programs=self.root/'unmount-programs'; programs.mkdir()
        marker=self.root/'unmount-helper'; release=self.root/'release-helper'
        wrapper=programs/'fusermount3'
        wrapper.write_text('#!/bin/sh\nif [ "$1" = "-u" ] && [ ! -e '+shlex.quote(str(release))+' ]; then\n'
                           'printf "%s" "$$" > '+shlex.quote(str(marker))+'\nexec /bin/sleep 300\nfi\nexec '+
                           shlex.quote(shutil.which('fusermount3'))+' "$@"\n')
        wrapper.chmod(0o700)
        env=dict(self.environment(),PATH=str(programs)+os.pathsep+os.environ['PATH'])
        runner=subprocess.Popen([str(BINARY),'HEAD','--write-policy=none','--','/bin/true'],
                                cwd=self.repo,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        helper=None
        try:
            deadline=time.monotonic()+10
            while not marker.exists() or not marker.read_text().strip():
                if runner.poll() is not None:
                    self.fail(runner.communicate()[1].decode(errors='replace'))
                self.assertLess(time.monotonic(),deadline,'unmount helper did not start')
                time.sleep(.02)
            helper=int(marker.read_text())
            runner.send_signal(signal.SIGTERM)
            _,stderr=runner.communicate(timeout=3)
            self.assertEqual(runner.returncode,-signal.SIGTERM,stderr)
            self.assertFalse(Path(f'/proc/{helper}').exists(),'owned helper was not reaped')
            run=next((self.system_tmp / "git-on").glob("??????"))
            self.assertIn(os.fsencode(run),stderr,'cleanup diagnostic must identify retained state')
            self.assert_success(self.cleanup_run(run))
            self.assert_no_runtime_files()
        finally:
            release.touch()
            if helper is not None and Path(f'/proc/{helper}/status').exists():
                if f'PPid:\t{runner.pid}\n' in Path(f'/proc/{helper}/status').read_text():
                    os.kill(helper,signal.SIGKILL)
            try: runner.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                runner.kill(); runner.communicate(timeout=5)
            for run in (self.system_tmp / "git-on").glob("??????"):
                self.assert_success(self.cleanup_run(run))

    def test_cleanup_after_reading_a_spilled_blob(self):
        self.require_linux_nfs_deferred_unlink()
        directory = self.repo / "cache-spill"
        directory.mkdir()
        large = directory / "large"
        large.write_bytes(b"x" * (2 * 1024 * 1024))
        self.add_spill_blobs(directory)
        git(self.repo, "add", "cache-spill")
        git(self.repo, "commit", "--quiet", "-m", "reread a spilled blob")
        oid = git(self.repo, "hash-object", str(large))
        code = """
import os, sys
from pathlib import Path
with open('cache-spill/large', 'rb', buffering=0) as large:
    assert large.read(1) == b'x'
    for index in range(9):
        with open('cache-spill/spill/%d' % index, 'rb', buffering=0) as filler:
            assert filler.read(1) == bytes([index])
    [raw] = Path.cwd().parent.glob('raw-objects-*')
    assert (raw / sys.argv[1]).is_file()
    assert os.pread(large.fileno(), 1, 2 * 1024 * 1024 - 1) == b'x'
print('reread from spill')
"""
        result, _ = self.invoke_argv(
            ["HEAD", "--write-policy=none", "--", sys.executable, "-c", code, oid]
        )
        self.assert_success(result)
        self.assertEqual(result.stdout, b"reread from spill\n")
        self.assert_no_runtime_files()

    @unittest.skipUnless(sys.platform == "linux", "Linux prlimit")
    def test_checked_block_read_survives_cache_spill_limits_locally_and_remotely(self):
        directory = self.repo / 'cache-spill'
        directory.mkdir()
        (directory / 'large').write_bytes(b'x' * (2 * 1024 * 1024))
        self.add_spill_blobs(directory)
        # 254 complete 64 KiB payloads plus entry charges fill the 16 MiB
        # resident budget and displace the already-backed large-file block.
        for index in range(254):
            (directory / f'f{index:04d}').write_bytes(index.to_bytes(4, 'little') + b's' * (65536 - 4))
        git(self.repo, 'add', 'cache-spill')
        git(self.repo, 'commit', '--quiet', '-m', 'cache-only spill limit')
        source, ca = self.serve_repository()
        code = """
import errno, os, resource
with open('cache-spill/large', 'rb', buffering=0) as large:
    assert large.read(1) == b'x'
    for index in range(9):
        with open('cache-spill/spill/%d' % index, 'rb', buffering=0) as filler:
            assert filler.read(1) == bytes([index])
    for index in range(254):
        with open('cache-spill/f%04d' % index, 'rb', buffering=0) as small:
            assert small.read(4) == index.to_bytes(4, 'little')
    runner = os.getppid()
    original = resource.prlimit(runner, resource.RLIMIT_FSIZE)
    try:
        resource.prlimit(runner, resource.RLIMIT_FSIZE, (3, original[1]))
        # An untouched far block must be read and authenticated from raw disk.
        try:
            data = os.pread(large.fileno(), 1, 2 * 1024 * 1024 - 1)
        except OSError as error:
            assert error.errno in (errno.EIO, errno.EFBIG), error
            print('handled cache-only read error', error.errno)
        else:
            assert data == b'x'
            with open('cache-spill/f0000', 'rb', buffering=0) as resident:
                assert resident.read(4) == b'\\0' * 4
            print('checked bytes survived cache-only spill failure')
    finally:
        resource.prlimit(runner, resource.RLIMIT_FSIZE, original)
"""
        for operands in ([], [source]):
            with self.subTest(source=operands):
                try:
                    # Turn the write limit into EFBIG to exercise failed optional
                    # admission independently of the inherited signal policy.
                    result = subprocess.run([str(BINARY), *operands, 'HEAD', '--write-policy=none', '--', sys.executable, '-c', code],
                                            cwd=self.repo, env=dict(self.environment(), GIT_CONFIG_COUNT='0', GIT_SSL_CAINFO=ca), capture_output=True, timeout=60,
                                            preexec_fn=lambda: signal.signal(signal.SIGXFSZ, signal.SIG_IGN))
                    self.assert_success(result)
                    self.assertEqual(result.stdout, b'checked bytes survived cache-only spill failure\n')
                    self.assert_no_runtime_files()
                finally:
                    for run in (self.system_tmp / "git-on").glob("??????"):
                        self.cleanup_run(run)

    @fuse_only
    def test_partial_backing_write_and_sparse_blocks_remain_visible(self):
        code = r'''
import errno, os, resource
from pathlib import Path

path = Path('out/partial')
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
runner = os.getppid()
original = resource.prlimit(runner, resource.RLIMIT_FSIZE)
try:
    # Limit the FUSE server's backing writes, not the client's write syscall.
    # The server inherited SIGXFSZ ignored solely for this isolated regression.
    resource.prlimit(runner, resource.RLIMIT_FSIZE, (3, original[1]))
    assert os.write(fd, b'abcdef') == 3
    try: os.write(fd, b'x')
    except OSError as error: assert error.errno == errno.EFBIG, error
    else: raise AssertionError('no-progress backing write did not fail')
finally:
    resource.prlimit(runner, resource.RLIMIT_FSIZE, original)
assert os.fstat(fd).st_size == 3
assert os.pread(fd, 10, 0) == b'abc'
assert path.read_bytes() == b'abc'
for size in [1 << 30, 0]:
    os.ftruncate(fd, size)
    visible = os.fstat(fd)
    backing = os.stat(Path.cwd().parent / 'backing' / str(visible.st_ino))
    assert visible.st_size == backing.st_size == size
    assert visible.st_blocks == backing.st_blocks, (visible, backing)
assert os.pwrite(fd, b'x', 1 << 20) == 1
visible = os.fstat(fd)
backing = os.stat(Path.cwd().parent / 'backing' / str(visible.st_ino))
assert visible.st_size == backing.st_size
assert visible.st_blocks == backing.st_blocks, (visible, backing)
assert os.pread(fd, 2, (1 << 20) - 1) == b'\0x'
os.close(fd)
'''
        result = subprocess.run([str(BINARY), 'HEAD', '--', sys.executable, '-c', code],
                                cwd=self.repo, env=self.environment(), capture_output=True,
                                timeout=30,
                                preexec_fn=lambda: signal.signal(signal.SIGXFSZ, signal.SIG_IGN))
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_short_source_write_preserves_unwritten_bytes_after_reopen(self):
        (self.repo/'large').write_bytes(b'x'*(2*65536+17))
        git(self.repo,'add','large');git(self.repo,'commit','-qm','short source write')
        command=[sys.executable,'-I','-S','-c',r'''
import os,resource
from pathlib import Path
fd=os.open('large',os.O_RDWR)
# Copy up before lowering the runner's write limit.
assert os.pwrite(fd,b'y',65536)==1
runner=os.getppid()
original=resource.prlimit(runner,resource.RLIMIT_FSIZE)
try:
 resource.prlimit(runner,resource.RLIMIT_FSIZE,(3,original[1]))
 assert os.pwrite(fd,b'z'*65536,0)==3
finally:resource.prlimit(runner,resource.RLIMIT_FSIZE,original)
os.close(fd)
expected=b'z'*3+b'x'*(65536-3)+b'y'+b'x'*(65536+16)
assert Path('large').read_bytes()==expected
fd=os.open('large',os.O_RDWR)
assert os.pwrite(fd,b'abcd',65534)==4
os.ftruncate(fd,65537);os.ftruncate(fd,65570)
assert os.pread(fd,40,65532)==b'xxabc'+b'\0'*33
os.close(fd)
# Subsequent I/O uses the private file, including bytes carried through copy-up.
backing=Path.cwd().parent/'backing'/str(Path('large').stat().st_ino)
with backing.open('r+b',buffering=0) as out:out.seek(10);out.write(b'!')
with open('large','rb',buffering=0) as reopened:
 assert os.pread(reopened.fileno(),1,10)==b'!'
''']
        result,_=self.invoke_argv(['HEAD','--write-policy=all','--',*command],
                                  preexec_fn=lambda: signal.signal(signal.SIGXFSZ, signal.SIG_IGN))
        self.assert_success(result);self.assert_no_runtime_files()

    @fuse_only
    def test_mknod_regular_files_follow_policy_and_special_nodes_are_refused(self):
        result, _ = self.python(r'''
import errno, os, socket, stat
from pathlib import Path

def denied(action, codes):
    try: action()
    except OSError as error: assert error.errno in codes, error
    else: raise AssertionError('mknod unexpectedly succeeded')

old_umask = os.umask(0o027)
try:
    for path in ['out/node', 'node.cache', 'nested/node.tmp']:
        os.mknod(path, stat.S_IFREG | 0o666)
        attrs = os.stat(path)
        assert stat.S_ISREG(attrs.st_mode) and attrs.st_size == 0
        assert stat.S_IMODE(attrs.st_mode) == 0o640, oct(attrs.st_mode)
        assert (attrs.st_uid, attrs.st_gid) == (os.getuid(), os.getgid())
        denied(lambda: os.mknod(path, stat.S_IFREG | 0o600), (errno.EEXIST,))
        Path(path).write_bytes(b'mknod payload')
        assert Path(path).read_bytes() == b'mknod payload'
        os.unlink(path)
        assert not os.path.lexists(path)
finally:
    os.umask(old_umask)

for path in ['forbidden-node', 'keep.cache', 'nested/keep.tmp']:
    denied(lambda: os.mknod(path, stat.S_IFREG | 0o600),
           (errno.EROFS, errno.EACCES, errno.EPERM))
    assert not os.path.lexists(path)
for path in ['tracked.txt', 'out/protected.txt', 'tracked-link']:
    original = Path(path).read_bytes()
    attrs = os.lstat(path)
    denied(lambda: os.mknod(path, stat.S_IFREG | 0o600), (errno.EEXIST,))
    assert Path(path).read_bytes() == original
    after = os.lstat(path)
    assert (after.st_ino, after.st_mode, after.st_size, after.st_mtime_ns) == (attrs.st_ino, attrs.st_mode, attrs.st_size, attrs.st_mtime_ns)
for kind in [stat.S_IFIFO, stat.S_IFSOCK]:
    path = 'out/special-' + str(kind)
    denied(lambda: os.mknod(path, kind | 0o600), (errno.EPERM,))
    assert not os.path.lexists(path)
denied(lambda: os.mkfifo('out/fifo', 0o600), (errno.EPERM,))
assert not os.path.lexists('out/fifo')
with socket.socket(socket.AF_UNIX) as sock:
    denied(lambda: sock.bind('out/socket'), (errno.EPERM,))
assert not os.path.lexists('out/socket')
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    def test_fsync_fdatasync_and_directory_sync_preserve_live_handles(self):
        result, _ = self.python(r'''
import os
from pathlib import Path

Path('out/syncdir').mkdir()
for path in ['tracked.txt', 'out/protected.txt']:
    expected = Path(path).read_bytes()
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
        getattr(os, "fdatasync", os.fsync)(fd)
        assert os.pread(fd, 100, 0) == expected
    finally: os.close(fd)
fd = os.open('out/syncdir/value', os.O_RDWR | os.O_CREAT, 0o600)
try:
    assert os.write(fd, b'before') == 6
    getattr(os, "fdatasync", os.fsync)(fd)
    assert Path('out/syncdir/value').read_bytes() == b'before'
    duplicate = os.dup(fd)
    os.close(duplicate)  # FLUSH must not invalidate the remaining description.
    assert os.pwrite(fd, b'after!', 0) == 6
    os.fsync(fd)
    assert Path('out/syncdir/value').read_bytes() == b'after!'
    os.unlink('out/syncdir/value')
    assert os.pwrite(fd, b'open', 0) == 4
    os.fsync(fd)
    getattr(os, "fdatasync", os.fsync)(fd)
    assert os.pread(fd, 20, 0) == b'openr!'
    assert os.fstat(fd).st_nlink == 0
finally: os.close(fd)
for path in ['.', 'nested', 'out', 'out/syncdir']:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
        getattr(os, "fdatasync", os.fsync)(fd)
    finally: os.close(fd)
assert not Path('out/syncdir/value').exists()
assert Path('tracked.txt').read_bytes() == b'committed\n'
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    def test_statvfs_reports_coherent_backing_filesystem_capacity(self):
        result, _ = self.python(r'''
import os, sys
from pathlib import Path

Path('out/capacity').write_bytes(b'x' * 8192)
backing = os.statvfs(Path.cwd().parent / 'backing')
for path in ['.', 'nested', 'tracked.txt', 'out/capacity']:
    stats = os.statvfs(path)
    # The Darwin client reports NFS capacity in its own fragment units.
    assert stats.f_blocks * stats.f_frsize == backing.f_blocks * backing.f_frsize, (path, stats, backing)
    if sys.platform != 'darwin':
        for field in ['f_bsize', 'f_frsize', 'f_blocks', 'f_files']:
            assert getattr(stats, field) == getattr(backing, field), (path, field, stats, backing)
    assert stats.f_bsize > 0 and stats.f_frsize > 0 and stats.f_blocks > 0
    assert 0 <= stats.f_bavail <= stats.f_bfree <= stats.f_blocks, stats
    assert 0 <= stats.f_favail <= stats.f_ffree <= stats.f_files, stats
    assert stats.f_namemax == 255
    assert not stats.f_flag & os.ST_RDONLY
    fd = os.open(path, os.O_RDONLY)
    try:
        by_fd = os.fstatvfs(fd)
        assert (by_fd.f_blocks, by_fd.f_bsize, by_fd.f_frsize) == (stats.f_blocks, stats.f_bsize, stats.f_frsize)
    finally: os.close(fd)
# Free counters are live host values and may change concurrently; they are not
# asserted equal between separate requests on a shared CI runner.
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_renameat2_noreplace_and_rejected_flags_are_atomic(self):
        result, _ = self.python(r'''
import ctypes, errno, os
from pathlib import Path

libc = ctypes.CDLL(None, use_errno=True)
renameat2 = libc.renameat2
renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int,
                      ctypes.c_char_p, ctypes.c_uint]
renameat2.restype = ctypes.c_int

def rename(source, destination, flags, codes=()):
    ctypes.set_errno(0)
    result = renameat2(-100, os.fsencode(source), -100, os.fsencode(destination), flags)
    if codes:
        assert result == -1 and ctypes.get_errno() in codes, (source, destination, flags, result, ctypes.get_errno())
    else:
        assert result == 0, (source, destination, flags, ctypes.get_errno())

Path('out/source').write_bytes(b'source')
Path('out/destination').write_bytes(b'destination')
source_ino = os.stat('out/source').st_ino
destination_ino = os.stat('out/destination').st_ino
for flags, codes in [(1, (errno.EEXIST,)), (2, (errno.EINVAL,)),
                     (3, (errno.EINVAL,)), (8, (errno.EINVAL,))]:
    # RENAME_EXCHANGE needs filesystem support; unknown flags can be rejected
    # by the VFS before a request reaches the daemon.
    rename('out/source', 'out/destination', flags, codes)
    assert Path('out/source').read_bytes() == b'source'
    assert Path('out/destination').read_bytes() == b'destination'
    assert os.stat('out/source').st_ino == source_ino
    assert os.stat('out/destination').st_ino == destination_ino
rename('out/source', 'out/source', 1, (errno.EEXIST,))
for target in ['tracked.txt', 'out/protected.txt']:
    original = Path(target).read_bytes()
    rename('out/source', target, 1, (errno.EEXIST, errno.EROFS))
    assert Path(target).read_bytes() == original
    assert Path('out/source').read_bytes() == b'source'
for target in ['forbidden-rename', 'keep.cache', 'nested/keep.tmp']:
    rename('out/source', target, 1, (errno.EROFS, errno.EACCES, errno.EPERM))
    assert not os.path.lexists(target)
    assert Path('out/source').read_bytes() == b'source'
rename('out/source', 'out/new', 1)
assert not Path('out/source').exists()
assert Path('out/new').read_bytes() == b'source'
assert os.stat('out/new').st_ino == source_ino
Path('out/from-dir').mkdir()
Path('out/from-dir/value').write_bytes(b'directory payload')
Path('out/to-dir').mkdir()
for flags, code in [(1, errno.EEXIST), (2, errno.EINVAL)]:
    rename('out/from-dir', 'out/to-dir', flags, (code,))
    assert Path('out/from-dir/value').read_bytes() == b'directory payload'
    assert list(Path('out/to-dir').iterdir()) == []
rename('out/from-dir', 'out/moved-dir', 1)
assert not Path('out/from-dir').exists()
assert Path('out/moved-dir/value').read_bytes() == b'directory payload'
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_readdir_pagination_and_seekdir_preserve_merged_entries(self):
        for index in range(640):
            self.put(f"out/paged/tracked-{index:04d}-" + "x" * 96, "tracked entry\n")
        git(self.repo, "add", "--force", "out/paged")
        git(self.repo, "commit", "--quiet", "-m", "paginated directory fixture")
        result, _ = self.python(r'''
import ctypes, os
from pathlib import Path

# Long names force multiple getdents/FUSE_READDIR replies, rather than merely
# iterating several entries already buffered by libc.
tracked = {('tracked-%04d-' % i + 'x' * 96).encode() for i in range(640)}
mutable = {('mutable-%04d-' % i + 'y' * 96).encode() for i in range(640)}
for name in mutable:
    Path('out/paged', os.fsdecode(name)).write_bytes(b'upper')
expected = tracked | mutable | {b'.', b'..'}
libc = ctypes.CDLL(None, use_errno=True)
class Dirent64(ctypes.Structure):
    _fields_ = [('ino', ctypes.c_uint64), ('offset', ctypes.c_int64),
                ('reclen', ctypes.c_ushort), ('kind', ctypes.c_ubyte),
                ('name', ctypes.c_char * 256)]
libc.opendir.argtypes = [ctypes.c_char_p]
libc.opendir.restype = ctypes.c_void_p
libc.readdir64.argtypes = [ctypes.c_void_p]
libc.readdir64.restype = ctypes.POINTER(Dirent64)
libc.telldir.argtypes = [ctypes.c_void_p]
libc.telldir.restype = ctypes.c_long
libc.seekdir.argtypes = [ctypes.c_void_p, ctypes.c_long]
libc.seekdir.restype = None
libc.rewinddir.argtypes = [ctypes.c_void_p]
libc.rewinddir.restype = None
libc.closedir.argtypes = [ctypes.c_void_p]
libc.closedir.restype = ctypes.c_int
directory = libc.opendir(b'out/paged')
assert directory, ctypes.get_errno()

def read_one():
    ctypes.set_errno(0)
    entry = libc.readdir64(directory)
    if not entry:
        assert ctypes.get_errno() == 0, ctypes.get_errno()
        return None
    entry = entry.contents
    name = bytes(entry.name)
    assert name in expected and entry.ino > 0, (name, entry.ino)
    assert entry.kind == (4 if name in {b'.', b'..'} else 8), (name, entry.kind)
    return name

def read_rest():
    result = []
    while (name := read_one()) is not None:
        result.append(name)
    return result

try:
    prefix = [read_one() for _ in range(450)]
    assert None not in prefix
    cookie = libc.telldir(directory)
    assert cookie >= 0
    suffix = read_rest()
    complete = prefix + suffix
    assert len(complete) == len(expected) and set(complete) == expected
    assert read_one() is None
    libc.seekdir(directory, cookie)
    assert read_rest() == suffix
    libc.rewinddir(directory)
    assert read_rest() == complete
    # An independent open has its own offset and returns the same full set.
    assert set(os.listdir(b'out/paged')) == expected - {b'.', b'..'}
finally:
    assert libc.closedir(directory) == 0

# Repeated partial streams must not rebuild or reserve the whole directory.
# Keep this in the existing mixed fixture so it adds no scale preparation.
def read_prefix(stream):
    rows=[]
    for _ in range(20):
        entry=libc.readdir64(stream)
        assert entry, ctypes.get_errno()
        name=bytes(entry.contents.name);ino=entry.contents.ino
        if name not in {b'.',b'..'}:
            assert os.lstat(b'out/paged/'+name).st_ino==ino,(name,ino)
        rows.append((name,ino))
    return rows
versions=[]
for generation in range(8):
    changing=Path('out/paged/changing-%d.tmp' % generation)
    changing.write_bytes(b'new generation')
    first=libc.opendir(b'out/paged');second=libc.opendir(b'out/paged')
    assert first and second,ctypes.get_errno()
    try:
        prefix=read_prefix(first)
        assert read_prefix(second)==prefix
        libc.seekdir(first,0)
        assert read_prefix(first)==prefix
        # Keep one partly consumed stream across subsequent mutations. There
        # are enough long names to leave unrequested FUSE pages, not just a
        # suffix already buffered by libc. Each stream retains its own names.
        versions.append((first,prefix,libc.telldir(first),changing.name.encode()))
        first=None
    finally:
        if first:
            assert libc.closedir(first)==0
        assert libc.closedir(second)==0
    changing.unlink()
for stream,prefix,cookie,extra in reversed(versions):
    try:
        suffix=[]
        while True:
            ctypes.set_errno(0)
            entry=libc.readdir64(stream)
            if not entry:
                assert ctypes.get_errno()==0,ctypes.get_errno()
                break
            suffix.append((bytes(entry.contents.name),entry.contents.ino))
        complete=prefix+suffix
        wanted=expected|{extra}
        assert len(complete)==len(wanted) and {name for name,_ in complete}==wanted
        libc.seekdir(stream,cookie)
        replay=[]
        while (entry:=libc.readdir64(stream)):
            replay.append((bytes(entry.contents.name),entry.contents.ino))
        assert replay==suffix
        libc.rewinddir(stream)
        assert read_prefix(stream)==prefix
    finally:
        assert libc.closedir(stream)==0
assert set(os.listdir(b'out/paged'))==expected-{b'.',b'..'}
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_xattrs_are_unsupported_without_changing_files_or_directories(self):
        result, _ = self.python(r'''
import errno, os
from pathlib import Path

Path('out/xattr').write_bytes(b'upper xattr bytes')
Path('out/xattrdir').mkdir()
for path in ['out/xattr', 'tracked.txt', 'out/protected.txt', 'out/xattrdir', 'nested']:
    before = os.stat(path)
    content = Path(path).read_bytes() if Path(path).is_file() else sorted(os.listdir(path))
    # Linux maps the unimplemented FUSE callbacks to EOPNOTSUPP and caches
    # that result per operation; repeat to exercise the cached syscall path.
    for _ in range(2):
        for action in [lambda: os.getxattr(path, 'user.git_on_test'),
                       lambda: os.listxattr(path),
                       lambda: os.setxattr(path, 'user.git_on_test', b'value'),
                       lambda: os.removexattr(path, 'user.git_on_test')]:
            try: action()
            except OSError as error: assert error.errno == errno.EOPNOTSUPP, (path, error)
            else: raise AssertionError('unsupported xattr operation succeeded: ' + path)
    after = os.stat(path)
    assert (after.st_mode, after.st_size, after.st_mtime_ns) == (before.st_mode, before.st_size, before.st_mtime_ns)
    assert (Path(path).read_bytes() if Path(path).is_file() else sorted(os.listdir(path))) == content
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_fallocate_and_unknown_ioctl_fail_without_changing_bytes(self):
        result, _ = self.python(r'''
import ctypes, errno, fcntl, os
from pathlib import Path

libc = ctypes.CDLL(None, use_errno=True)
libc.fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int64, ctypes.c_int64]
libc.fallocate.restype = ctypes.c_int
Path('out/allocate').write_bytes(b'unchanged allocation payload')
fd = os.open('out/allocate', os.O_RDWR)
try:
    original = Path('out/allocate').read_bytes()
    # Call fallocate directly: glibc posix_fallocate may emulate allocation
    # with writes, which would not test the unsupported FUSE request.
    for mode in [0, 1, 3]:  # ordinary, KEEP_SIZE, PUNCH_HOLE | KEEP_SIZE
        ctypes.set_errno(0)
        result = libc.fallocate(fd, mode, 0, 4096)
        assert result == -1 and ctypes.get_errno() == errno.EOPNOTSUPP, (mode, result, ctypes.get_errno())
        assert os.fstat(fd).st_size == len(original)
        assert os.pread(fd, 4096, 0) == original
finally: os.close(fd)
for path in ['out/allocate', 'tracked.txt']:
    original = Path(path).read_bytes()
    fd = os.open(path, os.O_RDONLY)
    try:
        for _ in range(2):
            try: fcntl.ioctl(fd, 0x5a7f, 0)  # Private, argument-free _IO command.
            except OSError as error: assert error.errno == errno.ENOTTY, error
            else: raise AssertionError('unknown ioctl unexpectedly succeeded')
        ctypes.set_errno(0)
        assert libc.fallocate(fd, 0, 0, 4096) == -1
        assert ctypes.get_errno() == errno.EBADF
        assert os.pread(fd, 4096, 0) == original
    finally: os.close(fd)
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_posix_and_flock_locks_use_local_kernel_fallback(self):
        result, _ = self.python(r'''
import errno, fcntl, json, os, subprocess, sys
from pathlib import Path

# Remote locking capabilities are not negotiated. These are kernel-local
# locking contracts, not evidence that GETLK/SETLK callbacks were dispatched.
Path('out/locked').write_bytes(b'lock payload')
fd = os.open('out/locked', os.O_RDWR)
child = r"""
import ctypes, errno, fcntl, json, os, sys
class Flock(ctypes.Structure):
    _fields_ = [('kind', ctypes.c_short), ('whence', ctypes.c_short),
                ('start', ctypes.c_int64), ('length', ctypes.c_int64),
                ('pid', ctypes.c_int)]
fd = os.open('out/locked', os.O_RDWR)
try:
    query = Flock(fcntl.F_WRLCK, os.SEEK_SET, 1, 3, 0)
    reply = fcntl.fcntl(fd, fcntl.F_GETLK, bytes(query))
    lock = Flock.from_buffer_copy(reply)
    if sys.argv[1] == 'held':
        assert lock.kind == fcntl.F_WRLCK
        assert (lock.start, lock.length) == (1, 3)
        try: fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 3, 1)
        except OSError as error: assert error.errno in (errno.EACCES, errno.EAGAIN)
        else: raise AssertionError('conflicting POSIX lock succeeded')
        # A disjoint range remains usable while the first range is locked.
        fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 2, 5)
        fcntl.lockf(fd, fcntl.LOCK_UN, 2, 5)
        print(json.dumps(lock.pid))
    else:
        assert lock.kind == fcntl.F_UNLCK
        fcntl.lockf(fd, fcntl.LOCK_EX, 3, 1)
        fcntl.lockf(fd, fcntl.LOCK_UN, 3, 1)
finally: os.close(fd)
"""
try:
    fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 3, 1)
    check = subprocess.run([sys.executable, '-c', child, 'held'], capture_output=True, timeout=10)
    assert check.returncode == 0, check.stderr
    assert json.loads(check.stdout) == os.getpid()
    fcntl.lockf(fd, fcntl.LOCK_UN, 3, 1)
    check = subprocess.run([sys.executable, '-c', child, 'released'], capture_output=True, timeout=10)
    assert check.returncode == 0, check.stderr
    other = os.open('out/locked', os.O_RDWR)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try: fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error: assert error.errno in (errno.EACCES, errno.EAGAIN)
        else: raise AssertionError('conflicting flock succeeded')
        fcntl.flock(fd, fcntl.LOCK_UN)
        fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(other, fcntl.LOCK_UN)
    finally: os.close(other)
    assert os.pread(fd, 20, 0) == b'lock payload'
finally: os.close(fd)
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_poll_and_lseek_use_regular_file_kernel_fallbacks(self):
        result, _ = self.python(r'''
import errno, os, select
from pathlib import Path

Path('out/seek').write_bytes(b'0123456789')
for path, flags in [('tracked.txt', os.O_RDONLY), ('out/seek', os.O_RDWR)]:
    expected = Path(path).read_bytes()
    fd = os.open(path, flags)
    try:
        assert os.lseek(fd, 3, os.SEEK_SET) == 3
        assert os.lseek(fd, 2, os.SEEK_CUR) == 5
        assert os.read(fd, 2) == expected[5:7]
        assert os.lseek(fd, -2, os.SEEK_END) == len(expected) - 2
        assert os.read(fd, 9) == expected[-2:]
        # SET/CUR/END are handled by the VFS. DATA/HOLE reach LSEEK once;
        # ENOSYS then enables Linux's all-data/EOF-hole fallback.
        for _ in range(2):
            assert os.lseek(fd, 2, os.SEEK_DATA) == 2
            assert os.lseek(fd, 2, os.SEEK_HOLE) == len(expected)
            for whence in [os.SEEK_DATA, os.SEEK_HOLE]:
                try: os.lseek(fd, len(expected), whence)
                except OSError as error: assert error.errno == errno.ENXIO
                else: raise AssertionError('seek at EOF unexpectedly succeeded')
        poller = select.poll()
        poller.register(fd, select.POLLIN | select.POLLOUT)
        # An unsupported POLL callback yields default regular-file readiness,
        # including readable EOF; it does not turn poll into ENOSYS.
        for _ in range(2):
            events = dict(poller.poll(1000))
            assert events.get(fd) == select.POLLIN | select.POLLOUT, events
        assert os.pread(fd, len(expected) + 1, 0) == expected
    finally: os.close(fd)
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    @fuse_only
    def test_copy_file_range_kernel_fallback_preserves_offsets_and_snapshot(self):
        result, _ = self.python(r'''
import errno, os
from pathlib import Path

# The filesystem has no COPY_FILE_RANGE implementation. Linux falls back to
# ordinary reads/writes after ENOSYS; syscall success is not callback coverage.
payload = bytes(range(256)) * 257
Path('out/copy-source').write_bytes(payload)
for source in ['tracked.txt', 'out/copy-source']:
    original = Path(source).read_bytes()
    Path('out/copy-destination').write_bytes(b'prefix:')
    src = os.open(source, os.O_RDONLY)
    dst = os.open('out/copy-destination', os.O_RDWR)
    try:
        os.lseek(src, 2, os.SEEK_SET)
        os.lseek(dst, 3, os.SEEK_SET)
        done = 0
        while done < len(original) - 2:
            amount = os.copy_file_range(src, dst, len(original) - 2 - done, 1 + done, 7 + done)
            assert 0 < amount <= len(original) - 2 - done, amount
            done += amount
        assert os.lseek(src, 0, os.SEEK_CUR) == 2
        assert os.lseek(dst, 0, os.SEEK_CUR) == 3
        assert Path('out/copy-destination').read_bytes() == b'prefix:' + original[1:-1]
        assert os.copy_file_range(src, dst, 10, len(original), 0) == 0
        assert os.lseek(src, 0, os.SEEK_SET) == 0
        assert os.lseek(dst, 0, os.SEEK_SET) == 0
        assert os.copy_file_range(src, dst, 3) == 3
        assert os.lseek(src, 0, os.SEEK_CUR) == 3
        assert os.lseek(dst, 0, os.SEEK_CUR) == 3
        os.fsync(dst)
        assert Path('out/copy-destination').read_bytes() == original[:3] + b'fix:' + original[1:-1]
        protected = os.open('tracked.txt', os.O_RDONLY)
        try:
            try: os.copy_file_range(src, protected, 1, 0, 0)
            except OSError as error: assert error.errno == errno.EBADF
            else: raise AssertionError('copy into read-only tracked descriptor succeeded')
        finally: os.close(protected)
        assert Path(source).read_bytes() == original
        assert Path('tracked.txt').read_bytes() == b'committed\n'
    finally:
        os.close(src)
        os.close(dst)
''')
        self.assert_success(result)
        self.assert_no_runtime_files()

    def test_nested_ignore_rules_and_tracked_precedence_survive_cold_lookups(self):
        self.put("nested/deeper/.gitignore", "!local.tmp\n*.cache\n!keep.cache\n")
        self.put("nested/deeper/protected.tmp", "nested tracked\n")
        self.put("nested/deeper/anchor", "anchor\n")
        git(self.repo, "add", "--force", "nested/deeper")
        git(self.repo, "commit", "--quiet", "-m", "three-level ignore policy")
        candidates = ["nested/deeper/local.tmp", "nested/deeper/allowed.tmp",
                      "nested/deeper/cache.cache", "nested/deeper/keep.cache",
                      "plain/allowed.tmp", "nested/deeper/plain.txt"]
        expected = {}
        for candidate in candidates:
            decision = subprocess.run(["git", "check-ignore", "--no-index", "--quiet", "--", candidate], cwd=self.repo)
            self.assertIn(decision.returncode, (0, 1))
            expected[candidate] = decision.returncode == 0
        result, _ = self.invoke(sys.executable, "-I", "-S", "-c", r'''
import errno, json, os, sys
from pathlib import Path
for path, allowed in json.loads(sys.argv[1]).items():
    try: Path(path).write_text('created')
    except OSError as error:
        assert not allowed and error.errno in (errno.EROFS, errno.EACCES, errno.EPERM), (path, error)
    else:
        assert allowed, path
        Path(path).unlink()
# These targets have not first been listed or read by the command.
for path in ['nested/deeper/protected.tmp', 'out/protected.txt']:
    Path('out/replacement').write_text('replacement')
    try: os.replace('out/replacement', path)
    except OSError as error: assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM), error
    else: raise AssertionError('unmaterialized tracked entry was replaced: ' + path)
    assert Path('out/replacement').read_text() == 'replacement'
assert Path('nested/deeper/protected.tmp').read_text() == 'nested tracked\n'
assert Path('out/protected.txt').read_text() == 'tracked despite ignore\n'
''', json.dumps(expected))
        self.assert_success(result)

    def test_concurrent_readers_and_writers_do_not_share_artifacts_or_corrupt_appends(self):
        result, run = self.python(r'''
from concurrent.futures import ThreadPoolExecutor
import os, sys
from pathlib import Path
Path('out/appends').write_bytes(b'')
def worker(index):
    row = ('%04d\n' % index).encode()
    for iteration in range(20):
        assert Path('tracked.txt').read_bytes() == b'committed\n'
        source = Path('out/worker-%d-%d' % (index, iteration))
        source.write_bytes(row)
        renamed = source.with_name(source.name + '-renamed')
        source.rename(renamed)
        assert renamed.read_bytes() == row
        renamed.unlink()
        fd = os.open('out/appends', os.O_WRONLY | os.O_APPEND)
        try: assert os.write(fd, row) == len(row)
        finally: os.close(fd)
with ThreadPoolExecutor(max_workers=8) as pool:
    list(pool.map(worker, range(8)))
rows = Path('out/appends').read_bytes().splitlines()
assert len(rows) == 160, len(rows)
for index in range(8): assert rows.count(('%04d' % index).encode()) == 20
entries = {path.name for path in Path('out').iterdir()}
if sys.platform == 'darwin':
    entries.discard('._appends')  # Native AppleDouble metadata for the live file.
assert entries == {'protected.txt', 'appends'}, entries
print(len(rows))
''')
        self.assert_success(result)
        self.assertEqual(result.stdout, b"160\n")
        self.assertIsNone(run)
        self.assert_no_runtime_files()
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))
        self.assertFalse((self.repo / "out/appends").exists())

    def test_lazy_metadata_does_not_load_unread_blobs_or_descendant_trees(self):
        # Missing, unreachable-to-the-command objects make eager reads observable
        # without a production metrics stream or a persistent manifest.
        self.put("unused/blob", "never read this unique blob\n")
        self.put("unused/deeper/leaf", "never read this descendant\n")
        git(self.repo, "add", "unused")
        git(self.repo, "commit", "--quiet", "-m", "lazy missing-object fixture")
        for path in ["unused/blob", "unused/deeper"]:
            oid = git(self.repo, "rev-parse", "HEAD:" + path)
            loose = self.repo / ".git/objects" / oid[:2] / oid[2:]
            self.assertTrue(loose.is_file(), f"fixture must have a loose object: {path}")
            loose.unlink()
        code = r"""
import os
from pathlib import Path
assert 'unused' in os.listdir('.')
assert set(os.listdir('unused')) == {'blob', 'deeper'}
assert os.stat('tracked.txt').st_size == len(b'committed\n')
assert Path('tracked.txt').read_bytes() == b'committed\n'
for action in [lambda: Path('unused/blob').read_bytes(), lambda: os.listdir('unused/deeper')]:
    try: action()
    except OSError: pass
    else: raise AssertionError('missing object unexpectedly readable')
assert Path('tracked.txt').read_bytes() == b'committed\n'
"""
        for policy in ("none", "ignored", "untracked", "all"):
            with self.subTest(policy=policy):
                result, run = self.invoke_argv(["HEAD", "--write-policy=" + policy,
                                               "--", sys.executable, "-I", "-S", "-c", code])
                self.assert_success(result)
                self.assertIsNone(run)
                self.assert_no_runtime_files()

    def test_large_digit_tree_supports_small_lazy_workloads(self):
        from benchmark import digit_fixture
        tree = digit_fixture(self.repo, 10000)["tree_oid"]
        workloads = ["pass", "import os; assert os.listdir('.') == ['0']",
                     "import os; assert os.stat('0/0/6/4/2/1').st_size == 7",
                     "import os; assert not os.path.exists('0/0/6/4/2/missing')",
                     "from pathlib import Path; assert Path('0/0/6/4/2/1').read_bytes() == b'006421\\n'"]
        for code in workloads:
            with self.subTest(code=code):
                result, run = self.invoke(sys.executable, "-I", "-S", "-c", code, tree=tree)
                self.assert_success(result)
                self.assertIsNone(run)
                self.assert_no_runtime_files()

    def test_lazy_writes_load_only_relevant_ancestor_ignore_files(self):
        for index in range(12):
            self.put(f"untouched-{index}/.gitignore", f"unused-{index}/\n")
            self.put(f"untouched-{index}/payload", "unread sibling content\n")
        git(self.repo, "add", "--force", "--all")
        git(self.repo, "commit", "--quiet", "-m", "untouched policy subtrees")
        for index in range(12):
            oid = git(self.repo, "rev-parse", f"HEAD:untouched-{index}/.gitignore")
            loose = self.repo / ".git/objects" / oid[:2] / oid[2:]
            self.assertTrue(loose.is_file())
            loose.unlink()
        result, run = self.invoke(sys.executable, "-I", "-S", "-c", r'''
import os
from pathlib import Path
Path('nested/first.tmp').write_text('first')
with open('nested/first.tmp', 'a') as output: output.write(' appended')
os.rename('nested/first.tmp', 'nested/second.tmp')
assert Path('nested/second.tmp').read_text() == 'first appended'
os.truncate('nested/second.tmp', 0)
os.unlink('nested/second.tmp')
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_repeated_snapshot_reads_share_identical_blob_contents(self):
        # Read aliases of one immutable blob through independent file handles.
        # Cache bounds and decode counts are covered by Snapshot unit tests.
        for index in range(16):
            self.put(f"shared/file-{index}", "shared payload\n")
        git(self.repo, "add", "shared")
        git(self.repo, "commit", "--quiet", "-m", "shared immutable blob")
        result, run = self.invoke(sys.executable, "-I", "-S", "-c", r'''
from pathlib import Path
for _ in range(3):
    for index in range(16):
        assert Path('shared/file-%d' % index).read_bytes() == b'shared payload\n'
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    @fuse_only
    def test_evicted_small_source_survives_removal_of_its_original_git_object(self):
        # Distinct 64 KiB objects exceed the production 16 MiB payload budget.
        # Request removal of the earlier clean kernel data pages with DONTNEED
        # so the final reread exercises the mounted store after eviction.
        directory = self.repo / "evicted"
        directory.mkdir()
        git(self.repo, "config", "gc.auto", "0")
        git(self.repo, "config", "maintenance.auto", "false")
        for index in range(260):
            payload = index.to_bytes(4, "big") + bytes([index % 251]) * (65536 - 4)
            (directory / f"{index:04d}").write_bytes(payload)
        git(self.repo, "add", "evicted")
        git(self.repo, "commit", "--quiet", "-m", "distinct cache-eviction objects")
        oid = git(self.repo, "rev-parse", "HEAD:evicted/0000")
        original = self.repo / ".git/objects" / oid[:2] / oid[2:]
        self.assertTrue(original.is_file())
        result, run = self.invoke_argv([
            "HEAD", "--write-policy=none", "--", sys.executable, "-I", "-S", "-c", r'''
import os, sys
from pathlib import Path
original = Path(sys.argv[1])
assert original.is_file(), original
for index in range(260):
    expected = index.to_bytes(4, 'big') + bytes([index % 251]) * (65536 - 4)
    with open('evicted/%04d' % index, 'rb', buffering=0) as source:
        assert source.read() == expected
        os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    if index == 0:
        original.unlink()
with open('evicted/0000', 'rb', buffering=0) as source:
    assert source.read() == bytes(65536)
''', str(original),
        ])
        self.assert_success(result)
        self.assertFalse(original.exists())
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_large_snapshot_blob_random_and_sequential_reads_agree(self):
        import hashlib
        payload = bytes(range(256)) * (16 * 1024 * 1024 // 256)
        (self.repo / "large.bin").write_bytes(payload)
        expected = hashlib.sha256(payload).hexdigest()
        git(self.repo, "add", "large.bin")
        git(self.repo, "commit", "--quiet", "-m", "large immutable read fixture")
        result, run = self.invoke(sys.executable, "-I", "-S", "-c", r'''
import hashlib, os, sys
size = 16 * 1024 * 1024
fd = os.open('large.bin', os.O_RDONLY)
try:
    assert os.pread(fd, 4096, size - 4096) == bytes(range(256)) * 16
    actual = hashlib.sha256()
    while True:
        chunk = os.read(fd, 65536)
        if not chunk: break
        actual.update(chunk)
    assert actual.hexdigest() == sys.argv[1]
    assert os.pread(fd, 4096, 0) == bytes(range(256)) * 16
    assert os.pread(fd, 4096, size) == b''
finally:
    os.close(fd)
''', expected)
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_unsupported_entries_fail_when_accessed_without_poisoning_siblings(self):
        pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:" + "a" * 64 + "\nsize 123\n"
        self.put("unsupported/lfs.bin", pointer)
        self.put("unsupported/ordinary.txt", "ordinary\n")
        git(self.repo, "add", "unsupported")
        head = git(self.repo, "rev-parse", "HEAD")
        git(self.repo, "update-index", "--add", "--cacheinfo", f"160000,{head},unsupported/module")
        git(self.repo, "commit", "--quiet", "-m", "unsupported objects accessed lazily")
        code = r'''
import errno, os
from pathlib import Path
assert Path('tracked.txt').read_text() == 'committed\n'
assert set(os.listdir('unsupported')) == {'lfs.bin', 'ordinary.txt', 'module'}
assert Path('unsupported/ordinary.txt').read_text() == 'ordinary\n'
for action in [lambda: Path('unsupported/lfs.bin').read_bytes(),
               lambda: os.stat('unsupported/module')]:
    try: action()
    except OSError as error: assert error.errno in (errno.EIO, errno.EOPNOTSUPP, errno.ENOSYS), error
    else: raise AssertionError('unsupported object was presented as usable')
assert Path('unsupported/ordinary.txt').read_text() == 'ordinary\n'
'''
        for policy in ("none", "ignored", "untracked", "all"):
            with self.subTest(policy=policy):
                result, run = self.invoke_argv(["HEAD", "--write-policy=" + policy,
                                               "--", sys.executable, "-I", "-S", "-c", code])
                self.assert_success(result)
                self.assertIsNone(run)
                self.assert_no_runtime_files()

    def test_runtime_ignore_file_cannot_expand_write_policy(self):
        result, _ = self.python('''
from pathlib import Path
Path('plain/.gitignore').write_text('*\\n')
try: Path('plain/new-source.py').write_text('not allowed')
except OSError: pass
else: raise AssertionError('runtime ignores changed pinned policy')
try: Path('.gitignore').write_text('*\\n')
except OSError: pass
else: raise AssertionError('tracked ignore file was writable')
''')
        self.assert_success(result)

    def test_symlink_and_hardlink_cannot_mutate_snapshot(self):
        result, _ = self.python('''
import os
from pathlib import Path
for make, path in [(lambda: os.symlink('../tracked.txt','out/link'), 'out/link'),
                   (lambda: os.link('tracked.txt','out/hard'), 'out/hard')]:
    try: make()
    except OSError: continue
    try: Path(path).write_text('bad')
    except OSError: pass
    else: raise AssertionError('alias allowed mutation')
assert Path('tracked.txt').read_text() == 'committed\\n'
Path('out/owned').write_text('one')
try: os.link('out/owned', 'out/alias')
except OSError: pass
else:
    Path('out/alias').write_text('two')
    assert Path('out/owned').read_text() == 'two'
''')
        self.assert_success(result)

    def test_process_exit_and_signal_are_propagated_and_cleaned(self):
        result, run = self.python("raise SystemExit(37)")
        self.assertEqual(result.returncode, 37)
        self.assertEqual(result.stderr, b"")
        self.assertIsNone(run)
        self.assert_no_runtime_files()
        result, run = self.python("import os,signal; os.kill(os.getpid(), signal.SIGTERM)")
        self.assertEqual(result.returncode, -signal.SIGTERM)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_missing_executable_reports_runner_error_and_removes_tempdir(self):
        result, run = self.invoke("git-on-certainly-nonexistent-executable")
        self.assertEqual(result.returncode, 125)
        self.assertIn(b"git-on", result.stderr)
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_simultaneous_runs_use_private_unique_directories_and_artifacts(self):
        processes = []
        ready_files = [self.root / f"parallel-ready-{index}" for index in range(3)]
        release = self.root / "parallel-release"
        try:
            for index, ready in enumerate(ready_files):
                code = r"""
import json, os, sys, time
from pathlib import Path
index, ready, release = sys.argv[1:]
Path('out/shared').write_text(index)
pending = Path(ready + '.pending')
pending.write_text(str(Path.cwd().parent))
pending.replace(ready)
deadline = time.monotonic() + 20
while not Path(release).exists() and time.monotonic() < deadline: time.sleep(.01)
assert Path(release).exists(), 'parent did not release concurrent runs'
assert Path('out/shared').read_text() == index
print(index)
"""
                processes.append(subprocess.Popen([str(BINARY), "HEAD", *MOUNT_ARGS, "--", sys.executable,
                                                    "-c", code, str(index), str(ready), str(release)],
                                                   cwd=self.repo, env=self.environment(),
                                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE))
            deadline = time.monotonic() + 20
            while not all(path.exists() for path in ready_files) and time.monotonic() < deadline:
                if any(process.poll() is not None for process in processes): break
                time.sleep(.02)
            self.assertTrue(all(path.exists() for path in ready_files), "parallel children did not become ready")
            runs = [Path(path.read_text()) for path in ready_files]
            self.assertEqual(len(set(runs)), 3)
            self.assertEqual(list(self.system_tmp.iterdir()), [self.system_tmp / "git-on"])
            for run in runs:
                self.assertEqual(run.parent, self.system_tmp / "git-on")
                self.assertEqual(run.stat().st_mode & 0o777, 0o700)
                self.assertFalse((run / ".state").exists())
                self.assertFalse((run / ".lock").exists())
                self.assertFalse((run / "manifest.json").exists())
                self.assertFalse((run / "stdout.log").exists())
                self.assertFalse((run / "stderr.log").exists())
            release.touch()
            for index, process in enumerate(processes):
                stdout, stderr = process.communicate(timeout=30)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual(stdout, f"{index}\n".encode())
                self.assertEqual(stderr, b"")
            self.assert_no_runtime_files()
            self.assertFalse((self.repo / "out/shared").exists())
        finally:
            release.touch()
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                try: process.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate(timeout=5)

    def test_append_flags_and_shared_mmap_follow_current_open_description(self):
        result, _ = self.python('''
import fcntl, mmap, os
from pathlib import Path
path = Path('out/append')
path.write_bytes(b'abc')
fd = os.open(path, os.O_RDWR | os.O_APPEND)
with mmap.mmap(fd, 3, access=mmap.ACCESS_WRITE) as view:
    view[0] = ord('X')
    view.flush()
os.close(fd)
assert path.read_bytes() == b'Xbc', path.read_bytes()
path.write_bytes(b'abc')
fd = os.open(path, os.O_RDWR | os.O_APPEND)
fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) & ~os.O_APPEND)
os.lseek(fd, 0, os.SEEK_SET)
os.write(fd, b'X')
os.close(fd)
assert path.read_bytes() == b'Xbc', path.read_bytes()
path.write_bytes(b'abc')
fd = os.open(path, os.O_RDWR)
fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) | os.O_APPEND)
os.lseek(fd, 0, os.SEEK_SET)
os.write(fd, b'X')
os.close(fd)
assert path.read_bytes() == b'abcX', path.read_bytes()
''')
        self.assert_success(result)

    def test_shared_mmap_and_record_locks(self):
        result, left = self.invoke_argv([
            "HEAD", "--write-policy=all", "--", sys.executable, "-c", r"""
import errno, fcntl, mmap, os, subprocess, sys
with open('tracked.txt', 'r+b', buffering=0) as stream:
    fd = stream.fileno()
    os.ftruncate(fd, 4096)
    with mmap.mmap(fd, 4096, access=mmap.ACCESS_WRITE) as view:
        view[:4] = b'MMAP'
        view.flush()
    os.fsync(fd)
    assert os.pread(fd, 4, 0) == b'MMAP'
    fcntl.lockf(fd, fcntl.LOCK_EX, 8)
    contender = '''
import errno, fcntl, sys
with open('tracked.txt', 'r+b', buffering=0) as file:
    try:
        fcntl.lockf(file, fcntl.LOCK_EX | fcntl.LOCK_NB, 8)
    except OSError as error:
        assert sys.argv[1] == 'held' and error.errno in (errno.EACCES, errno.EAGAIN), error
    else:
        assert sys.argv[1] == 'released'
'''
    subprocess.run([sys.executable, '-c', contender, 'held'], check=True)
    fcntl.lockf(fd, fcntl.LOCK_UN, 8)
    subprocess.run([sys.executable, '-c', contender, 'released'], check=True)
""",
        ])
        self.assert_success(result)
        self.assertIsNone(left)
        self.assert_no_runtime_files()


    def test_interactive_pty_stdin_has_foreground_process_group(self):
        code = "import os,sys; assert all(os.isatty(fd) for fd in (0,1,2)); assert os.tcgetpgrp(0) == os.getpgrp(); print('input-ready',flush=True); assert sys.stdin.readline() == 'terminal-input\\n'; print('terminal-ok',flush=True)"
        pid, fd = pty.fork()
        if pid == 0:
            import termios
            tty_settings = termios.tcgetattr(0)
            tty_settings[3] |= termios.TOSTOP
            termios.tcsetattr(0, termios.TCSANOW, tty_settings)
            os.chdir(self.repo)
            env = self.environment()
            env["PATH"] = str(BINARY.parent) + os.pathsep + env.get("PATH", "")
            os.execvpe("git", ["git", "on", "HEAD", "--", sys.executable, "-c", code], env)
        captured = bytearray()
        status = None
        sent = False
        deadline = time.monotonic() + 15
        try:
            while time.monotonic() < deadline:
                if select.select([fd], [], [], 0.05)[0]:
                    try: chunk = os.read(fd, 65536)
                    except OSError: chunk = b""
                    captured.extend(chunk)
                    if not sent and b"input-ready" in captured:
                        os.write(fd, b"terminal-input\n")
                        sent = True
                finished, value = os.waitpid(pid, os.WNOHANG)
                if finished:
                    status = value
                    while select.select([fd], [], [], 0.05)[0]:
                        try: tail = os.read(fd, 65536)
                        except OSError: break
                        if not tail: break
                        captured.extend(tail)
                    break
            self.assertIsNotNone(status, f"interactive child hung: {captured!r}")
            self.assertEqual(os.waitstatus_to_exitcode(status), 0, captured)
            self.assertIn(b"terminal-ok", captured)
        finally:
            if status is None:
                try: os.kill(pid, signal.SIGTERM)
                except ProcessLookupError: pass
                time.sleep(0.1)
                try: os.kill(pid, signal.SIGKILL)
                except ProcessLookupError: pass
                os.waitpid(pid, 0)
            os.close(fd)

    def test_terminal_stop_continue_is_relayed_to_foreground_job(self):
        code = "import os,signal; print('stop-ready',flush=True); os.kill(os.getpid(),signal.SIGSTOP); assert os.tcgetpgrp(0)==os.getpgrp(); print('resumed-ok',flush=True)"
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.repo)
            env = self.environment()
            env["PATH"] = str(BINARY.parent) + os.pathsep + env.get("PATH", "")
            os.execvpe("git", ["git", "on", "HEAD", "--", sys.executable, "-c", code], env)
        captured = bytearray()
        status = None
        stops = 0
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if select.select([fd], [], [], 0.05)[0]:
                    try: captured.extend(os.read(fd, 65536))
                    except OSError: pass
                finished, value = os.waitpid(pid, os.WNOHANG | os.WUNTRACED)
                if finished and os.WIFSTOPPED(value):
                    stops += 1
                    os.killpg(pid, signal.SIGCONT)
                elif finished:
                    status = value
                    while select.select([fd], [], [], 0.05)[0]:
                        try: tail = os.read(fd, 65536)
                        except OSError: break
                        if not tail: break
                        captured.extend(tail)
                    break
            self.assertIsNotNone(status, f"job control hung: {captured!r}")
            self.assertEqual(os.waitstatus_to_exitcode(status), 0, captured)
            self.assertEqual(stops, 1, captured)
            self.assertIn(b"resumed-ok", captured)
        finally:
            if status is None:
                try: os.killpg(pid, signal.SIGCONT); os.killpg(pid, signal.SIGTERM)
                except ProcessLookupError: pass
                time.sleep(.1)
                try: os.kill(pid, signal.SIGKILL)
                except ProcessLookupError: pass
                os.waitpid(pid, 0)
            os.close(fd)

    def assert_pty_controller(self, controller, input_marker=None, terminal_input=None):
        """Run a small shell-like controller with a real controlling terminal."""
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.chdir(self.repo)
                controller()
            except BaseException:
                import traceback
                traceback.print_exc()
                os._exit(1)
            os._exit(0)
        captured = bytearray()
        status = None
        sent = False
        try:
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                if select.select([fd], [], [], .05)[0]:
                    try: chunk = os.read(fd, 65536)
                    except OSError: chunk = b""
                    captured.extend(chunk)
                    if input_marker and not sent and input_marker in captured:
                        os.write(fd, terminal_input)
                        sent = True
                finished, value = os.waitpid(pid, os.WNOHANG)
                if finished:
                    status = value
                    while select.select([fd], [], [], .05)[0]:
                        try: tail = os.read(fd, 65536)
                        except OSError: break
                        if not tail: break
                        captured.extend(tail)
                    break
            self.assertIsNotNone(status, f"terminal controller hung: {captured!r}")
            self.assertEqual(os.waitstatus_to_exitcode(status), 0, captured)
        finally:
            if status is None:
                try: os.killpg(pid, signal.SIGKILL)
                except ProcessLookupError: pass
                os.waitpid(pid, 0)
            os.close(fd)
        self.assert_no_runtime_files()
        return captured

    def test_background_job_can_move_to_foreground_and_read_terminal(self):
        def controller():
            signal.signal(signal.SIGTTOU, signal.SIG_IGN)
            code = r"""
import os, sys
assert all(os.isatty(fd) for fd in (0, 1, 2))
# The runner is initially a background job. Reading must stop the job,
# allowing the controller to perform the equivalent of the shell's fg.
assert sys.stdin.readline() == 'foreground-input\n'
assert os.tcgetpgrp(0) == os.getpgrp()
print('foreground-read-ok', flush=True)
"""
            process = subprocess.Popen([str(BINARY), "HEAD", "--", sys.executable, "-c", code],
                                       cwd=self.repo, env=self.environment(), preexec_fn=os.setpgrp)
            reaped = False
            try:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    finished, status = os.waitpid(process.pid, os.WNOHANG | os.WUNTRACED)
                    if finished:
                        assert os.WIFSTOPPED(status), ('background runner did not stop', status)
                        break
                    time.sleep(.01)
                else: raise AssertionError('background terminal reader did not stop')
                assert os.tcgetpgrp(0) == os.getpgrp(), 'background runner stole the terminal'
                print('bringing-job-foreground', flush=True)
                os.tcsetpgrp(0, process.pid)
                os.killpg(process.pid, signal.SIGCONT)
                _, status = os.waitpid(process.pid, 0)
                reaped = True
                process.returncode = os.waitstatus_to_exitcode(status)
                assert process.returncode == 0, status
                assert os.tcgetpgrp(0) == process.pid, 'runner did not restore foreground group'
                os.tcsetpgrp(0, os.getpgrp())
                print('controller-restored', flush=True)
            finally:
                if not reaped:
                    try: os.killpg(process.pid, signal.SIGCONT); process.terminate()
                    except ProcessLookupError: pass
                    try: process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                os.tcsetpgrp(0, os.getpgrp())
        captured = self.assert_pty_controller(controller, b"bringing-job-foreground",
                                              b"foreground-input\n")
        self.assertIn(b"foreground-read-ok", captured)
        self.assertIn(b"controller-restored", captured)

    def test_running_background_job_receives_foreground_without_sigcont(self):
        ready = self.root / "running-background-ready"
        def controller():
            signal.signal(signal.SIGTTOU, signal.SIG_IGN)
            code = r"""
import os, sys, time
from pathlib import Path
assert all(os.isatty(fd) for fd in (0, 1, 2))
with open('/dev/tty', 'rb', buffering=0) as tty:
    assert os.tcgetpgrp(tty.fileno()) != os.getpgrp(), 'job did not start in the background'
    print('running-background-ready', flush=True)
    Path(sys.argv[1]).touch()
    # A running background job does not stop itself or read the terminal.
    # Bash's fg changes terminal ownership without sending SIGCONT here.
    time.sleep(.4)
    assert os.tcgetpgrp(tty.fileno()) == os.getpgrp(), 'running child did not receive foreground ownership'
print('running-foreground-ok', flush=True)
"""
            process = subprocess.Popen([str(BINARY), "HEAD", "--", sys.executable, "-c", code, str(ready)],
                                       cwd=self.repo, env=self.environment(), preexec_fn=os.setpgrp)
            reaped = False
            try:
                deadline = time.monotonic() + 15
                while not ready.exists() and time.monotonic() < deadline:
                    assert process.poll() is None, 'background job exited before readiness'
                    time.sleep(.005)
                assert ready.exists(), 'background job did not become ready'
                finished, status = os.waitpid(process.pid, os.WNOHANG | os.WUNTRACED)
                assert finished == 0, ('background job must still be running', status)
                assert os.tcgetpgrp(0) == os.getpgrp(), 'background runner stole the terminal'
                os.tcsetpgrp(0, process.pid)
                # Deliberately no SIGCONT: terminal ownership alone must cause
                # the runner to transfer the foreground to its existing child.
                process.wait(timeout=10)
                reaped = True
                assert process.returncode == 0, process.returncode
                assert os.tcgetpgrp(0) == process.pid, 'runner did not restore foreground group'
            finally:
                if not reaped:
                    try: os.killpg(process.pid, signal.SIGCONT); process.terminate()
                    except ProcessLookupError: pass
                    try: process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                os.tcsetpgrp(0, os.getpgrp())
        captured = self.assert_pty_controller(controller)
        self.assertIn(b"running-background-ready", captured)
        self.assertIn(b"running-foreground-ok", captured)

    def test_redirected_stdin_preserves_controlling_terminal_job_control(self):
        def controller():
            import termios
            settings = termios.tcgetattr(0)
            settings[3] |= termios.TOSTOP
            termios.tcsetattr(0, termios.TCSANOW, settings)
            signal.signal(signal.SIGTTOU, signal.SIG_IGN)
            code = r"""
import os, signal
assert not os.isatty(0)
assert os.isatty(1) and os.isatty(2)
with open('/dev/tty', 'rb', buffering=0) as tty:
    assert os.tcgetpgrp(tty.fileno()) == os.getpgrp()
    os.kill(os.getpid(), signal.SIGSTOP)
    assert os.tcgetpgrp(tty.fileno()) == os.getpgrp()
print('redirected-terminal-ok', flush=True)
"""
            # Give the runner its own foreground job group, just as
            # `git on ... </dev/null` invoked by an interactive shell.
            process = subprocess.Popen([str(BINARY), "HEAD", "--", sys.executable, "-c", code],
                                       cwd=self.repo, env=self.environment(),
                                       stdin=subprocess.DEVNULL, preexec_fn=os.setpgrp)
            reaped = False
            try:
                os.tcsetpgrp(0, process.pid)
                deadline = time.monotonic() + 15
                stops = 0
                while time.monotonic() < deadline:
                    finished, status = os.waitpid(process.pid, os.WNOHANG | os.WUNTRACED)
                    if finished and os.WIFSTOPPED(status):
                        stops += 1
                        os.tcsetpgrp(0, process.pid)
                        os.killpg(process.pid, signal.SIGCONT)
                    elif finished:
                        reaped = True
                        process.returncode = os.waitstatus_to_exitcode(status)
                        assert process.returncode == 0, status
                        assert stops == 1, stops
                        assert os.tcgetpgrp(0) == process.pid
                        break
                    time.sleep(.01)
                else: raise AssertionError('redirected-stdin job control hung')
            finally:
                if not reaped:
                    try: os.killpg(process.pid, signal.SIGCONT); process.terminate()
                    except ProcessLookupError: pass
                    try: process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                os.tcsetpgrp(0, os.getpgrp())
        captured = self.assert_pty_controller(controller)
        self.assertIn(b"redirected-terminal-ok", captured)

    def test_deep_ignore_negation_is_not_lost_to_matcher_limits(self):
        name = "a" * 100
        self.put(".gitignore", "*\n!" + "*a" * 100 + "\n")
        git(self.repo, "add", "--force", ".gitignore")
        git(self.repo, "commit", "--quiet", "-m", "deep negative ignore rule")
        oracle = subprocess.run(["git", "check-ignore", "--no-index", "--quiet", "--", name], cwd=self.repo)
        self.assertEqual(oracle.returncode, 1)
        code = "from pathlib import Path\nimport errno,sys\ntry: Path(sys.argv[1]).write_text('denied')\nexcept OSError as e: assert e.errno in (errno.EROFS, errno.EACCES, errno.EPERM), e\nelse: raise AssertionError('negation was ignored')\nPath('allowed').write_text('works')"
        result, _ = self.invoke(sys.executable, "-c", code, name)
        self.assert_success(result)

    def test_escaped_descendant_retains_busy_mount_until_explicit_cleanup(self):
        pidfile = self.root / "escaped-pid"
        runfile = self.root / "escaped-run"
        descendant = f"import os,time; from pathlib import Path; Path({str(pidfile)!r}).write_text(str(os.getpid())); time.sleep(300)"
        code = r"""
import subprocess, sys, time
from pathlib import Path
Path(sys.argv[3]).write_text(str(Path.cwd().parent))
assert list(Path.cwd().parent.glob('t*/socket'))
Path('out/recoverable').write_text('artifact')
subprocess.Popen([sys.executable,'-c',sys.argv[1]], start_new_session=True,
                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
p = Path(sys.argv[2])
deadline = time.monotonic() + 5
while not p.exists() and time.monotonic() < deadline: time.sleep(.01)
assert p.exists()
"""
        run = None
        try:
            with controlling_terminal() as attach:
                result, run = self.invoke_argv([
                    "HEAD", "--", sys.executable, "-c", code, descendant, str(pidfile), str(runfile),
                ], timeout=20, preexec_fn=attach)
            self.assertEqual(result.returncode, 125, result.stderr)
            self.assertIsNotNone(run)
            self.assertEqual(run, Path(runfile.read_text()))
            self.assertIn(os.fsencode(run), result.stderr)
            self.assertNotIn(b"pin preserved", result.stderr)
            # A disconnected FUSE mount returns ENOTCONN to stat(), so
            # os.path.ismount misleadingly reports false. Inspect mountinfo.
            if sys.platform == "darwin":
                mounts = subprocess.check_output(["/sbin/mount"], text=True).splitlines()
                mountpoints = [line.partition(" on ")[2].rpartition(" (")[0] for line in mounts]
            else:
                mountpoints = [line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()]
            self.assertIn(str(run / "mount"), mountpoints, "busy mount must remain for recovery")
            self.assertTrue((run / "backing").is_dir())
            self.assertFalse((run / ".state").exists())
            self.assertFalse(list(run.glob("t*")))
            self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))
            cleanup = self.cleanup_run(run)
            if sys.platform == "darwin":
                self.assert_success(cleanup)  # The checked recovery detaches a dead server.
            else:
                self.assertNotEqual(cleanup.returncode, 0, "live escaped cwd must remain busy")
                self.assertTrue((run / "backing").is_dir())
        finally:
            if pidfile.exists():
                try: os.killpg(int(pidfile.read_text()), signal.SIGKILL)
                except ProcessLookupError: pass
            if run is None and runfile.exists(): run = Path(runfile.read_text())
            if run is not None and run.exists():
                self.assert_success(self.cleanup_run(run))
        self.assert_no_runtime_files()

    def test_non_utf8_command_argument_is_preserved(self):
        raw = b"-bad-\xff-name"
        code = f"import os,sys; assert list(map(os.fsencode, sys.argv[1:])) == {[raw, b'--', b'-k', b'--keep', b'']!r}"
        prefix = [b"HEAD", b"--"]
        result, run = self.invoke_argv([*prefix, os.fsencode(sys.executable), b"-c",
                                       code.encode(), raw, b"--", b"-k", b"--keep", b""])
        self.assert_success(result)
        self.assertEqual(result.stderr, b"")
        self.assertIsNone(run)
        self.assert_no_runtime_files()

    def test_ignore_decisions_match_git_for_complex_patterns(self):
        self.put(".gitignore", "out/\n*.tmp\n!keep.tmp\n/root-only.dat\nnested/a[0-9].bin\nnested/**/deep.bin\nescaped\\ space.dat\n\\#hash.dat\n\\!bang.dat\n*.trailing\\ \ndirs/\n!dirs/keep.txt\n{foo,bar}.dat\ntabbed\t\nnbsp\u00a0\n[unclosed\nx[[:digit:]].dat\n")
        self.put("nested/.gitignore", "!keep.tmp\n")
        self.put("nested/deeper/anchor", "tracked")
        git(self.repo, "add", "--force", "--all")
        git(self.repo, "commit", "--quiet", "-m", "complex ignore patterns")
        candidates = ["a.tmp", "keep.tmp", "nested/a.tmp", "nested/keep.tmp", "root-only.dat", "nested/root-only.dat", "nested/a7.bin", "nested/aa.bin", "nested/deep.bin", "nested/deeper/deep.bin", "escaped space.dat", "#hash.dat", "!bang.dat", "x.trailing ", "x.trailing", "dirs/keep.txt", "{foo,bar}.dat", "foo.dat", "bar.dat", "tabbed", "tabbed\t", "nbsp", "nbsp\u00a0", "[unclosed", "x7.dat", "xa.dat"]
        expected = {}
        for candidate in candidates:
            decision = subprocess.run(["git", "check-ignore", "--no-index", "--quiet", "--", candidate], cwd=self.repo)
            self.assertIn(decision.returncode, (0, 1))
            expected[candidate] = decision.returncode == 0
        code = "from pathlib import Path\nimport json,sys\nPath('dirs').mkdir()\nfor name,allowed in json.loads(sys.argv[1]).items():\n try: Path(name).write_text('created')\n except OSError:\n  assert not allowed, name + ' should be allowed'\n else:\n  assert allowed, name + ' should be denied'"
        result, _ = self.invoke(sys.executable, "-c", code, json.dumps(expected))
        self.assert_success(result)

    def test_worktree_local_and_global_ignore_settings_are_excluded(self):
        (self.repo / ".git/info/exclude").write_text("external-only.tmp\n")
        global_ignore = self.root / "global-ignore"
        global_ignore.write_text("global-only.tmp\n")
        git(self.repo, "config", "core.excludesFile", str(global_ignore))
        result, _ = self.python("from pathlib import Path\nfor name in ['external-only.tmp','global-only.tmp']:\n try: Path(name).write_text('bad')\n except OSError: pass\n else: raise AssertionError(name)")
        self.assert_success(result)

    def start_sleeping_run(self):
        ready = self.root / "child-ready"
        code = r"""
import json, os, sys, time
from pathlib import Path
Path('out/keep').write_text('artifact')
ready = Path(sys.argv[1])
pending = ready.with_suffix('.pending')
pending.write_text(json.dumps({'pid': os.getpid(), 'run': str(Path.cwd().parent)}))
pending.replace(ready)
time.sleep(300)
"""
        process = subprocess.Popen([str(BINARY), "HEAD", *MOUNT_ARGS, "--",
                                    sys.executable, "-c", code, str(ready)], cwd=self.repo,
                                   env=self.environment(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 20
        while not ready.exists() and time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.02)
        if not ready.exists():
            process.kill()
            _, stderr = process.communicate(timeout=5)
            self.fail(f"child did not start: {stderr!r}")
        data = json.loads(ready.read_text())
        child_pid, run = data["pid"], Path(data["run"])
        def stop():
            try: os.killpg(child_pid, signal.SIGKILL)
            except ProcessLookupError: pass
            if process.poll() is None: process.terminate()
            try: process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=5)
            if run.exists():
                self.cleanup_run(run)
        self.addCleanup(stop)
        return process, run, child_pid

    def test_forwarded_signal_removes_runtime_files_and_active_cleanup_is_refused(self):
        process, run, _ = self.start_sleeping_run()
        active_cleanup = self.cleanup_run(run)
        self.assertNotEqual(active_cleanup.returncode, 0)
        self.assertFalse((run / ".lock").exists())
        self.assertTrue((run / "backing").is_dir())
        process.send_signal(signal.SIGTERM)
        _, stderr = process.communicate(timeout=15)
        self.assertEqual(process.returncode, -signal.SIGTERM, stderr)
        self.assert_no_runtime_files()

    def test_sigkill_recovery_explicitly_removes_stale_tempdir(self):
        self.recover_stale_run()

    @unittest.skipUnless(sys.platform == "darwin", "requires an NFS mount")
    def test_sigkill_recovery_after_nfs_socket_directory_disappears(self):
        self.recover_stale_run(remove_socket=True)

    def recover_stale_run(self, remove_socket=False):
        socket_dir = None
        if sys.platform == "darwin":
            group = self.system_tmp / "Git-On"
            group.mkdir(mode=0o700)
            if not (self.system_tmp / "git-on").exists():
                group.rmdir()  # The volume is case-sensitive.
        process, run, child_pid = self.start_sleeping_run()
        if sys.platform == "darwin":
            mounts = subprocess.check_output(["/sbin/mount"], text=True).splitlines()
            source, = [line.partition(" on ")[0] for line in mounts
                       if line.partition(" on ")[2].rpartition(" (")[0] == str(run / "mount")]
            self.assertTrue(source.startswith("<") and source.endswith(">:/"), source)
            socket_dir = Path(source[1:-3]).parent
            self.assertTrue(socket_dir.is_dir(), socket_dir)
        process.kill()
        try: os.killpg(child_pid, signal.SIGKILL)
        except ProcessLookupError: pass
        process.communicate(timeout=10)
        self.assertFalse((run / ".state").exists())
        self.assertTrue((run / "backing").is_dir())
        if remove_socket:
            def restore_endpoint_for_cleanup():
                if run.exists() and socket_dir.parent.is_dir():
                    socket_dir.mkdir(mode=0o700, exist_ok=True)
            self.addCleanup(restore_endpoint_for_cleanup)
            (socket_dir / "s").unlink(missing_ok=True)
            socket_dir.rmdir()
            mounts = subprocess.check_output(["/sbin/mount"], text=True).splitlines()
            self.assertTrue(any(line.startswith(source + " on " + str(run / "mount") + " (")
                                for line in mounts))
        if sys.platform == "linux":
            moved = self.system_tmp.with_name(self.system_tmp.name + "-moved")
            self.system_tmp.rename(moved)
            try:
                recovered = moved / run.relative_to(self.system_tmp)
                self.assert_success(self.cleanup_run(recovered))
                self.assertFalse(recovered.exists())
            finally:
                moved.rename(self.system_tmp)
        else:
            self.assert_success(self.cleanup_run(run))
        if socket_dir is not None:
            self.assertFalse(socket_dir.exists(), socket_dir)
        self.assertFalse(run.exists())
        self.assert_no_runtime_files()

    @fuse_only
    def test_cleanup_reaps_delayed_unmount_with_inherited_sigchld_blocked(self):
        import shlex
        process, run, child_pid = self.start_sleeping_run()
        process.kill()
        try: os.killpg(child_pid, signal.SIGKILL)
        except ProcessLookupError: pass
        process.communicate(timeout=10)
        programs=self.root/'cleanup-programs'; programs.mkdir()
        unmounted=self.root/'unmounted'
        wrapper=programs/'fusermount3'
        wrapper.write_text('#!/bin/sh\n'+shlex.quote(shutil.which('fusermount3'))+' "$@" || exit $?\n'
                           'printf done > '+shlex.quote(str(unmounted))+'\nexec /bin/sleep 0.2\n')
        wrapper.chmod(0o700)
        env=dict(self.environment(),PATH=str(programs)+os.pathsep+os.environ['PATH'])
        cleaner=subprocess.Popen([str(BINARY),'--cleanup',str(run)],cwd=self.repo,env=env,
                                 preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_BLOCK,{signal.SIGCHLD}),
                                 stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            try:
                _,stderr=cleaner.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                self.assertTrue(unmounted.exists(),'helper never completed real unmount')
                self.fail('cleanup hung after successful unmount with SIGCHLD blocked')
            self.assertEqual(cleaner.returncode,0,stderr)
            self.assertTrue(unmounted.exists())
            self.assert_no_runtime_files()
        finally:
            if cleaner.poll() is None: cleaner.kill()
            cleaner.communicate(timeout=5)
            if run.exists(): self.assert_success(self.cleanup_run(run))

    def test_concurrent_cleanup_removes_only_stale_run(self):
        process, run, child_pid = self.start_sleeping_run()
        process.kill()
        try: os.killpg(child_pid, signal.SIGKILL)
        except ProcessLookupError: pass
        process.communicate(timeout=10)
        sibling = self.system_tmp / "unrelated-directory"
        sibling.mkdir()
        (sibling / "sentinel").write_text("preserve")
        results = self.race_cleanup(run)
        self.assertTrue(any(cleaner.returncode == 0 for cleaner, _ in results), results)
        self.assertFalse(run.exists())
        self.assertEqual((sibling / "sentinel").read_text(), "preserve")
        self.assertEqual(list(self.system_tmp.iterdir()), [sibling])
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

    @unittest.skipUnless(sys.platform == "darwin", "requires a Darwin NFS mount")
    def test_cleanup_recovers_a_legacy_run_on_nfs(self):
        code = """
import subprocess, sys
from pathlib import Path

group = Path.cwd() / 'git-on'
group.mkdir(mode=0o700)
run = group / 'g12345'
run.mkdir(mode=0o700)
(run / '.git-on-lock-v2').touch(mode=0o600)
result = subprocess.run([sys.argv[1], '--cleanup', str(run)], capture_output=True, timeout=10)
assert result.returncode == 0, result.stderr
assert not group.exists(), list(group.iterdir())
"""
        result, _ = self.invoke_argv([
            "HEAD", "--write-policy=all", "--", sys.executable, "-I", "-S", "-c", code, str(BINARY),
        ])
        self.assert_success(result)
        self.assert_no_runtime_files()

    def test_only_tree_oid_is_injected_and_tmpdir_is_private_then_removed(self):
        group = self.system_tmp / "git-on"
        if sys.platform == "darwin":
            mixed_case = self.system_tmp / "Git-On"
            mixed_case.mkdir(mode=0o700)
            if group.exists():
                group = mixed_case
            else:
                mixed_case.rmdir()
        code = r"""
import os, subprocess, sys
from pathlib import Path
injected = {key: value for key, value in os.environ.items() if key.startswith('GIT_ON_')}
assert injected == {'GIT_ON_TREE_OID': os.environ['EXPECTED_TREE']}, injected
run = Path.cwd().parent
assert Path.cwd().name == 'mount'
assert run.parent == Path(os.environ['EXPECTED_TEMP_GROUP'])
assert run.stat().st_mode & 0o777 == 0o700
assert not (run / '.state').exists()
assert not (run / '.lock').exists()
record = run.parent / f'.git-on-run-{os.geteuid():x}-{run.name}'
assert record.stat().st_mode & 0o777 == 0o700
[marker] = list(record.glob('.git-on-lock-v2-*'))
assert marker.stat().st_size == 0
if sys.platform == 'darwin':
    mounts = subprocess.check_output(['/sbin/mount'], text=True).splitlines()
    source, = [line.partition(' on ')[0] for line in mounts
               if line.partition(' on ')[2].rpartition(' (')[0] == str(Path.cwd())]
    assert source.startswith('<') and source.endswith('>:/'), source
    socket = Path(source[1:-3])
    group = socket.parent.parent
    assert group.parent == Path('/private/tmp')
    assert group.name in ('git-on', f'.go{os.geteuid():x}')
    assert socket.parent.name.startswith('nfs-')
    assert socket.is_socket()
    print(socket.parent)
assert (run / 'backing').is_dir()
assert not (run / 'manifest.json').exists()
assert not (run / 'stdout.log').exists()
assert not (run / 'stderr.log').exists()
assert not (run / 'output').exists()
assert os.environ['UNRELATED_VARIABLE'] == 'inherited'
print(run)
"""
        result, run = self.python(code, extra_env={
            "EXPECTED_TREE": self.tree, "EXPECTED_TEMP_GROUP": str(group),
            "UNRELATED_VARIABLE": "inherited",
        })
        self.assert_success(result)
        self.assertIsNone(run)
        for directory in os.fsdecode(result.stdout).splitlines():
            self.assertFalse(Path(directory).exists(), directory)
        self.assertEqual(result.stderr, b"")
        self.assert_no_runtime_files()

    def test_obsolete_run_directory_env_does_not_select_storage(self):
        old_run = self.root / "old-run"
        old_run.mkdir()
        sentinel = old_run / "sentinel"
        sentinel.write_text("untouched")
        result, run = self.python("from pathlib import Path; print(Path.cwd().parent)",
                                  extra_env={"GIT_ON_RUN_DIR": str(old_run)})
        self.assert_success(result)
        self.assertIsNone(run)
        self.assertEqual(Path(os.fsdecode(result.stdout).strip()).parent, self.system_tmp / 'git-on')
        self.assertEqual(list(old_run.iterdir()), [sentinel])
        self.assertEqual(sentinel.read_text(), "untouched")
        self.assert_no_runtime_files()


@unittest.skipUnless(MOUNT_AVAILABLE and OPTIONS.uv, "pass --uv with a real mount for dependency integration")
class UvTests(MountedRepositoryTest):
    def test_locked_pep723_and_project_with_numba_cache_reuse(self):
        self.assertIsNotNone(shutil.which("uv"), "uv is required for --uv")
        for example in ("pep723", "uv-project"):
            with self.subTest(example=example):
                destination = self.repo / example
                shutil.copytree(ROOT / "examples" / example, destination, ignore=shutil.ignore_patterns(".venv", "__pycache__"))
                git(self.repo, "add", "--force", "--all")
                git(self.repo, "commit", "--quiet", "-m", f"add {example}")
                command = ["uv", "run", "--locked", "--script", "main.py"] if example == "pep723" else ["uv", "run", "--locked", "main.py"]
                result, run = self.invoke_argv([f"HEAD:{example}", "--", *command], timeout=300)
                self.assert_success(result)
                self.assertIn(b"cache-reused", result.stdout)
                self.assertIsNone(run)
                # uv creates its own lock files in inherited TMPDIR. Those are
                # ordinary child host side effects, not git-on execution state.
                self.assertTrue(all(p.name.startswith("uv-") and p.suffix == ".lock"
                                    for p in self.system_tmp.iterdir()))
                self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))


if __name__ == "__main__":
    if not BINARY.is_file():
        PARSER.error(f"binary not found: {BINARY}; run cargo build first")
    if OPTIONS.require_nfs and (OPTIONS.require_fuse or sys.platform != "darwin"):
        PARSER.error("select the Darwin NFS client on macOS or the Linux FUSE client")
    if OPTIONS.require_fuse and not FUSE_AVAILABLE:
        PARSER.error("real FUSE required: /dev/fuse and fusermount3/fusermount must be available")
    if OPTIONS.require_nfs_tmpdir and sys.platform != "linux":
        PARSER.error("NFS-backed TMPDIR checks require the Linux NFS client")
    unittest.main(argv=[sys.argv[0], *UNITTEST_ARGS], verbosity=2)
