#!/usr/bin/env python3
"""Mandatory real Apple Silicon/macFUSE acceptance. Missing prerequisites FAIL.
Run with the extracted/installed distribution: --binary /absolute/path/git-on.
No installation, extension loading, privilege or security changes are performed.
"""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',type=Path,required=True)
args=parser.parse_args()
BINARY=args.binary.resolve()
if sys.platform!='darwin' or os.uname().machine!='arm64': parser.error('native Apple Silicon Mac required')
for prerequisite in ['/Library/Filesystems/macfuse.fs/Contents/Frameworks/MFMount.framework/MFMount']:
    if not Path(prerequisite).is_file(): parser.error('missing macFUSE prerequisite: '+prerequisite)


class MacFuseTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name); self.repo=self.root/'repo'; self.repo.mkdir()
        self.tmp=self.root/'tmp'; self.tmp.mkdir()
        self.env=dict(os.environ,TMPDIR=str(self.tmp),GIT_AUTHOR_NAME='Fixture',GIT_COMMITTER_NAME='Fixture',GIT_AUTHOR_EMAIL='f@example.invalid',GIT_COMMITTER_EMAIL='f@example.invalid')
        def git(*argv):return subprocess.check_output(['git',*argv],cwd=self.repo,env=self.env)
        self.git=git
        git('init','-q'); (self.repo/'tracked').write_text('snapshot\n'); (self.repo/'.gitignore').write_text('out/\n'); (self.repo/'tool').write_text('#!/bin/sh\nprintf executable\n'); (self.repo/'tool').chmod(0o755); os.symlink('tracked',self.repo/'link')
        git('add','.');git('commit','-qm','fixture')
        self.tree=git('rev-parse','HEAD^{tree}').strip()
        (self.repo/'tracked').write_text('user edits\n');self.status=git('status','--porcelain')

    def command(self,policy,code):return [str(BINARY),'HEAD','--write-policy='+policy,'--',sys.executable,'-c',code]
    def clean(self):
        self.assertEqual(list(self.tmp.iterdir()),[])
        self.assertEqual(self.git('status','--porcelain'),self.status)
        self.assertEqual(self.git('for-each-ref','--format=%(refname)','refs/git-on/'),b'')

    def test_real_mount_all_policies_bytes_links_modes_identity_and_cleanup(self):
        for policy in ['none','ignored','untracked','all']:
            with self.subTest(policy=policy):
                code="""
import os,subprocess
from pathlib import Path
assert Path('tracked').read_bytes()==b'snapshot\\n'
assert Path('link').read_bytes()==b'snapshot\\n' and os.readlink('link')=='tracked'
assert Path('tool').stat().st_mode & 0o111 == 0o111
assert subprocess.check_output(['./tool'])==b'executable'
assert not Path('.git').exists()
def write(path):
 try:Path(path).write_text('temporary');return True
 except PermissionError:return False
 except OSError as e:
  assert e.errno in [1,13,30],e
  return False
"""+f"assert write('tracked') == {policy=='all'}\nassert write('fresh') == {policy in ['untracked','all']}\n"+"""
try:Path('out').mkdir(); created=True
except OSError as e:assert e.errno in [1,13,30];created=False
"""+f"assert created == {policy!='none'}\n"+"print(os.environ['GIT_ON_TREE_OID'])\n"
                result=subprocess.run(self.command(policy,code),cwd=self.repo,env=self.env,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertNotIn(b'materialized backend',result.stderr)
                self.assertEqual(result.stdout.strip(),self.tree)
                self.clean()

    def test_status_signals_and_checked_normal_cleanup(self):
        result=subprocess.run(self.command('all','raise SystemExit(23)'),cwd=self.repo,env=self.env,capture_output=True,timeout=20)
        self.assertEqual(result.returncode,23,result.stderr);self.clean()
        for sig in [signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGQUIT]:
            ready=self.root/'ready';ready.unlink(missing_ok=True)
            code=f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(60)"
            child=subprocess.Popen(self.command('ignored',code),cwd=self.repo,env=self.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            try:
                deadline=time.monotonic()+15
                while not ready.exists() and child.poll() is None and time.monotonic()<deadline:time.sleep(.02)
                self.assertTrue(ready.exists(),'real mount/child did not become ready')
                child.send_signal(sig);out,err=child.communicate(timeout=15)
                self.assertEqual(child.returncode,-sig,(out,err));self.clean()
            finally:
                if child.poll() is None:child.kill();child.communicate(timeout=10)

    def test_actual_posix_permissions_are_enforced(self):
        code=r'''
import os,subprocess
from pathlib import Path
assert os.geteuid()!=0
Path('out').mkdir()
p=Path('out/locked');p.write_bytes(b'private');p.chmod(0)
for flags in [os.O_RDONLY,os.O_WRONLY,os.O_RDWR|os.O_TRUNC]:
 try:fd=os.open(p,flags)
 except PermissionError:pass
 else:os.close(fd);raise AssertionError('mode-000 file access allowed')
p.chmod(0o600)
d=Path('out/readonly');d.mkdir();d.chmod(0o555)
try:(d/'new').write_text('must fail')
except PermissionError:pass
else:raise AssertionError('write into mode-0555 directory allowed')
d.chmod(0o755)
p=Path('out/program');p.write_text('#!/bin/sh\nexit 0\n');p.chmod(0o644)
try:subprocess.run(['./out/program'],check=True)
except PermissionError:pass
else:raise AssertionError('non-executable mode ignored')
'''
        result=subprocess.run(self.command('ignored',code),cwd=self.repo,env=self.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr);self.clean()

    def test_native_exclusive_rename_preserves_both_files(self):
        code=r'''
import ctypes,errno
from pathlib import Path
Path('out').mkdir()
Path('out/source').write_bytes(b'source')
Path('out/destination').write_bytes(b'destination')
libc=ctypes.CDLL(None,use_errno=True)
rename=libc.renamex_np
rename.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_uint]
rename.restype=ctypes.c_int
assert rename(b'out/source',b'out/destination',4)==-1
assert ctypes.get_errno()==errno.EEXIST
assert Path('out/source').read_bytes()==b'source'
assert Path('out/destination').read_bytes()==b'destination'
'''
        result=subprocess.run(self.command('ignored',code),cwd=self.repo,env=self.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr);self.clean()


unittest.main(argv=[sys.argv[0]],verbosity=2)
