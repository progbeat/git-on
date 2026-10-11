#!/usr/bin/env python3
"""Black-box tests against real Git repositories and a real FUSE mount.

Run: python3 tests/integration.py --binary target/debug/git-on --require-fuse --uv
No mount or filesystem operation is mocked. Omit --require-fuse only for local
non-FUSE development; skipped tests are explicitly reported by unittest.
"""
from __future__ import annotations

import argparse
import json
import os
import pty
import select
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
PARSER = argparse.ArgumentParser(description=__doc__)
PARSER.add_argument("--binary", type=Path, default=ROOT / "target/debug/git-on")
PARSER.add_argument("--require-fuse", action="store_true")
PARSER.add_argument("--uv", action="store_true", help="Also run network-backed uv examples")
OPTIONS, UNITTEST_ARGS = PARSER.parse_known_args()
BINARY = OPTIONS.binary.resolve()
FUSE_AVAILABLE = Path("/dev/fuse").exists() and bool(shutil.which("fusermount3") or shutil.which("fusermount"))


def git(repo, *args, **kwargs):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, **kwargs).stdout.decode().strip()


class RepositoryTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="git-on-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
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
        self.counter = 0

    def put(self, name, content):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def environment(self, run=None):
        env = os.environ.copy()
        env.pop("GIT_ON_RUN_DIR", None)
        env["XDG_DATA_HOME"] = str(self.root / "data")
        env["XDG_CACHE_HOME"] = str(self.root / "cache")
        env["GIT_CONFIG_GLOBAL"] = os.devnull
        env["GIT_CONFIG_NOSYSTEM"] = "1"
        if run is not None:
            env["GIT_ON_RUN_DIR"] = str(run)
        return env

    def invoke(self, *command, tree="HEAD", run=None, input=None, timeout=60, extra_env=None):
        if run is None:
            self.counter += 1
            run = self.root / f"run-{self.counter}"
        result = subprocess.run([str(BINARY), tree, *map(str, command)], cwd=self.repo,
                                env=dict(self.environment(run), **(extra_env or {})), input=input, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=timeout)
        return result, run

    def python(self, code, **kwargs):
        return self.invoke(sys.executable, "-c", code, **kwargs)

    def manifest(self, run):
        return json.loads((run / "manifest.json").read_text())

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))


class CliTests(RepositoryTest):
    def test_help_and_version(self):
        for arg in ("--help", "--version"):
            result = subprocess.run([str(BINARY), arg], capture_output=True, timeout=10)
            self.assert_success(result)
            self.assertTrue(b"git-on" in result.stdout or b"git on" in result.stdout)

    def test_unknown_tree_never_executes(self):
        sentinel = self.root / "executed"
        result, _ = self.python(f"open({str(sentinel)!r}, 'w').close()", tree="missing-tree-ish")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(sentinel.exists())

    def test_existing_run_directory_rejected_without_touching_it(self):
        run = self.root / "existing"
        run.mkdir()
        (run / "sentinel").write_text("keep")
        result, _ = self.python("raise SystemExit(0)", run=run)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(run.iterdir()), [run / "sentinel"])
        self.assertEqual((run / "sentinel").read_text(), "keep")


