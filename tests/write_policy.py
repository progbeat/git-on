#!/usr/bin/env python3
"""Write-policy acceptance against actual FUSE, sharing the integration fixture.

python3 tests/write_policy.py --binary target/debug/git-on --require-fuse
"""
import errno
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import unittest

from integration import BINARY, FUSE_AVAILABLE, OPTIONS, PARSER, RepositoryTest, git


@unittest.skipUnless(FUSE_AVAILABLE, "real FUSE device and fusermount3 required")
class WritePolicyTests(RepositoryTest):
    def policy(self, policy, code):
        return self.invoke_argv([
            "HEAD", "--write-policy=" + policy, *(["--keep"] if keep else []),
            "--", sys.executable, "-c", code,
        ])

    def assert_source_unchanged(self):
        self.assertEqual(git(self.repo, "rev-parse", "HEAD^{tree}"), self.tree)
        self.assertEqual(git(self.repo, "status", "--porcelain", "--ignored"), "")
        self.assertEqual((self.repo / "tracked.txt").read_text(), "committed\n")
        self.assertEqual(os.readlink(self.repo / "tracked-link"), "tracked.txt")

    def test_none_reads_but_denies_mutations_even_in_ignored_paths(self):
        result, run = self.policy("none", r'''
import errno, os
from pathlib import Path
assert Path('tracked.txt').read_text() == 'committed\n'
assert Path('tracked-link').read_text() == 'committed\n'
assert 'protected.txt' in os.listdir('out')
operations = [
    lambda: Path('out/new').write_text('new'),
    lambda: Path('tracked.txt').write_text('changed'),
    lambda: Path('tracked.txt').unlink(),
    lambda: os.rename('tracked.txt', 'out/moved'),
    lambda: os.mkdir('out/directory'),
    lambda: os.symlink('tracked.txt', 'out/link'),
    lambda: os.link('tracked.txt', 'out/hardlink'),
    lambda: os.chmod('tracked.txt', 0o600),
    lambda: os.utime('tracked.txt', (1, 1)),
]
for operation in operations:
    try:
        operation()
    except OSError as error:
        assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM), error
    else:
        raise AssertionError('read-only mutation succeeded')
assert Path('tracked.txt').read_text() == 'committed\n'
assert sorted(os.listdir('out')) == ['protected.txt']
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_source_unchanged()
        self.assert_no_runtime_files()

    def test_explicit_ignored_matches_default_and_pinned_nested_rules(self):
        code = r'''
import errno, os
from pathlib import Path
Path('out/new').write_text('new')
os.rename('out/new', 'out/renamed')
assert Path('out/renamed').read_text() == 'new'
os.mkdir('out/directory')
os.symlink('../tracked.txt', 'out/link')
assert Path('out/link').read_text() == 'committed\n'
os.chmod('out/renamed', 0o640)
os.utime('out/renamed', (1234, 1234))
Path('nested/allowed.tmp').write_text('nested')
Path('out/.gitignore').write_text('!renamed\n')
Path('out/renamed').write_text('still allowed by pinned rules')
for path in ('not-ignored', 'keep.cache', 'nested/keep.tmp', 'out/protected.txt'):
    try:
        Path(path).write_text('forbidden')
    except OSError as error:
        assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM), error
    else:
        raise AssertionError(path)
assert Path('out/protected.txt').read_text() == 'tracked despite ignore\n'
Path('out/renamed').unlink()
os.rmdir('out/directory')
'''
        for policy in (None, "ignored"):
            with self.subTest(policy=policy):
                result, run = self.python(code) if policy is None else self.policy(policy, code)
                self.assert_success(result)
                self.assertIsNone(run)
                self.assert_source_unchanged()
                self.assert_no_runtime_files()

    def test_all_mutates_tracked_and_untracked_namespace_without_source_changes(self):
        result, run = self.policy("all", r'''
import os, stat
from pathlib import Path
Path('tracked.txt').write_text('changed')
with open('tracked.txt', 'a') as file:
    file.write(' + append')
