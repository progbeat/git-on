#!/usr/bin/env python3
"""Real-mount per-file fsync callback/error probe; requires a test-metrics binary.

A successful fsync must reach backing storage. On Linux, injected EIO must
reach the caller. On macOS it must be visible in the runner's result even
when the kernel consumes the error while flushing buffered writes.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',type=Path,required=True)
parser.add_argument('--require-nfs', action='store_true')
args=parser.parse_args()
with tempfile.TemporaryDirectory(prefix='git-on-fsync-contract-') as scratch:
    root=Path(scratch);repo=root/'repo';repo.mkdir();temporary=root/'tmp';temporary.mkdir()
    env=dict(os.environ,TMPDIR=str(temporary),GIT_AUTHOR_NAME='Fixture',GIT_COMMITTER_NAME='Fixture',GIT_AUTHOR_EMAIL='f@example.invalid',GIT_COMMITTER_EMAIL='f@example.invalid')
    def git(*argv):return subprocess.check_output(['git',*argv],cwd=repo,env=env)
    git('init','-q');(repo/'.gitignore').write_text('out/\n');git('add','.');git('commit','-qm','sync fixture')
    for inject in ('0','1'):
        code=r'''
import errno,os,sys
from pathlib import Path
Path('out').mkdir()
try:
 with open('out/file','wb',buffering=0) as f:
  f.write(b'committed-to-backing'*4096)
  os.fsync(f.fileno())
except OSError as error:
 assert os.environ['GIT_ON_TEST_FSYNC_ERROR']=='1' and error.errno==errno.EIO,error
else:
 assert os.environ['GIT_ON_TEST_FSYNC_ERROR']=='0' or sys.platform=='darwin','fsync ignored the daemon EIO'
print('child completed')
'''
        read_fd,write_fd=os.pipe()
        try:
            backend=['--backend=nfs'] if args.require_nfs else []
            result=subprocess.run([str(args.binary.resolve()),'HEAD',*backend,'--',sys.executable,'-c',code],cwd=repo,
                                  env=dict(env,GIT_ON_TEST_METRICS_FD=str(write_fd),GIT_ON_TEST_FSYNC_ERROR=inject),
                                  pass_fds=[write_fd],capture_output=True,timeout=60)
        finally:os.close(write_fd)
        with os.fdopen(read_fd) as stream:raw=stream.read()
        metrics=json.loads(raw)
        if args.require_nfs:
            assert metrics['backend']=='nfs',metrics
        assert metrics['file_sync_callbacks']>0,('no real fsync callback',inject,metrics)
        assert result.stdout==b'child completed\n',(result.stdout,result.stderr)
        if args.require_nfs and inject=='1':
            assert result.returncode==125 and b'NFS writeback failed' in result.stderr, result
        else:
            assert result.returncode==0,(inject,result.stdout,result.stderr)
        assert not list(temporary.iterdir()) and git('status','--porcelain')==b''
        print(json.dumps({'injected_EIO':inject=='1','file_sync_callbacks':metrics['file_sync_callbacks'],'caller_received_expected_result':True},sort_keys=True))