@unittest.skipUnless(FUSE_AVAILABLE, "real FUSE is unavailable (use --require-fuse in CI)")
class FuseTests(RepositoryTest):
    def test_snapshot_ignores_dirty_index_worktree_and_untracked_files(self):
        self.put("tracked.txt", "staged\n")
        git(self.repo, "add", "tracked.txt")
        self.put("tracked.txt", "dirty\n")
        self.put("untracked.txt", "not in snapshot")
        result, run = self.python("from pathlib import Path; assert Path('tracked.txt').read_text() == 'committed\\n'; assert not Path('untracked.txt').exists(); assert not Path('.git').exists(); print('snapshot')")
        self.assert_success(result)
        self.assertEqual(self.manifest(run)["tree_oid"], self.tree)
        self.assertEqual((self.repo / "tracked.txt").read_text(), "dirty\n")

    def test_executable_mode_symlinks_and_repository_discovery(self):
        self.put("executable.sh", "#!/bin/sh\nprintf 'executable\\n'\n")
        (self.repo / "executable.sh").chmod(0o755)
        git(self.repo, "add", "executable.sh")
        git(self.repo, "commit", "--quiet", "-m", "executable fixture")
        run = self.root / "discovered-run"
        code = "from pathlib import Path; import os,subprocess; assert os.access('executable.sh',os.X_OK); assert Path('tracked-link').is_symlink(); assert Path('tracked-link').read_text() == 'committed\\n'; subprocess.run(['./executable.sh'],check=True)"
        result = subprocess.run([str(BINARY), "HEAD", sys.executable, "-c", code], cwd=self.repo / "nested", env=self.environment(run), capture_output=True, timeout=30)
        self.assert_success(result)
        self.assertEqual(result.stdout, b"executable\n")

    def test_raw_tree_and_subtree_root(self):
        for tree in (self.tree, "HEAD:subtree"):
            source = "tracked.txt" if tree == self.tree else "source.txt"
            result, run = self.python(f"from pathlib import Path; assert Path({source!r}).is_file(); print('ok')", tree=tree)
            self.assert_success(result)
            self.assertEqual(self.manifest(run)["tree_oid"], git(self.repo, "rev-parse", tree))
        result, _ = self.python("from pathlib import Path; Path('generated').mkdir(); Path('generated/result').write_text('ok')", tree="HEAD:subtree")
        self.assert_success(result)

    def test_no_shell_inherited_stdin_and_binary_logs(self):
        code = "import os,sys; assert sys.argv[1:] == ['$(touch nope)', 'a b', '; exit 5']; assert sys.stdin.buffer.read() == b'input\\x00bytes'; os.write(1,b'out\\x00\\xff\\n'); os.write(2,b'err\\x00\\xfe\\n')"
        argv = [sys.executable, "-c", code, "$(touch nope)", "a b", "; exit 5"]
        result, run = self.invoke(*argv, input=b"input\x00bytes")
        self.assert_success(result)
        self.assertEqual(result.stdout, b"out\x00\xff\n")
        self.assertIn(b"err\x00\xfe\n", result.stderr)
        self.assertEqual((run / "stdout.log").read_bytes(), b"out\x00\xff\n")
        self.assertEqual((run / "stderr.log").read_bytes(), b"err\x00\xfe\n")
        self.assertEqual(self.manifest(run)["argv"], argv)

    def test_large_stdout_and_stderr_do_not_deadlock(self):
        result, run = self.python("import os; [(os.write(1,b'x'*65536),os.write(2,b'y'*65536)) for _ in range(40)]")
        self.assert_success(result)
        self.assertEqual((run / "stdout.log").stat().st_size, 40 * 65536)
        self.assertEqual((run / "stderr.log").stat().st_size, 40 * 65536)

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
Path(os.environ['GIT_ON_OUTPUT_DIR'], 'result.txt').write_text(Path('out/new/result').read_text())
''')
        self.assert_success(result)
        self.assertEqual((run / "output/result.txt").read_text(), "first second")
        self.assertFalse((self.repo / "out/new").exists())

    def test_file_operation_matrix_preserves_tracked_bytes(self):
        self.put("protected.cache", "tracked cache\n")
        self.put("nested/protected.tmp", "tracked nested\n")
        git(self.repo, "add", "--force", "protected.cache", "nested/protected.tmp")
        git(self.repo, "commit", "--quiet", "-m", "tracked ignore precedence")
        result, _ = self.python(r'''