assert Path('tracked-link').read_text() == 'changed + append'
os.chmod('tracked.txt', 0o640)
os.utime('tracked.txt', ns=(1234567890000000000, 1234567890000000000))
assert stat.S_IMODE(os.stat('tracked.txt').st_mode) == 0o640
assert os.stat('tracked.txt').st_mtime_ns == 1234567890000000000
Path('new').write_text('unignored')
os.rename('new', 'replacement')
os.replace('replacement', 'tracked.txt')
assert Path('tracked.txt').read_text() == 'unignored'
os.mkdir('directory')
Path('directory/value').write_text('payload')
os.symlink('value', 'directory/link')
assert Path('directory/link').read_text() == 'payload'
os.link('directory/value', 'directory/hardlink')
Path('directory/hardlink').write_text('shared')
assert Path('directory/value').read_text() == 'shared'
os.rename('directory', 'moved')
assert Path('moved/link').read_text() == 'shared'
os.rename('nested', 'renamed-source')
assert not Path('nested').exists()
assert Path('renamed-source/module.py').read_text() == 'VALUE = 42\n'
Path('renamed-source/module.py').write_text('VALUE = 99\n')
Path('renamed-source/new').write_text('new nested')
Path('tracked-link').unlink()
os.symlink('moved/value', 'tracked-link')
assert Path('tracked-link').read_text() == 'shared'
Path('out/protected.txt').unlink()
assert not Path('out/protected.txt').exists()
assert os.listdir('out') == []
os.rmdir('out')
os.mkdir('out')
assert os.listdir('out') == []
Path('plain/source.py').unlink()
os.rmdir('plain')
assert not Path('plain').exists()
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_source_unchanged()
        self.assert_no_runtime_files()

    def test_all_existing_open_handles_follow_copyup_and_rename(self):
        result, run = self.policy("all", r'''
import os
from pathlib import Path
reader = open('tracked.txt', 'rb', buffering=0)
writer = open('tracked.txt', 'r+b', buffering=0)
writer.write(b'CHANGED!!\n')
reader.seek(0)
assert reader.read() == b'CHANGED!!\n'
os.rename('tracked.txt', 'renamed')
writer.seek(0)
writer.write(b'RENAMED!!\n')
assert Path('renamed').read_bytes() == b'RENAMED!!\n'
os.unlink('renamed')
reader.seek(0)
assert reader.read() == b'RENAMED!!\n'
writer.close()
reader.close()
assert not Path('tracked.txt').exists()
assert not Path('renamed').exists()
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_source_unchanged()
        self.assert_no_runtime_files()

    def test_untracked_allows_new_paths_and_protects_source_and_parent_directories(self):
        result, run = self.policy("untracked", r'''
import errno, os
from pathlib import Path
for path in ('out/new', 'not-ignored', 'keep.cache', 'nested/keep.tmp'):
    Path(path).write_text('new')
    os.rename(path, path + '.renamed')
    assert Path(path + '.renamed').read_text() == 'new'
    Path(path + '.renamed').write_text('edited')
    Path(path + '.renamed').unlink()
os.mkdir('new-dir')
Path('new-dir/value').write_text('new child')
os.symlink('value', 'new-dir/link')
os.rename('new-dir', 'new-renamed')
assert Path('new-renamed/link').read_text() == 'new child'
Path('new-renamed/link').unlink()
Path('new-renamed/value').unlink()
os.rmdir('new-renamed')
os.mkdir('replacement')
operations = [
    lambda: Path('tracked.txt').write_text('changed'),
    lambda: Path('out/protected.txt').unlink(),
    lambda: os.rename('tracked.txt', 'new-tracked-name'),
    lambda: os.rename('nested', 'moved-source-directory'),
    lambda: os.replace('replacement', 'nested'),
    lambda: os.rmdir('nested'),
    lambda: os.chmod('nested', 0o700),
    lambda: os.symlink('missing', 'tracked-link'),
    lambda: os.link('tracked.txt', 'hardlink-to-source'),
]
for operation in operations:
    try:
        operation()
    except OSError as error:
        assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM, errno.EEXIST), error
    else:
        raise AssertionError('tracked protection bypassed')
