#!/usr/bin/env python3
"""Test the copy backend, including automatic fallback without a native mount."""
import os
import json
from pathlib import Path
import shlex
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

    @unittest.skipUnless(sys.platform == "darwin", "Apple Git launcher")
    def test_apple_git_leaves_the_system_temp_directory_empty(self):
        runtime = self.root / "native-git-tmp"
        runtime.mkdir()
        env = dict(self.env, PATH="/usr/bin:/bin:/usr/sbin:/sbin", TMPDIR=str(runtime))
        result = subprocess.run(
            [BINARY, "HEAD", "--backend=copy", "--write-policy=all", "--", "/bin/cat", "tracked"],
            cwd=self.repo, env=env, capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"committed\x00\xff\r\n")
        self.assertEqual(list(runtime.iterdir()), [])

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

    def test_small_files_respect_individual_fsize_limits_without_aggregate_ceiling(self):
        import hashlib
        import resource
        limit=65536
        def limits():
            signal.signal(signal.SIGXFSZ,signal.SIG_IGN)
            resource.setrlimit(resource.RLIMIT_FSIZE,(limit,limit))
            resource.setrlimit(resource.RLIMIT_NOFILE,(64,64))
        for index in range(80):
            (self.repo/f'f{index:05d}').write_bytes(hashlib.sha256(str(index).encode()).digest()+b'\0'*(8192-32))
        self.git('add','.');self.git('commit','-qm','small-file FSIZE regression')
        self.before=self.git('status','--porcelain')
        code="""
import hashlib,os
from pathlib import Path
assert not any(p.name.startswith('raw-parts-') for p in Path('.').iterdir())
for _ in range(2):
 for index in range(80):
  expected=hashlib.sha256(str(index).encode()).digest()+b'\\0'*(8192-32)
  assert Path(f'f{index:05d}').read_bytes()==expected
"""
        clone=self.root/'clone'
        result=subprocess.run(['git','clone','--quiet',self.repo,clone],env=self.env,preexec_fn=limits,capture_output=True,timeout=20)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(len(list(clone.glob('f*'))),80)
        result=subprocess.run([str(BINARY),*COPY_ARGS,'HEAD','--write-policy=all','--',sys.executable,'-I','-S','-c',code],cwd=self.repo,env=self.env,preexec_fn=limits,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr)
        self.clean()

    def test_incompressible_objects_respect_fsize_when_clone_and_command_succeed(self):
        import hashlib
        import resource
        import zlib
        with (self.repo/'.gitattributes').open('a') as attributes:
            attributes.write('random-* -text\n')
        for index in range(16):
            payload=hashlib.shake_256(str(index).encode()).digest(8192)
            self.assertGreater(len(zlib.compress(payload)),8192)
            (self.repo/f'random-{index:02d}').write_bytes(payload)
        self.git('add','.')
        self.git('commit','-qm','incompressible file-size regression')
        self.git('config','uploadpack.allowFilter','true')
        self.git('config','uploadpack.allowAnySHA1InWant','true')
        self.before=self.git('status','--porcelain')
        def limits():
            signal.signal(signal.SIGXFSZ,signal.SIG_DFL)
            resource.setrlimit(resource.RLIMIT_FSIZE,(16384,16384))
            resource.setrlimit(resource.RLIMIT_CORE,(0,0))
        command=[sys.executable,'-I','-S','-c',
                 "import hashlib; from pathlib import Path; assert all(Path(f'random-{i:02d}').read_bytes()==hashlib.shake_256(str(i).encode()).digest(8192) for i in range(16)); print('incompressible files verified')"]
        clone=self.root/'clone'
        result=subprocess.run(['git','clone','--quiet',str(self.repo),str(clone)],env=self.env,
                              preexec_fn=limits,capture_output=True,timeout=20)
        self.assertEqual(result.returncode,0,result.stderr)
        direct=subprocess.run(command,cwd=clone,env=self.env,preexec_fn=limits,capture_output=True,timeout=10)
        self.assertEqual(direct.returncode,0,direct.stderr)
        result=subprocess.run([str(BINARY),*COPY_ARGS,str(self.repo),'HEAD','--write-policy=all','--',*command],
                              cwd=self.repo,env=self.env,preexec_fn=limits,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout,direct.stdout)
        self.clean()

    def test_bytes_links_modes_argv_identity_freshness_and_source_isolation(self):
        for _ in range(2):
            result = self.invoke("""
import os,sys,subprocess
from pathlib import Path
assert not subprocess.check_output(['git', '-C', SOURCE_REPOSITORY, 'for-each-ref', 'refs/git-on/'])
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
""".replace('SOURCE_REPOSITORY', repr(str(self.repo))), '--write-policy=all')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, b'')
            self.assertEqual(result.stdout.strip(), self.oid)
            self.clean()

    def test_explicit_local_sources_use_rev_parse_without_fetch_or_hooks(self):
        self.git('update-index', '--add', '--cacheinfo',
                 '100644,' + self.git('rev-parse', 'HEAD:tracked').strip().decode() + ',deep/file')
        self.git('commit', '-qm', 'subtree')
        caller = self.root / 'caller'
        caller.mkdir()
        subprocess.run(['git', 'init', '-q'], cwd=caller, env=self.env, check=True)
        subprocess.run(['git', 'remote', 'add', 'origin', '../repo'],
                       cwd=caller, env=self.env, check=True)
        nested = caller / 'nested'
        nested.mkdir()
        hooks = caller / 'hooks'
        hooks.mkdir()
        hook = hooks / 'reference-transaction'
        marker = self.root / 'hook-ran'
        hook.write_text(f'#!{sys.executable}\nwith open({str(marker)!r}, "a"): pass\n')
        hook.chmod(0o700)
        subprocess.run(['git', 'config', 'core.hooksPath', str(hooks)],
                       cwd=caller, env=self.env, check=True)
        original = self.git('show-ref')
        env, invocation_log = record_git_invocations(self.root, self.env)
        command = ['HEAD:deep', '--backend=copy', '--write-policy=all',
                   '--', sys.executable, '-I', '-S', '-c',
                   "import sys; from pathlib import Path; "
                   "assert not (Path.cwd().parent / 'remote-repository').exists(); "
                   "sys.stdout.buffer.write(Path('file').read_bytes())"]
        direct = subprocess.run([BINARY, *command], cwd=self.repo, env=env,
                                capture_output=True, timeout=20)
        self.assertEqual(direct.returncode, 0, direct.stderr)
        self.assertEqual(direct.stdout, b'committed\x00\xff\r\n')
        for source, cwd in (('../repo', caller), ('../../repo', nested),
                            (self.repo.as_uri(), caller), ('origin', nested)):
            with self.subTest(source=source):
                result = subprocess.run([BINARY, source, *command], cwd=cwd, env=env,
                                        capture_output=True, timeout=20)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, direct.stdout)
        destination = 'refs/heads/demo'
        result = subprocess.run([BINARY, 'origin', 'HEAD:' + destination, *command[1:]],
                                cwd=caller, env=env, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertEqual(result.stdout, b'')
        self.assertFalse(any('fetch' in argv for argv in git_invocations(invocation_log)))
        self.assertFalse(marker.exists())
        self.assertEqual(self.git('show-ref'), original)
        self.assertEqual(list(self.tmp.iterdir()), [])

    def test_explicit_reftable_source_uses_native_object_reads(self):
        source = self.root / "table-repository"
        run = subprocess.run(["git", "init", "--ref-format=reftable", "-q", source],
                             env=self.env, capture_output=True, timeout=10)
        if run.returncode and b"unknown option" in run.stderr:
            self.skipTest("this Git does not support reftable")
        self.assertEqual(run.returncode, 0, run.stderr)
        (source / "value").write_text("native repository\n")
        subprocess.run(["git", "-C", source, "add", "--", "value"],
                       env=self.env, check=True, timeout=10)
        subprocess.run(["git", "-C", source, "-c", "user.name=Fixture",
                        "-c", "user.email=fixture@example.invalid", "commit", "-qm", "table"],
                       env=self.env, check=True, timeout=10)
        result = subprocess.run([BINARY, source, "HEAD", "--backend=copy", "--write-policy=all",
                                 "--", "/bin/cat", "value"], cwd=self.repo, env=self.env,
                                capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"native repository\n")

    def test_native_objects_when_libgit2_cannot_open_the_odb(self):
        objects = self.repo / ".git/objects"
        alternate = self.root / "alternate"
        objects.rename(alternate)
        (alternate / "info/alternates").mkdir()
        (objects / "info").mkdir(parents=True)
        (objects / "info/alternates").write_text(f"{alternate}\n")
        expected = self.git("cat-file", "blob", "HEAD:tracked")
        self.assertEqual(expected, b"committed\x00\xff\r\n")
        result = subprocess.run(
            [BINARY, self.repo, "HEAD", "--backend=copy", "--write-policy=all",
             "--", "/bin/cat", "tracked"],
            cwd=self.root, env=self.env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, expected)
        self.clean()

    def test_relative_disk_alternate_uses_the_selected_source_directory(self):
        objects = self.repo / '.git/objects'
        oid = self.git('rev-parse', 'HEAD:tracked').strip().decode()
        object_name = Path(oid[:2]) / oid[2:]
        alternate = objects / 'pool'
        other = self.root / 'pool'
        (alternate / object_name.parent).mkdir(parents=True)
        (other / object_name.parent).mkdir(parents=True)
        (objects / object_name).rename(other / object_name)
        (objects / 'info/alternates').write_text('pool\n')
        command = [BINARY, self.repo, 'HEAD', '--backend=copy',
                   '--write-policy=all', '--', '/bin/cat', 'tracked']
        oracle = subprocess.run(['git', '-C', self.repo, 'cat-file', 'blob', oid],
                                env=self.env, capture_output=True, timeout=15)
        self.assertNotEqual(oracle.returncode, 0)
        result = subprocess.run(command, cwd=self.root, env=self.env, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertEqual(result.stdout, b'')
        (other / object_name).rename(alternate / object_name)
        result = subprocess.run(command, cwd=self.root, env=self.env, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, self.git('cat-file', 'blob', oid))
        self.clean()

    def test_shared_clone_reads_objects_without_a_native_helper(self):
        shared = self.root / 'shared'
        reference = self.root / 'reference'
        self.git('clone', '--quiet', self.repo, reference)
        self.git('clone', '--shared', '--reference', reference, '--quiet', self.repo, shared)
        alternates = (shared / '.git/objects/info/alternates').read_text().splitlines()
        self.assertEqual({Path(line).resolve() for line in alternates},
                         {(repo / '.git/objects').resolve() for repo in (self.repo, reference)})
        # Each alternate must contribute a blob to materialize the checkout.
        for repo, name in ((self.repo, 'link'), (reference, 'tracked')):
            oid = self.git('rev-parse', f'HEAD:{name}').strip().decode()
            (repo / '.git/objects' / oid[:2] / oid[2:]).unlink()
        self.before = self.git('status', '--porcelain')
        env, log = record_git_invocations(self.root, self.env)
        result = subprocess.run(
            [BINARY, shared, 'HEAD', '--backend=copy', '--write-policy=all',
             '--', '/bin/cat', 'tracked', 'link'],
            cwd=self.root, env=env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, self.git('cat-file', 'blob', 'HEAD:tracked') * 2)
        self.assertFalse(any('cat-file' in argv for argv in git_invocations(log)))
        self.clean()

    def test_explicit_index_and_alternates_match_implicit_source_on_older_git(self):
        pool = self.root / 'objects'
        pool.mkdir()
        index = self.root / 'alternate-index'
        def git(*args, data=None, **settings):
            return subprocess.check_output(['git', *args], cwd=self.repo,
                                           env=dict(self.env, **settings), input=data).strip().decode()
        objects = {'GIT_OBJECT_DIRECTORY': str(pool)}
        blob = git('hash-object', '-w', '--stdin', data=b'alternate contents\n', **objects)
        tree = git('mktree', data=f'100644 blob {blob}\ttracked\n'.encode(), **objects)
        commit = git('commit-tree', tree, data=b'alternate commit\n', **objects)
        settings = dict(GIT_INDEX_FILE=str(index), GIT_ALTERNATE_OBJECT_DIRECTORIES=str(pool))
        git('update-index', '--add', '--cacheinfo', f'160000,{commit},view', **settings)
        self.assertEqual(git('rev-parse', ':view', **settings), commit)
        before = index.read_bytes()
        from support.git_invocations import git_invocations, record_git_invocations
        environs, log = record_git_invocations(self.root, self.env)
        for name, path in [('GIT_ALTERNATE_OBJECT_DIRECTORIES', pool),
                           ('GIT_OBJECT_DIRECTORY', self.repo / '.git/objects')]:
            with self.subTest(primary_objects=name):
                log.write_text('')
                result = subprocess.run(
                    [BINARY, self.repo, 'HEAD', '--backend=copy', '--write-policy=all', '--', '/bin/cat', 'tracked'],
                    cwd=self.root, env=dict(environs, **{name: str(path)}), capture_output=True, timeout=15,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'committed\x00\xff\r\n')
                self.assertFalse(any('cat-file' in argv for argv in git_invocations(log)))

        # Simulate a Git with cat-file and index support but no lazy-fetch switch.
        tools = self.root / 'older-git'
        tools.mkdir()
        shim = tools / 'git'
        shim.write_text('#!/bin/sh\nif [ "$1" = --no-lazy-fetch ]; then exit 129; fi\n'
                        f'exec {shlex.quote(shutil.which("git"))} "$@"\n')
        shim.chmod(0o700)
        env = dict(self.env, **settings)
        env['PATH'] = str(tools)
        command = [':view', '--backend=copy', '--write-policy=all', '--', '/bin/cat', 'tracked']
        for cwd, operands in [(self.repo, []), (self.root, [self.repo])]:
            result = subprocess.run([BINARY, *operands, *command],
                                    cwd=cwd, env=env, capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, b'alternate contents\n')
        self.assertEqual(index.read_bytes(), before)
        self.git('config', 'remote.origin.promisor', 'true')
        result = subprocess.run([BINARY, self.repo, *command], cwd=self.root,
                                env=dict(env, GIT_CONFIG=os.devnull),
                                capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'supports --no-lazy-fetch', result.stderr)
        self.assertEqual(result.stdout, b'')
        self.clean()

    def test_index_revision_does_not_run_the_configured_fsmonitor(self):
        commit = self.git('rev-parse', 'HEAD').strip().decode()
        self.git('update-index', '--add', '--cacheinfo', f'160000,{commit},view')
        helper = self.root / 'fsmonitor'
        marker = self.repo / 'helper-done'
        helper.write_text(f'#!{sys.executable}\nimport sys\nwith open({str(marker)!r}, "a"): pass\n'
                          'sys.stdout.buffer.write(b"clock\\0")\n')
        helper.chmod(0o700)
        self.git('update-index', '--split-index')
        shared, = (self.repo / '.git').glob('sharedindex.*')
        os.utime(shared, (1, 1))
        shared_mtime = shared.stat().st_mtime_ns
        self.git('config', 'core.fsmonitor', str(helper))
        before = (self.repo / '.git/index').read_bytes()
        trace = self.root / 'git-trace'
        result = subprocess.run([BINARY, ':view', '--backend=copy', '--write-policy=all',
                                 '--', '/bin/cat', 'tracked'], cwd=self.repo,
                                env=dict(self.env, GIT_TRACE2_EVENT=str(trace)),
                                capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'committed\x00\xff\r\n')
        self.assertFalse(marker.exists())
        self.assertFalse(trace.exists())
        self.assertEqual((self.repo / '.git/index').read_bytes(), before)
        self.assertEqual(shared.stat().st_mtime_ns, shared_mtime)

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
        (run/'.state').write_text(json.dumps(dict(format='git-on-temp-v1', id='materialized-test')))
        mount=run/'mount';mount.mkdir()
        nested=mount/'d';nested.mkdir()
        (nested/'output').write_text('disposable')
        os.symlink(outside,mount/'external')
        nested.chmod(0o555);mount.chmod(0)
        result=subprocess.run([str(BINARY),'--cleanup',str(run)],cwd=self.repo,env=self.env,capture_output=True,timeout=20)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assert_outside_untouched(outside)
        self.clean()

    def test_cleanup_preserves_legacy_source_ref_and_recovery_record(self):
        run = self.tmp / 'git-on-recovery'
        run.mkdir(mode=0o700)
        (run / '.lock').touch(mode=0o600)
        ref = 'refs/git-on/legacy'
        self.git('update-ref', ref, self.oid.decode())
        record = json.dumps(dict(format='git-on-temp-v1', id='legacy', pin_ref=ref,
                                 git_dir=str(self.repo / '.git'), tree_oid=self.oid.decode()))
        (run / '.state').write_text(record)
        result = subprocess.run([BINARY, '--cleanup', run], cwd=self.repo, env=self.env,
                                capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'use git-on 1.5.2', result.stderr)
        self.assertEqual((run / '.state').read_text(), record)
        self.assertEqual(self.git('rev-parse', '--verify', ref).strip(), self.oid)

        changed = self.git('rev-parse', 'HEAD').strip()
        self.git('update-ref', ref, changed.decode())
        result = subprocess.run([BINARY, '--cleanup', run], cwd=self.repo, env=self.env,
                                capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'different target', result.stderr)
        self.assertEqual((run / '.state').read_text(), record)
        self.assertEqual(self.git('rev-parse', '--verify', ref).strip(), changed)

        offline = self.root / 'offline'
        self.repo.rename(offline)
        result = subprocess.run([BINARY, '--cleanup', run], cwd=self.root, env=self.env,
                                capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'legacy source repository unavailable', result.stderr)
        self.assertEqual((run / '.state').read_text(), record)
        offline.rename(self.repo)

        # A pre-upgrade run could have stopped after recording the ref name
        # but before creating it. An absent ref needs no legacy Git changes.
        self.git('update-ref', '-d', ref)
        result = subprocess.run([BINARY, '--cleanup', run], cwd=self.repo, env=self.env,
                                capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.clean()

    def test_submodule_and_lfs_prevent_launch(self):
        for mode, data in [('160000', None), ('100644', b'version https://git-lfs.github.com/spec/v1\noid sha256:' + b'a'*64 + b'\nsize 10\n')]:
            oid = self.git('rev-parse', 'HEAD').strip() if data is None else self.git('hash-object', '-w', '--stdin', data=data).strip()
            self.git('update-index', '--add', '--cacheinfo', mode + ',' + oid.decode() + ',unsupported')
            self.git('commit', '-qm', 'unsupported')
            self.before = self.git('status', '--porcelain')
            for storage in ('loose', 'packed') if data is not None else ('loose',):
                with self.subTest(mode=mode, storage=storage):
                    if storage == 'packed':
                        self.git('repack', '-a', '-d', '--window=0')
                    result = self.invoke('print("launched")', '--write-policy=all')
                    self.assertEqual(result.returncode, 125, result.stderr)
                    if data is not None:
                        self.assertIn(b'Git LFS pointer', result.stderr)
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