import errno, os, stat
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
        ('chown-self', lambda: os.chown(path, os.getuid(), os.getgid())),
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
import os
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
assert set(Path('out').iterdir()) == {Path('out/protected.txt'), Path('out/appends')}
Path(os.environ['GIT_ON_OUTPUT_DIR'], 'append-count').write_text(str(len(rows)))
''')
        self.assert_success(result)
        self.assertEqual((run / "output/append-count").read_text(), "160")
        self.assertFalse((run / "mount").exists())
        self.assertFalse((run / "backing").exists())
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))
        self.assertFalse((self.repo / "out/appends").exists())

    def test_lazy_metadata_does_not_load_unread_blobs_or_descendant_trees(self):
        from benchmark import digit_fixture, snapshot_metrics
        fixture = digit_fixture(self.repo, 10000)
        tree = fixture["tree_oid"]
        # Isolated Python suppresses CWD module searching and site startup.
        workloads = [
            ("pass", 0, 0, 0, 0),
            ("import os; assert os.listdir('.') == ['0']", 1, 0, 0, 0),
            ("import os; assert os.stat('0/0/6/4/2/1').st_size == 7", 6, 1, 0, 0),
            ("import os; assert not os.path.exists('0/0/6/4/2/missing')", 6, 0, 0, 0),
            ("from pathlib import Path; assert Path('0/0/6/4/2/1').read_bytes() == b'006421\\n'", 6, 1, 1, 7),
        ]
        for code, trees, headers, blobs, blob_bytes in workloads:
            with self.subTest(code=code):
                result, run = self.invoke(sys.executable, "-I", "-S", "-c", code, tree=tree)
                self.assert_success(result)
                metrics = snapshot_metrics(self.manifest(run))
                self.assertLessEqual(metrics["tree_reads"], trees, metrics)
                self.assertLessEqual(metrics["header_reads"], headers, metrics)
                self.assertEqual(metrics["blob_loads"], blobs, metrics)
                self.assertEqual(metrics["blob_bytes"], blob_bytes, metrics)

    def test_lazy_writes_load_only_relevant_ancestor_ignore_files(self):
        from benchmark import snapshot_metrics
        for index in range(12):
            self.put(f"untouched-{index}/.gitignore", f"unused-{index}/\n")
            self.put(f"untouched-{index}/payload", "unread sibling content\n")
        git(self.repo, "add", "--force", "--all")
        git(self.repo, "commit", "--quiet", "-m", "untouched policy subtrees")
        expected_rule_bytes = sum((self.repo / path).stat().st_size
                                  for path in [".gitignore", "nested/.gitignore"])
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
        metrics = snapshot_metrics(self.manifest(run))
        self.assertLessEqual(metrics["tree_reads"], 2, metrics)
        self.assertLessEqual(metrics["header_reads"], 2, metrics)
        self.assertEqual(metrics["blob_loads"], 2, metrics)
        self.assertEqual(metrics["blob_bytes"], expected_rule_bytes, metrics)

    def test_repeated_snapshot_reads_reuse_bounded_blob_cache(self):
        from benchmark import snapshot_metrics
        # Distinct directory entries reference the same blob. Page-cache reuse
        # of one pathname alone cannot satisfy this object-cache assertion.
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
        metrics = snapshot_metrics(self.manifest(run))
        self.assertEqual(metrics["blob_loads"], 1, metrics)
        self.assertEqual(metrics["blob_bytes"], len(b"shared payload\n"), metrics)
        self.assertGreaterEqual(metrics["cache_hits"], 15, metrics)
        self.assertLessEqual(metrics["tree_reads"], 2, metrics)

    def test_large_snapshot_blob_reads_decode_once_and_respect_cache_bound(self):
        import hashlib
        from benchmark import snapshot_metrics
        payload = bytes(range(256)) * (16 * 1024 * 1024 // 256)
        (self.repo / "large.bin").write_bytes(payload)
        expected = hashlib.sha256(payload).hexdigest()
        git(self.repo, "add", "large.bin")
        git(self.repo, "commit", "--quiet", "-m", "large immutable read fixture")
        spool_run = self.root / "large-spool-run"
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
''', expected, run=spool_run, extra_env={'TMPDIR': str(spool_run / 'mount/cache')})
        self.assert_success(result)
        metrics = snapshot_metrics(self.manifest(run))
        self.assertEqual(metrics["blob_loads"], 1, metrics)
        self.assertEqual(metrics["blob_bytes"], len(payload), metrics)
        self.assertLessEqual(metrics["tree_reads"], 1, metrics)
        self.assertLessEqual(metrics["header_reads"], 1, metrics)

    def test_unsupported_entries_fail_when_accessed_without_poisoning_siblings(self):
        pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:" + "a" * 64 + "\nsize 123\n"
        self.put("unsupported/lfs.bin", pointer)
        self.put("unsupported/ordinary.txt", "ordinary\n")
        git(self.repo, "add", "unsupported")
        head = git(self.repo, "rev-parse", "HEAD")
        git(self.repo, "update-index", "--add", "--cacheinfo", f"160000,{head},unsupported/module")
        git(self.repo, "commit", "--quiet", "-m", "unsupported objects accessed lazily")
        result, run = self.invoke(sys.executable, "-I", "-S", "-c", r'''
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
''')
        self.assert_success(result)
        self.assertFalse((run / "mount").exists())
        self.assertFalse((run / "backing").exists())
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

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

    def test_process_exit_and_signal_recorded(self):
        result, run = self.python("raise SystemExit(37)")
        self.assertEqual(result.returncode, 37)
        self.assertEqual(self.manifest(run)["status"]["exit_code"], 37)
        result, run = self.python("import os,signal; os.kill(os.getpid(), signal.SIGTERM)")
        self.assertIn(result.returncode, (-signal.SIGTERM, 128 + signal.SIGTERM))
        self.assertEqual(self.manifest(run)["status"]["signal"], signal.SIGTERM)

    def test_missing_executable_records_runner_error(self):
        result, run = self.invoke("git-on-certainly-nonexistent-executable")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.manifest(run)["status"]["runner_error"])

    def test_simultaneous_runs_are_isolated(self):
        processes = []
        for index in range(2):
            run = self.root / f"parallel-{index}"
            code = f"from pathlib import Path; import time,os; Path('out/shared').write_text('{index}'); time.sleep(.5); assert Path('out/shared').read_text() == '{index}'; Path(os.environ['GIT_ON_OUTPUT_DIR'], 'result').write_text('{index}')"
            processes.append((subprocess.Popen([str(BINARY), "HEAD", sys.executable, "-c", code], cwd=self.repo, env=self.environment(run), stdout=subprocess.PIPE, stderr=subprocess.PIPE), run, index))
        for process, run, index in processes:
            _, stderr = process.communicate(timeout=30)
            self.assertEqual(process.returncode, 0, stderr)
            self.assertEqual((run / "output/result").read_text(), str(index))

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

    def test_interactive_pty_stdin_has_foreground_process_group(self):
        run = self.root / "pty-run"
        code = "import os,sys; assert os.isatty(0); assert os.tcgetpgrp(0) == os.getpgrp(); print('input-ready',flush=True); assert sys.stdin.readline() == 'terminal-input\\n'; print('terminal-ok',flush=True)"
        pid, fd = pty.fork()
        if pid == 0:
            import termios
            tty_settings = termios.tcgetattr(0)
            tty_settings[3] |= termios.TOSTOP
            termios.tcsetattr(0, termios.TCSANOW, tty_settings)
            os.chdir(self.repo)
            env = self.environment(run)
            env["PATH"] = str(BINARY.parent) + os.pathsep + env.get("PATH", "")
            os.execvpe("git", ["git", "on", "HEAD", sys.executable, "-c", code], env)
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
        run = self.root / "job-control-run"
        code = "import os,signal; print('stop-ready',flush=True); os.kill(os.getpid(),signal.SIGSTOP); assert os.tcgetpgrp(0)==os.getpgrp(); print('resumed-ok',flush=True)"
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.repo)
            env = self.environment(run)
            env["PATH"] = str(BINARY.parent) + os.pathsep + env.get("PATH", "")
            os.execvpe("git", ["git", "on", "HEAD", sys.executable, "-c", code], env)
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

    def test_escaped_descendant_keeps_busy_mount_for_explicit_cleanup(self):
        pidfile = self.root / "escaped-pid"
        descendant = f"import os,time; from pathlib import Path; Path({str(pidfile)!r}).write_text(str(os.getpid())); time.sleep(300)"
        code = "import os,subprocess,sys,time; from pathlib import Path; subprocess.Popen([sys.executable,'-c',sys.argv[1]],preexec_fn=os.setsid); p=Path(sys.argv[2]); deadline=time.monotonic()+5\nwhile not p.exists() and time.monotonic()<deadline: time.sleep(.01)\nassert p.exists()"
        run = self.root / "escaped-run"
        try:
            result, _ = self.invoke(sys.executable, "-c", code, descendant, str(pidfile), run=run, timeout=15)
            self.assertEqual(result.returncode, 125, result.stderr)
            self.assertTrue(self.manifest(run)["status"]["runner_error"])
            # A disconnected FUSE mount returns ENOTCONN to stat(), so
            # os.path.ismount misleadingly reports false. Inspect mountinfo.
            mountpoints = [line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()]
            self.assertIn(str(run / "mount"), mountpoints, "busy mount must remain for recovery")
            self.assertTrue((run / "backing").is_dir())
            self.assertTrue(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))
        finally:
            if pidfile.exists():
                try: os.killpg(int(pidfile.read_text()), signal.SIGKILL)
                except ProcessLookupError: pass
            cleanup = subprocess.run([str(BINARY), "--cleanup", str(run)], capture_output=True, timeout=15)
            self.assert_success(cleanup)
        self.assertFalse((run / "mount").exists())
        self.assertFalse((run / "backing").exists())
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

    def test_non_utf8_command_argument_is_preserved(self):
        raw = b"bad-\xff-name"
        code = "import os,sys; assert os.fsencode(sys.argv[1]) == bytes([98,97,100,45,255,45,110,97,109,101])"
        run = self.root / "raw-argv"
        result = subprocess.run([os.fsencode(BINARY), b"HEAD", os.fsencode(sys.executable), b"-c", code.encode(), raw], cwd=self.repo, env=self.environment(run), capture_output=True, timeout=30)
        self.assert_success(result)
        self.assertEqual(self.manifest(run)["argv_hex"][-1], raw.hex())

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
        run = self.root / "sleeping-run"
        ready = self.root / "child-pid"
        code = f"import os,time; from pathlib import Path; Path({str(ready)!r}).write_text(str(os.getpid())); Path(os.environ['GIT_ON_OUTPUT_DIR'], 'keep').write_text('artifact'); time.sleep(300)"
        process = subprocess.Popen([str(BINARY), "HEAD", sys.executable, "-c", code], cwd=self.repo, env=self.environment(run), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 20
        while not ready.exists() and time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.02)
        if not ready.exists():
            process.kill()
            _, stderr = process.communicate(timeout=5)
            self.fail(f"child did not start: {stderr!r}")
        child_pid = int(ready.read_text())
        def stop():
            try: os.killpg(child_pid, signal.SIGKILL)
            except ProcessLookupError: pass
            if process.poll() is None: process.kill()
            process.communicate(timeout=5)
        self.addCleanup(stop)
        return process, run, child_pid

    def test_forwarded_signal_and_cleanup_preserve_evidence(self):
        process, run, _ = self.start_sleeping_run()
        active_cleanup = subprocess.run([str(BINARY), "--cleanup", str(run)], capture_output=True, timeout=10)
        self.assertNotEqual(active_cleanup.returncode, 0)
        process.send_signal(signal.SIGTERM)
        process.communicate(timeout=15)
        self.assertEqual(process.returncode, 128 + signal.SIGTERM)
        manifest = self.manifest(run)
        self.assertEqual(manifest["status"]["signal"], signal.SIGTERM)
        self.assertEqual(manifest["phase"], "finished")
        self.assertTrue(manifest["logs_complete"])
        for _ in range(2):
            cleanup = subprocess.run([str(BINARY), "--cleanup", str(run)], capture_output=True, timeout=10)
            self.assert_success(cleanup)
            self.assertEqual((run / "output/keep").read_text(), "artifact")
            self.assertEqual(self.manifest(run), manifest)
            self.assertTrue((run / "stdout.log").exists())
            self.assertTrue((run / "stderr.log").exists())
            self.assertFalse((run / "mount").exists())
            self.assertFalse((run / "backing").exists())
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

    def test_sigkill_recovery_cleanup(self):
        process, run, child_pid = self.start_sleeping_run()
        process.kill()
        try: os.killpg(child_pid, signal.SIGKILL)
        except ProcessLookupError: pass
        process.communicate(timeout=10)
        cleanup = subprocess.run([str(BINARY), "--cleanup", str(run)], capture_output=True, timeout=15)
        self.assert_success(cleanup)
        self.assertEqual((run / "output/keep").read_text(), "artifact")
        self.assertTrue((run / "manifest.json").exists())
        self.assertFalse((run / "mount").exists())
        self.assertFalse((run / "backing").exists())
        self.assertFalse(git(self.repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"))

    def test_default_xdg_run_directory(self):
        result = subprocess.run([str(BINARY), "HEAD", sys.executable, "-c", "print('xdg')"], cwd=self.repo, env=self.environment(), capture_output=True, timeout=30)
        self.assert_success(result)
        manifests = list((self.root / "data").rglob("manifest.json"))
        self.assertEqual(len(manifests), 1)
        self.assertEqual(json.loads(manifests[0].read_text())["tree_oid"], self.tree)


@unittest.skipUnless(FUSE_AVAILABLE and OPTIONS.uv, "pass --uv with real FUSE for dependency integration")
class UvTests(RepositoryTest):
    def test_locked_pep723_and_project_with_numba_cache_reuse(self):
        self.assertIsNotNone(shutil.which("uv"), "uv is required for --uv")
        for example in ("pep723", "uv-project"):
            with self.subTest(example=example):
                destination = self.repo / example
                shutil.copytree(ROOT / "examples" / example, destination, ignore=shutil.ignore_patterns(".venv", "__pycache__"))
                git(self.repo, "add", "--force", "--all")
                git(self.repo, "commit", "--quiet", "-m", f"add {example}")
                command = ["uv", "run", "--locked", "--script", "main.py"] if example == "pep723" else ["uv", "run", "--locked", "main.py"]
                result, run = self.invoke(*command, tree=f"HEAD:{example}", timeout=300)
                self.assert_success(result)
                self.assertIn(b"cache-reused", result.stdout)
                self.assertTrue(list((run / "output").rglob("*.nbc")), "Numba compiled cache must be retained")
                self.assertTrue(list((run / "output").rglob("*.pyc")), "Local import bytecode must be retained")


if __name__ == "__main__":
    if not BINARY.is_file():
        PARSER.error(f"binary not found: {BINARY}; run cargo build first")
    if OPTIONS.require_fuse and not FUSE_AVAILABLE:
        PARSER.error("real FUSE required: /dev/fuse and fusermount3/fusermount must be available")
    unittest.main(argv=[sys.argv[0], *UNITTEST_ARGS], verbosity=2)