assert Path('tracked.txt').read_text() == 'committed\n'
assert Path('nested/module.py').read_text() == 'VALUE = 42\n'
assert Path('out/protected.txt').read_text() == 'tracked despite ignore\n'
assert Path('replacement').is_dir()
os.rmdir('replacement')
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_source_unchanged()
        self.assert_no_runtime_files()

    def test_untracked_uses_selected_commit_instead_of_index_or_worktree(self):
        self.put("tracked.txt", "dirty host contents")
        self.put("staged.txt", "staged host contents")
        git(self.repo, "add", "staged.txt")
        self.put("out/host-ignored", "host artifact")
        before = git(self.repo, "status", "--porcelain", "--ignored")
        result, run = self.policy("untracked", r'''
from pathlib import Path
assert Path('tracked.txt').read_text() == 'committed\n'
assert not Path('staged.txt').exists()
assert not Path('out/host-ignored').exists()
Path('staged.txt').write_text('temporary selected-tree untracked file')
Path('out/host-ignored').write_text('temporary ignored file')
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assertEqual(git(self.repo, "status", "--porcelain", "--ignored"), before)
        self.assertEqual((self.repo / "staged.txt").read_text(), "staged host contents")
        self.assertEqual((self.repo / "tracked.txt").read_text(), "dirty host contents")
        self.assertEqual((self.repo / "out/host-ignored").read_text(), "host artifact")
        self.assertEqual(git(self.repo, "rev-parse", "HEAD^{tree}"), self.tree)
        self.assert_no_runtime_files()

    def test_all_open_unlinked_source_allows_descriptor_metadata_changes(self):
        result, run = self.policy("all", r'''
import os, stat
reader = open('tracked.txt', 'rb', buffering=0)
os.unlink('tracked.txt')
os.fchmod(reader.fileno(), 0o600)
assert stat.S_IMODE(os.fstat(reader.fileno()).st_mode) == 0o600
assert reader.read() == b'committed\n'
reader.close()
''')
        self.assert_success(result)
        self.assertIsNone(run)
        self.assert_source_unchanged()
        self.assert_no_runtime_files()


    def test_interrupt_cleans_each_policy_and_preserves_the_source(self):
        for policy in ("none", "ignored", "untracked", "all"):
            with self.subTest(policy=policy):
                ready = self.root / "ready"
                ready.unlink(missing_ok=True)
                code = "from pathlib import Path; import time; "
                if policy != "none":
                    code += "Path('out/interrupted').write_text('temporary'); "
                if policy == "all":
                    code += "Path('tracked.txt').write_text('temporary'); "
                code += f"Path({str(ready)!r}).touch(); time.sleep(60)"
                child = subprocess.Popen([
                    str(BINARY), "HEAD", "--write-policy=" + policy,
                    "--", sys.executable, "-c", code,
                ], cwd=self.repo, env=self.environment(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    deadline = time.monotonic() + 15
                    while not ready.exists() and child.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.02)
                    self.assertTrue(ready.exists(), "command never became ready")
                    child.send_signal(signal.SIGINT)
                    _, stderr = child.communicate(timeout=15)
                    self.assertEqual(child.returncode, -signal.SIGINT, stderr)
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.communicate(timeout=15)
                self.assert_source_unchanged()
                self.assert_no_runtime_files()


if __name__ == "__main__":
    if not BINARY.is_file():
        PARSER.error(f"binary not found: {BINARY}; run cargo build first")
    if OPTIONS.require_fuse and not FUSE_AVAILABLE:
        PARSER.error("real FUSE is required")
    # integration.py parsed runner arguments before unittest sees test selectors.
    from integration import UNITTEST_ARGS
    unittest.main(argv=[sys.argv[0], *UNITTEST_ARGS], verbosity=2)
