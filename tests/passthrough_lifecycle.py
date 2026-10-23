#!/usr/bin/env python3
"""Observe Linux mmap dependencies using only disposable mounts owned by this test.

This deliberately compares normal unmount, test-only lazy detach and daemon
SIGKILL. It adds no product execution mode and grants no capabilities. Run the
coherence/bypass test first; --expect=enabled requires a provisioned test binary.
Outcomes after detach/daemon exit are observations, never a safety guarantee.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def wait_file(path, process, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if path.is_file():
            return json.loads(path.read_text())
        if process.poll() is not None:
            raise AssertionError(f'{path.name} missing; process exited {process.returncode}')
        time.sleep(.02)
    raise AssertionError(f'timed out waiting for {path}')


def mounts_below(root):
    # mkdtemp paths have no mountinfo escape characters.
    return [line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()
            if line.split()[4].startswith(str(root) + '/')]


DRIVER = r'''
import json,os,time
from pathlib import Path
control=Path(os.environ['LIFECYCLE_CONTROL'])
assert os.getuid()==os.geteuid()==int(os.environ['LIFECYCLE_UID'])!=0
status=dict(line.split(':',1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
for name in ['CapEff','CapPrm','CapAmb']:assert int(status[name].strip(),16)==0
(control/'driver-ready').write_text(json.dumps({'pid':os.getpid(),'mount':str(Path.cwd())}))
deadline=time.monotonic()+90
while not (control/'stop-driver').exists() and time.monotonic()<deadline:time.sleep(.02)
(control/'driver-done').write_text('{}')
'''

WORKER = r'''
import json,mmap,os,time
from pathlib import Path
control=Path(os.environ['LIFECYCLE_CONTROL'])
assert os.getuid()==os.geteuid()==int(os.environ['LIFECYCLE_UID'])!=0
status=dict(line.split(':',1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
for name in ['CapEff','CapPrm','CapAmb']:assert int(status[name].strip(),16)==0
Path('out').mkdir()
p=Path('out/generated')
f=p.open('w+b',buffering=0)
f.truncate(8*1024*1024)
f.seek(8*1024*1024-4)
f.write(b'last')
m=mmap.mmap(f.fileno(),0)
m.flush()
ino=p.stat().st_ino
p.unlink()
(control/'worker-ready').write_text(json.dumps({'inode':ino,'uid':os.geteuid()}))
deadline=time.monotonic()+60
while not (control/'fault-now').exists() and time.monotonic()<deadline:time.sleep(.02)
assert (control/'fault-now').exists()
# Discard populated pages before the read so this is a subsequent page fault.
result={}
try:
 m.madvise(mmap.MADV_DONTNEED)
 result['mmap_after_discard']=m[-4:].decode()
except OSError as error:result['mmap_errno']=error.errno
try:
 f.seek(-4,os.SEEK_END)
 result['descriptor_read']=f.read().decode()
except OSError as error:result['descriptor_errno']=error.errno
# Save the cached-tail/descriptor observation even if the untouched sparse
# page subsequently faults with SIGBUS after the daemon has exited.
(control/'worker-result').write_text(json.dumps(result))
result['unread_sparse_page']=m[4*1024*1024:4*1024*1024+4].hex()
(control/'worker-result').write_text(json.dumps(result))
os._exit(0)  # Avoid a disconnected FUSE flush obscuring the recorded observation.
'''


def scenario(binary, expected, mode):
    root = Path(tempfile.mkdtemp(prefix='git-on-passthrough-lifecycle-')).resolve()
    repo = root / 'repo'; repo.mkdir()
    temporary = root / 'tmp'; temporary.mkdir()
    control = root / 'control'; control.mkdir()
    environment = dict(os.environ, TMPDIR=str(temporary), LIFECYCLE_CONTROL=str(control),
                       LIFECYCLE_UID=str(os.getuid()), GIT_AUTHOR_NAME='Fixture',
                       GIT_COMMITTER_NAME='Fixture', GIT_AUTHOR_EMAIL='f@example.invalid',
                       GIT_COMMITTER_EMAIL='f@example.invalid')
    def git(*argv):
        return subprocess.check_output(['git', *argv], cwd=repo, env=environment)
    git('init', '-q'); (repo / '.gitignore').write_text('out/\n')
    git('add', '.'); git('commit', '-qm', 'fixture')
    runner = worker = None
    run_directory = None
    metrics_fd = None
    metrics_path = root / 'metrics.json'
    logs = (root / 'runner.log').open('wb')
    success = False
    try:
        metrics_fd = os.open(metrics_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        runner = subprocess.Popen([str(binary), 'HEAD', '--', sys.executable, '-c', DRIVER],
                                  cwd=repo, env=dict(environment, GIT_ON_TEST_METRICS_FD=str(metrics_fd)),
                                  pass_fds=[metrics_fd], stdout=logs, stderr=logs)
        os.close(metrics_fd); metrics_fd = None
        driver = wait_file(control / 'driver-ready', runner)
        mount = Path(driver['mount'])
        run_directory = mount.parent
        assert run_directory.parent == temporary and mount.name == 'mount'
        assert str(mount) in mounts_below(root), 'real FUSE mount required'
        worker = subprocess.Popen([sys.executable, '-c', WORKER], cwd=mount,
                                  env=environment, stdout=logs, stderr=logs)
        ready = wait_file(control / 'worker-ready', worker)
        # FUSE unlink must have removed the actual backing name as well.
        assert not (run_directory / 'backing' / str(ready['inode'])).exists()
        record = {'case': mode, 'kernel': os.uname().release, 'expect': expected,
                  'backing_name_unlinked': True, 'worker_uid': ready['uid']}
        if mode == 'daemon-exit':
            runner.kill(); runner.wait(timeout=10)
            record['daemon_status'] = runner.returncode
        else:
            option = '-u' if mode == 'normal-unmount' else '-uz'
            unmount = subprocess.run(['fusermount3', option, str(mount)],
                                     capture_output=True, timeout=15)
            record['unmount_status'] = unmount.returncode
            record['unmount_stderr'] = unmount.stderr.decode(errors='replace').strip()
            if mode == 'normal-unmount':
                assert unmount.returncode != 0, 'normal unmount unexpectedly succeeded with held cwd/mmap'
                assert str(mount) in mounts_below(root)
            else:
                assert unmount.returncode == 0, record
                assert str(mount) not in mounts_below(root)
        record['namespace_mount_present'] = str(mount) in mounts_below(root)
        record['temporary_state_still_present'] = run_directory.exists()
        (control / 'fault-now').touch()
        worker.wait(timeout=15)
        record['worker_status_after_fault'] = worker.returncode
        if (control / 'worker-result').exists():
            record.update(json.loads((control / 'worker-result').read_text()))
        if worker.returncode != 0:
            assert worker.returncode in [-signal.SIGBUS, -signal.SIGSEGV], record
        if mode == 'normal-unmount':
            assert worker.returncode == 0 and record['mmap_after_discard'] == 'last' and record['unread_sparse_page'] == '00000000', record
        (control / 'stop-driver').touch()
        if mode == 'daemon-exit':
            # The deliberately killed runner cannot reap its command. Let the
            # ordinary child leave its cwd itself before checked recovery.
            deadline = time.monotonic() + 10
            while not (control / 'driver-done').exists() and time.monotonic() < deadline:
                time.sleep(.02)
            assert (control / 'driver-done').exists(), 'command did not leave after daemon exit'
            # The marker precedes Python teardown; wait for the held cwd to
            # actually close before trying the normal recovery unmount.
            while time.monotonic() < deadline:
                try: cwd = os.readlink(f"/proc/{driver['pid']}/cwd")
                except FileNotFoundError: break
                if cwd != str(mount): break
                time.sleep(.02)
            else: raise AssertionError('command still holds mount after shutdown')
        else:
            runner.wait(timeout=20)
            record['runner_status'] = runner.returncode
            metrics = json.loads(metrics_path.read_text())
            assert (metrics['passthrough_handles'] > 0) == (expected == 'enabled'), metrics
            if expected == 'enabled':
                assert metrics['generated_read_callbacks'] == metrics['generated_write_callbacks'] == 0, metrics
            record['passthrough_handles'] = metrics['passthrough_handles']
            if mode == 'normal-unmount':
                assert runner.returncode == 0, (root / 'runner.log').read_text()
        if run_directory.exists():
            recovery = subprocess.run([str(binary), '--cleanup', str(run_directory)],
                                      cwd=repo, env=environment, capture_output=True, timeout=20)
            assert recovery.returncode == 0, recovery.stderr
        assert not mounts_below(root) and not list(temporary.iterdir())
        assert git('status', '--porcelain') == b''
        assert git('for-each-ref', '--format=%(refname)', 'refs/git-on/') == b''
        record['checked_cleanup'] = True
        print(json.dumps(record, sort_keys=True), flush=True)
        success = True
    finally:
        if metrics_fd is not None: os.close(metrics_fd)
        (control / 'fault-now').touch(); (control / 'stop-driver').touch()
        if worker is not None and worker.poll() is None:
            worker.kill(); worker.wait(timeout=10)
        if runner is not None and runner.poll() is None:
            runner.terminate()
            try: runner.wait(timeout=10)
            except subprocess.TimeoutExpired: runner.kill(); runner.wait(timeout=10)
        logs.close()
        if success:
            shutil.rmtree(root)
        else:
            # Never let generic recursive deletion descend into a live mount.
            print('Lifecycle experiment failed; preserving owned state and logs at ' + str(root), file=sys.stderr)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--expect', choices=['enabled', 'fallback'], required=True)
    arguments = parser.parse_args()
    if sys.platform != 'linux' or os.getuid() == 0 or not Path('/dev/fuse').exists() or not shutil.which('fusermount3'):
        parser.error('ordinary Linux user and usable FUSE required')
    for case in ['normal-unmount', 'lazy-detach-experiment', 'daemon-exit']:
        scenario(arguments.binary.resolve(), arguments.expect, case)
