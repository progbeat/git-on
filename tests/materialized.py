#!/usr/bin/env python3
"""Actual no-FUSE executions; run only on a host where FUSE is unavailable.
These do not count as real FUSE/macFUSE verification.
"""
import os
import json
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

BINARY = Path(sys.argv.pop(1)).resolve()


class MaterializedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.tmp = self.root / 'tmp'
        self.tmp.mkdir()
        programs = self.root / 'programs'
        programs.mkdir()
        (programs / 'git').symlink_to(shutil.which('git'))
        self.env = dict(os.environ, PATH=str(programs), TMPDIR=str(self.tmp), GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1', GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid', GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
        self.git('init', '-q')
        (self.repo / 'tracked').write_bytes(b'committed\x00\xff\r\n')
        (self.repo / 'tool').write_text('#!/bin/sh\nprintf executable\n')
        (self.repo / 'tool').chmod(0o755)
        (self.repo / '.gitattributes').write_text('* text eol=lf\n')
        os.symlink('tracked', self.repo / 'link')
        self.git('add', '.')
        # Put deliberately unfiltered bytes in Git despite text/eol attributes.
        blob = self.git('hash-object', '-w', '--stdin', data=b'committed\x00\xff\r\n').strip().decode()
        self.git('update-index', '--cacheinfo', '100644,' + blob + ',tracked')
        self.git('commit', '-qm', 'fixture')
        self.oid = self.git('rev-parse', 'HEAD^{tree}').strip()
        (self.repo / 'tracked').write_bytes(b'user edits')
        self.before = self.git('status', '--porcelain')

    def git(self, *args, data=None):
        return subprocess.check_output(['git', *args], cwd=self.repo, env=self.env, input=data)

    def invoke(self, code, *options):
        result = subprocess.run([str(BINARY), 'HEAD', *options, '--', sys.executable, '-c', code, '--keep', '-k', '', 'a b'], cwd=self.repo, env=self.env, capture_output=True, timeout=20)
        return result

    def clean(self):
        self.assertEqual(list(self.tmp.iterdir()), [])
        self.assertEqual(self.git('status', '--porcelain'), self.before)
        self.assertEqual(self.git('for-each-ref', '--format=%(refname)', 'refs/git-on/'), b'')

    def test_default_and_protected_policies_are_never_relaxed(self):
        for options in [[], ['--write-policy=none'], ['--write-policy=ignored'], ['--write-policy=untracked']]:
            result = self.invoke('raise Exception("child must not start")', *options)
            self.assertEqual(result.returncode, 125, result.stderr)
            self.assertIn(b'FUSE unavailable', result.stderr)
            self.clean()

    def test_bytes_links_modes_argv_identity_freshness_and_source_isolation(self):
        for _ in range(2):
            result = self.invoke("""
import os,sys,subprocess
from pathlib import Path
assert sys.argv[1:] == ['--keep','-k','','a b']
assert Path('tracked').read_bytes() == b'committed\\x00\\xff\\r\\n'
assert Path('link').read_bytes() == Path('tracked').read_bytes()
assert os.readlink('link') == 'tracked'
assert Path('tool').stat().st_mode & 0o111 == 0o111
assert subprocess.check_output(['./tool']) == b'executable'
assert not Path('.git').exists()
assert not Path('generated').exists()
Path('tracked').write_text('changed')
Path('generated').write_text('fresh')
print(os.environ['GIT_ON_TREE_OID'])
""", '--write-policy=all')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(b'materialized backend', result.stderr)
            self.assertEqual(result.stdout.strip(), self.oid)
            self.clean()

    def test_command_status_and_signal_cleanup(self):
        result = self.invoke('import sys; sys.exit(23)', '--write-policy=all')
        self.assertEqual(result.returncode, 23, result.stderr)
        self.clean()
        result = self.invoke('import os,signal; os.kill(os.getpid(),signal.SIGTERM)', '--write-policy=all')
        self.assertEqual(result.returncode, -signal.SIGTERM, result.stderr)
        self.clean()

    def outside_directory(self):
        outside=self.root/'outside';outside.mkdir()
        (outside/'preserve').write_text('external data')
        outside.chmod(0o555)
        self.addCleanup(outside.chmod,0o755)
        return outside

    def assert_outside_untouched(self,outside):
        self.assertEqual(outside.stat().st_mode & 0o777,0o555)
        self.assertEqual((outside/'preserve').read_text(),'external data')

    def test_normal_cleanup_restores_locked_directories_without_following_links(self):
        outside=self.outside_directory()
        result=self.invoke("""
import os,sys
from pathlib import Path
Path('d/nested').mkdir(parents=True)
Path('d/nested/output').write_text('disposable')
os.symlink(sys.argv[-1],'external')
os.chmod('d/nested',0)
os.chmod('d',0o555)
os.chmod('.',0)
""".replace('sys.argv[-1]',repr(str(outside))),'--write-policy=all')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assert_outside_untouched(outside)
        self.clean()

    def test_explicit_recovery_restores_locked_directories_without_following_links(self):
        outside=self.outside_directory()
        run=self.tmp/'git-on-recovery';run.mkdir(mode=0o700)
        (run/'.lock').touch(mode=0o600)
        (run/'.state').write_text(json.dumps(dict(format='git-on-temp-v1',id='materialized-test',git_dir=None,tree_oid=None,pin_ref=None)))
        mount=run/'mount';mount.mkdir()
        nested=mount/'d';nested.mkdir()
        (nested/'output').write_text('disposable')
        os.symlink(outside,mount/'external')
        nested.chmod(0o555);mount.chmod(0)
        result=subprocess.run([str(BINARY),'--cleanup',str(run)],cwd=self.repo,env=self.env,capture_output=True,timeout=20)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assert_outside_untouched(outside)
        self.clean()

    def test_submodule_and_lfs_prevent_launch(self):
        for mode, data in [('160000', None), ('100644', b'version https://git-lfs.github.com/spec/v1\noid sha256:' + b'a'*64 + b'\nsize 10\n')]:
            oid = self.git('rev-parse', 'HEAD').strip() if data is None else self.git('hash-object', '-w', '--stdin', data=data).strip()
            self.git('update-index', '--add', '--cacheinfo', mode + ',' + oid.decode() + ',unsupported')
            self.git('commit', '-qm', 'unsupported')
            self.before = self.git('status', '--porcelain')
            result = self.invoke('print("launched")', '--write-policy=all')
            self.assertEqual(result.returncode, 125, result.stderr)
            self.assertEqual(result.stdout, b'')
            self.clean()

    @unittest.skipUnless(sys.platform == 'darwin', 'host representation limit test on default macOS volume')
    def test_case_collision_fails_before_child(self):
        blob = self.git('hash-object', '-w', '--stdin', data=b'case').strip().decode()
        tree = self.git('mktree', data=f'100644 blob {blob}\tFoo\n100644 blob {blob}\tfoo\n'.encode()).strip().decode()
        result = subprocess.run([str(BINARY), tree, '--write-policy=all', '--', sys.executable, '-c', 'print("launched")'], cwd=self.repo, env=self.env, capture_output=True, timeout=20)
        # Explicitly establish that this host volume is case insensitive.
        probe = self.root / 'CaseProbe'
        probe.touch()
        if not (self.root / 'caseprobe').exists():
            self.skipTest('case-sensitive host; collision is faithfully representable')
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'foo', result.stderr)
        self.assertIn(b'Foo', result.stderr)
        self.assertEqual(result.stdout, b'')
        self.clean()


unittest.main(verbosity=2)
