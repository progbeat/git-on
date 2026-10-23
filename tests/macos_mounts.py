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
for prerequisite in ['/usr/local/lib/libfuse.2.dylib','/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse']:
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


unittest.main(argv=[sys.argv[0]],verbosity=2)
