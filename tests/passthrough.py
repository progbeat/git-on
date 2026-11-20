#!/usr/bin/env python3
"""Real Linux FUSE bypass and coherence check; needs a test-metrics build.
--expect=enabled requires actual registered passthrough plus zero generated-file
read/write callbacks. --expect=fallback requires ordinary userspace callbacks.
No sudo, capability changes, detached execution or forced unmount is performed.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary',type=Path,required=True)
p.add_argument('--expect',choices=['enabled','fallback'],required=True)
args=p.parse_args()
assert os.uname().sysname=='Linux','real Linux FUSE required'
assert Path('/dev/fuse').exists() and shutil.which('fusermount3'),'usable FUSE required'
with tempfile.TemporaryDirectory() as scratch:
    root=Path(scratch); repo=root/'repo'; repo.mkdir(); tmp=root/'tmp'; tmp.mkdir()
    env=dict(os.environ,TMPDIR=str(tmp),GIT_ON_EXPECT_UID=str(os.getuid()),GIT_AUTHOR_NAME='Fixture',GIT_COMMITTER_NAME='Fixture',GIT_AUTHOR_EMAIL='f@example.invalid',GIT_COMMITTER_EMAIL='f@example.invalid')
    def git(*argv): subprocess.run(['git',*argv],cwd=repo,env=env,check=True,capture_output=True)
    git('init','-q')
    (repo/'.gitignore').write_text('out/\n')
    (repo/'program.c').write_text('#include <stdio.h>\nint main(void) { puts("compiled"); return 0; }\n')
    (repo/'tracked-data').write_bytes(b't' * (128 * 1024))
    git('add','.'); git('commit','-qm','fixture')
    code=r'''
import mmap,os,subprocess,time
from pathlib import Path
assert os.getuid()==os.geteuid()==int(os.environ['GIT_ON_EXPECT_UID'])!=0
status=dict(line.split(':',1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
for name in ['CapEff','CapPrm','CapAmb']:assert int(status[name].strip(),16)==0,(name,status[name])
os.umask(0o027)
Path('out').mkdir()
preopen=Path('out/preopen')
os.mknod(preopen,0o100600)
stamp=946684800000000000
os.utime(preopen,ns=(stamp,stamp))
with preopen.open('rb') as first:
 s=os.fstat(first.fileno()); assert (s.st_atime_ns,s.st_mtime_ns)==(stamp,stamp)
preopen.unlink()
p=Path('out/generated')
with p.open('w+b') as f:
 old=946684800000000000
 os.utime(p,ns=(old,old))
 f.write(b'a'*131072); f.flush()
 os.fchmod(f.fileno(),0o640)  # no stat between bypass write and mode-only setattr
 s=p.stat(); assert s.st_size==131072 and s.st_mtime_ns>old and s.st_ctime_ns>=s.st_mtime_ns
 assert s.st_blocks>0 and s.st_mode & 0o777==0o640 and s.st_uid==os.getuid() and s.st_gid==os.getgid()
 # A simultaneous second handle must share coherent data and registration.
 with p.open('r+b',buffering=0) as second:
  second.seek(131072); second.write(b'extended')
  assert os.fstat(f.fileno()).st_size==131080
  f.seek(131072); assert f.read()==b'extended'
  second.truncate(131072)
 f.seek(0); assert f.read()==b'a'*131072
 with mmap.mmap(f.fileno(),0) as m:
  m[65536:65540]=b'mmap'; m.flush()
 assert p.stat().st_size==131072
assert p.read_bytes()[65536:65540]==b'mmap'
os.utime(p,ns=(old,old))
assert (p.stat().st_atime_ns,p.stat().st_mtime_ns)==(old,old)
# No descriptor is held here: path-based utime must reach the physical file
# after the daemon dropped its last cached open/passthrough handle.
with p.open('rb') as reopened:
 s=os.fstat(reopened.fileno()); assert (s.st_atime_ns,s.st_mtime_ns)==(old,old)
with p.open('r+b') as f:f.truncate(7)
s=p.stat(); assert s.st_size==7 and s.st_mtime_ns>old and 0<s.st_blocks<=256 and p.read_bytes()==b'a'*7
os.chmod(p,0o751); assert p.stat().st_mode & 0o777==0o751
# Exercise actual write -> LINK without stat or close in between. The core
# regression separately inspects the LINK reply before any later GETATTR.
linked_source=Path('out/link-before-stat')
linked_alias=Path('out/link-before-stat-alias')
with linked_source.open('w+b',buffering=0) as f:
 os.utime(f.fileno(),ns=(old,old))
 f.write(b'linked'*8192)
 os.link(linked_source,linked_alias)
 s=linked_alias.stat()
 assert s.st_size==6*8192 and s.st_blocks>0 and s.st_mtime_ns>old and s.st_nlink==2
 assert linked_alias.read_bytes()==b'linked'*8192
linked_alias.unlink(); linked_source.unlink()
# O_PATH pins the inode without sending FUSE OPEN. Metadata must survive both
# unlink and replacement after the last ordinary descriptor has closed.
for replace in [False,True]:
 target=Path('out/o-path-'+str(replace))
 target.write_bytes(b'o-path metadata')
 descriptor=os.open(target,os.O_PATH)
 try:
  expected=os.fstat(descriptor)
  if replace:
   replacement=Path('out/o-path-replacement'); replacement.write_bytes(b'replacement')
   os.replace(replacement,target)
  else:target.unlink()
  actual=os.fstat(descriptor)
  assert actual.st_nlink==0 and actual.st_ino==expected.st_ino
  assert actual.st_blocks>0 and (actual.st_size,actual.st_mtime_ns)==(expected.st_size,expected.st_mtime_ns)
 finally:os.close(descriptor)
 if replace:target.unlink()
before=p.stat().st_ctime_ns; time.sleep(.01)
os.link(p,'out/alias')
assert p.stat().st_ctime_ns>before and p.stat().st_nlink==2
Path('out/alias').unlink()
before=p.stat().st_ctime_ns; time.sleep(.01)
os.rename(p,'out/renamed'); p=Path('out/renamed')
assert p.stat().st_ctime_ns>before
with p.open('r+b') as f:
 f.truncate(8*1024*1024)
 with mmap.mmap(f.fileno(),0) as m:
  m[-4:]=b'last'; m.flush(); p.unlink()
  if hasattr(m,'madvise'):m.madvise(mmap.MADV_DONTNEED)
  assert m[-4:]==b'last'  # page fault after backing pathname unlink, daemon alive
subprocess.run(['cc','program.c','-o','out/program'],check=True)
assert subprocess.check_output(['./out/program'])==b'compiled\n'
print('ordinary child UID/zero capabilities, metadata, reopen, truncate, modes, rename/unlink, mmap and compiled executable verified')
'''
    def run_mount(code, policy):
        read_fd,write_fd=os.pipe()
        try:
            result=subprocess.run([str(args.binary.resolve()),'HEAD','--write-policy='+policy,'--',os.sys.executable,'-c',code],
                                  cwd=repo,env=dict(env,GIT_ON_TEST_METRICS_FD=str(write_fd)),pass_fds=[write_fd],
                                  capture_output=True,timeout=90)
        finally:
            os.close(write_fd)
        with os.fdopen(read_fd) as stream:
            assert result.returncode==0,(result.stdout,result.stderr)
            metrics=json.load(stream)
        return result.stdout,metrics

    output,metrics=run_mount(code,'ignored')
    if args.expect=='enabled':
        assert metrics['passthrough_handles']>0,metrics
        assert metrics['generated_read_callbacks']==0,metrics
        assert metrics['generated_write_callbacks']==0,metrics
    else:
        assert metrics['passthrough_handles']==0,metrics
        assert metrics['generated_write_callbacks']>0,metrics
    assert list(tmp.iterdir())==[],list(tmp.iterdir())
    assert subprocess.check_output(['git','status','--porcelain'],cwd=repo)==b''
    print(output.decode(),json.dumps(metrics,sort_keys=True))

    copied=r'''
import os,time
from pathlib import Path
atime=946684800000000000
os.utime('tracked-data',ns=(atime,atime))
# Copy up through an existing ordinary handle and read before releasing it.
fd=os.open('tracked-data',os.O_RDWR)
try:
    os.ftruncate(fd,65540)
    assert os.pread(fd,1,65536)==b't'
finally:
    os.close(fd)
before=os.stat('tracked-data')
backing=Path.cwd().parent / 'backing' / str(before.st_ino)
assert backing.stat().st_atime_ns==before.st_atime_ns==atime
time.sleep(.02)
fd=os.open('tracked-data',os.O_RDWR)
try:
    after=os.fstat(fd)
    assert after.st_atime_ns==before.st_atime_ns==atime
    assert after.st_mtime_ns==before.st_mtime_ns
    assert after.st_ctime_ns==before.st_ctime_ns
    assert os.pwrite(fd,b'copied',65530)==6
    assert os.pread(fd,12,65528)==b'ttcopiedtttt'
finally:
    os.close(fd)
'''
    _,metrics=run_mount(copied,'all')
    if args.expect=='enabled':
        assert metrics['passthrough_handles']>0,metrics
        assert metrics['fuse_requests'].get('write',0)==0,metrics
    else:
        assert metrics['passthrough_handles']==0,metrics
        assert metrics['fuse_requests'].get('write',0)>0,metrics
    assert (repo/'tracked-data').read_bytes()==b't'*(128*1024)
    assert list(tmp.iterdir())==[],list(tmp.iterdir())
    print('copied-up tracked file',json.dumps(metrics,sort_keys=True))
