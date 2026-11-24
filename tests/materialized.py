#!/usr/bin/env python3
"""Test the copy backend, including automatic fallback without a native mount."""
import base64
from hashlib import shake_256
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

from support.git_invocations import git_invocations, record_git_invocations

BINARY = Path(sys.argv.pop(1)).resolve()
# On Linux the fixture hides fusermount3. Darwin 25+ supplies NFS independently
# of PATH, so exercise the same copy backend explicitly there.
COPY_ARGS = ['--backend=copy'] if sys.platform == 'darwin' and int(os.uname().release.split('.')[0]) >= 25 else []


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
        result = subprocess.run([str(BINARY), *COPY_ARGS, 'HEAD', *options, '--', sys.executable, '-c', code, '--keep', '-k', '', 'a b'], cwd=self.repo, env=self.env, capture_output=True, timeout=20)
        return result

    def clean(self):
        self.assertEqual(list(self.tmp.iterdir()), [])
        self.assertEqual(self.git('status', '--porcelain'), self.before)
        self.assertEqual(self.git('for-each-ref', '--format=%(refname)', 'refs/git-on/'), b'')

    def test_alternates_comment_does_not_spawn_git_per_object(self):
        objects = self.repo / '.git' / 'objects'
        (objects / 'info' / 'alternates').write_text('# no configured disk alternates\n\n')
        oid = self.git('rev-parse', 'HEAD:tracked').decode().strip()
        alternate = self.root / 'alternate-objects'
        (alternate / oid[:2]).mkdir(parents=True)
        (objects / oid[:2] / oid[2:]).rename(alternate / oid[:2] / oid[2:])
        self.env, log = record_git_invocations(self.root, self.env)
        self.env['GIT_ALTERNATE_OBJECT_DIRECTORIES'] = str(alternate)
        result = self.invoke(
            "from pathlib import Path; print(Path('tracked').read_bytes().hex())",
            '--write-policy=all',
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'636f6d6d697474656400ff0d0a\n')
        starts = git_invocations(log)
        self.assertTrue(starts)
        self.assertEqual([args for args in starts if 'cat-file' in args], [])
        self.assertEqual([args for args in starts if 'config' in args and '--list' in args], [])
        self.clean()

    def test_default_and_protected_policies_are_never_relaxed(self):
        for options in [[], ['--write-policy=none'], ['--write-policy=ignored'], ['--write-policy=untracked']]:
            result = self.invoke('raise Exception("child must not start")', *options)
            if COPY_ARGS:
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(b'requires explicit --write-policy=all', result.stderr)
            else:
                self.assertEqual(result.returncode, 125, result.stderr)
                backend = b'NFS' if sys.platform == 'darwin' else b'FUSE'
                self.assertIn(backend + b' unavailable', result.stderr)
            self.clean()

    def test_small_files_materialize_with_a_reduced_descriptor_limit(self):
        import hashlib
        import resource
        def limit_descriptors():
            resource.setrlimit(resource.RLIMIT_NOFILE,(64,64))
        for index in range(80):
            (self.repo/f'f{index:05d}').write_bytes(hashlib.sha256(str(index).encode()).digest()+b'\0'*(8192-32))
        self.git('add','.');self.git('commit','-qm','small-file descriptor regression')
        self.before=self.git('status','--porcelain')
        code="""
import hashlib
from pathlib import Path
for index in range(80):
 expected=hashlib.sha256(str(index).encode()).digest()+b'\\0'*(8192-32)
 assert Path(f'f{index:05d}').read_bytes()==expected
"""
        for label,operands in [('local',['HEAD']),('remote',[self.repo.as_uri(),'HEAD'])]:
            with self.subTest(source=label):
                result=subprocess.run([str(BINARY),*COPY_ARGS,*operands,'--write-policy=all','--',sys.executable,'-I','-S','-c',code],cwd=self.repo,env=self.env,preexec_fn=limit_descriptors,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
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
            self.assertEqual(result.stderr, b'')
            self.assertEqual(result.stdout.strip(), self.oid)
            self.clean()

    def large_local_fixture(self,size=6*1024*1024):
        (self.repo/'large').write_bytes(b'x'*size)
        self.git('add','.'); self.git('commit','-qm','materialization disk budget')
        self.before=self.git('status','--porcelain')
        return [sys.executable,'-I','-S','-c',f"""
from pathlib import Path
assert Path('large').read_bytes()==b'x'*{size}
root=Path.cwd() if Path('.git').exists() else Path.cwd().parent
allocated=sum(p.lstat().st_blocks*512 for p in root.rglob('*'))
assert allocated < 9*1024*1024, ('materialization disk allocation',allocated)
print('large file verified')
"""]

    def test_large_local_materialization_fits_checkout_disk_budget(self):
        command=self.large_local_fixture()
        for encoding in ['loose','packed']:
            with self.subTest(encoding=encoding):
                if encoding=='packed': self.git('gc','--quiet')
                clone=self.root/f'clone-{encoding}'
                result=subprocess.run(['git','clone','--no-local','--quiet',str(self.repo),str(clone)],
                                      env=self.env,capture_output=True,timeout=20)
                self.assertEqual(result.returncode,0,result.stderr)
                direct=subprocess.run(command,cwd=clone,env=self.env,capture_output=True,timeout=10)
                self.assertEqual(direct.returncode,0,direct.stderr)
                result=subprocess.run([str(BINARY),*COPY_ARGS,'HEAD','--write-policy=all','--',*command],
                                      cwd=self.repo,env=self.env,capture_output=True,timeout=20)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,direct.stdout)
                self.clean()

    @unittest.skipUnless(sys.platform=='linux','private tmpfs quota requires Linux mount namespaces')
    def test_large_local_materialization_succeeds_on_nine_mib_tmpfs(self):
        from filesystem_quota import compare_on_tmpfs
        compare_on_tmpfs(self,self.root,BINARY,self.repo,self.env['PATH'],self.large_local_fixture())
        self.clean()

    def remote_materialization_fixture(self):
        command=self.large_local_fixture(7*1024*1024)
        self.git('config','uploadpack.allowFilter','true')
        self.git('config','uploadpack.allowAnySHA1InWant','true')
        return self.repo.as_uri(),command

    def test_native_remote_copy_reuses_one_decoder_for_a_pack(self):
        import hashlib

        paths = ['payload-a', 'payload-b', 'payload-c']
        expected = []
        for name in paths:
            data = shake_256(name.encode()).digest(80 * 1024)
            (self.repo / name).write_bytes(data)
            expected.append(hashlib.sha256(data).hexdigest())
        with (self.repo / '.gitattributes').open('a') as attributes:
            attributes.write('payload-* -text\n')
        self.git('add', '.gitattributes', *paths)
        self.git('commit', '-qm', 'native packed blobs')
        self.git('config', 'fetch.prune', 'false')
        self.before = self.git('status', '--porcelain')
        environment, log = record_git_invocations(self.root, self.env)
        result = subprocess.run(
            [str(BINARY), *COPY_ARGS, self.repo.as_uri(), 'HEAD', '--write-policy=all', '--',
             sys.executable, '-c',
             'import hashlib, pathlib, sys\n'
             'for path in sys.argv[1:]: print(hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest())',
             *paths],
            cwd=self.repo, env=environment, capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.decode().splitlines(), expected)
        calls = git_invocations(log)
        self.assertEqual(len([args for args in calls if 'fetch' in args]), 1, calls)
        readers = [args for args in calls if 'cat-file' in args]
        self.assertEqual(len(readers), 1, readers)
        self.assertIn('--batch-command', readers[0])
        self.clean()

    def commit_with_large_message(self):
        message = base64.b64encode(shake_256(b'commit metadata fixture').digest(512 * 1024))
        self.git('commit', '--allow-empty', '-q', '-F', '-', data=message)

    def test_file_snapshot_streams_with_default_fetch_policy(self):
        self.git('config', 'pull.rebase', 'true')
        source = self.repo.as_uri()
        args = [
            BINARY, source, 'HEAD', '--backend=copy', '--write-policy=all', '--',
            sys.executable, '-I', '-S', '-c',
            "from pathlib import Path; "
            "print((Path.cwd().parent / 'remote-repository').exists()); "
            "print(Path('tracked').read_bytes().hex())",
        ]
        child_env = {k: v for k, v in self.env.items() if not k.startswith('GIT_CONFIG_')}
        child_env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null')
        child_env, calls = record_git_invocations(self.root, child_env)
        for key, value, native in ((None, None, False), ('ssh.variant', 'ssh', False),
                                   ('color.ui', 'auto', False),
                                   ('fetch.parallel', '1', True)):
            with self.subTest(configuration=key):
                calls.write_text('')
                config = ({} if key is None else
                          {'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': key,
                           'GIT_CONFIG_VALUE_0': value})
                result = subprocess.run(args, cwd=self.repo, env=child_env | config,
                                        capture_output=True, timeout=20)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(),
                                 [str(native).encode(), b'636f6d6d697474656400ff0d0a'])
                self.assertEqual(any('fetch' in args for args in git_invocations(calls)), native)
                self.clean()

    @unittest.skipUnless(sys.platform == 'linux', 'private tmpfs quota requires Linux mount namespaces')
    def test_native_fetch_with_packed_metadata_fits_two_checkouts(self):
        from filesystem_quota import compare_on_tmpfs
        self.commit_with_large_message()
        # The clone keeps these similar tag messages as deltas. Importing
        # them individually as loose objects exceeds its disk budget.
        message = base64.b64encode(shake_256(b'tag metadata fixture').digest(192 * 1024))
        revision = 'HEAD'
        for index in range(6):
            name = f'nested-{index}'
            self.git('tag', '-a', '--cleanup=verbatim', '-F', '-', name, revision, data=message)
            revision = f'refs/tags/{name}'
        command = [sys.executable, '-I', '-S', '-c',
                   "from pathlib import Path; assert Path('tool').is_file(); print('ready')"]
        compare_on_tmpfs(self, self.root, BINARY, self.repo, self.env['PATH'], command,
                         size_mib=1, source=self.repo.as_uri(), checkout_copies=2,
                         git_config={'fetch.parallel': '1', 'fetch.unpackLimit': '1'},
                         revision=revision, service_on_volume=True)
        self.clean()

    @unittest.skipUnless(sys.platform=='linux','private tmpfs quota requires Linux mount namespaces')
    def test_large_remote_materialization_succeeds_on_nine_mib_tmpfs(self):
        from filesystem_quota import compare_on_tmpfs
        source,command=self.remote_materialization_fixture()
        compare_on_tmpfs(self,self.root,BINARY,self.repo,self.env['PATH'],command,source=source)
        self.clean()

    def test_command_status_and_signal_cleanup(self):
        code = f"""
import os, subprocess, sys
from pathlib import Path
run = Path.cwd().parent
for name in ('.state', '.state-new', '.lock'):
    assert not (run / name).exists(), name
record = run.with_name(f'.git-on-run-{{os.geteuid():x}}-{{run.name}}')
[marker] = record.glob('.git-on-lock-v2-*')
expected = (b'run is active or locked',)
if marker.name.startswith('.git-on-lock-v2-u'):
    expected = (b'persistent run directory identity', b'does not match its ownership record')
cleanup = subprocess.run([{str(BINARY)!r}, '--cleanup', str(run)], capture_output=True, timeout=5)
assert cleanup.returncode != 0 and any(reason in cleanup.stderr for reason in expected), cleanup
assert Path('tracked').is_file()
print(run)
sys.exit(23)
"""
        result = self.invoke(code, '--write-policy=all')
        self.assertEqual(result.returncode, 23, result.stderr)
        run = Path(os.fsdecode(result.stdout).strip())
        self.assertEqual(run.parent, self.tmp.resolve() / 'git-on')
        self.assertFalse(run.exists())
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
        result = subprocess.run([str(BINARY), *COPY_ARGS, tree, '--write-policy=all', '--', sys.executable, '-c', 'print("launched")'], cwd=self.repo, env=self.env, capture_output=True, timeout=20)
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
