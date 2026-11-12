"""Compare clone + command and Git-on under the same private tmpfs quota."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def compare_on_tmpfs(test, root, binary, repo, programs, command, size_mib=9):
    unshare=shutil.which('unshare')
    if not unshare:test.skipTest('unshare is unavailable')
    prefix=[unshare,'--user','--map-root-user','--mount','--propagation','private']
    probe=subprocess.run([*prefix,'/bin/true'],capture_output=True)
    uid,gid=0,0
    if probe.returncode:
        sudo=shutil.which('sudo')
        prefix=([sudo,'-n'] if os.geteuid()!=0 and sudo else [])+[
            unshare,'--mount','--propagation','private']
        probe=subprocess.run([*prefix,'/bin/true'],capture_output=True)
        if probe.returncode:
            reason='mount namespaces unavailable: '+probe.stderr.decode().strip()
            if os.environ.get('GITHUB_ACTIONS')=='true':test.fail(reason)
            test.skipTest(reason)
        uid,gid=os.getuid(),os.getgid()
    volume=root/'quota';volume.mkdir()
    driver=r'''
import json,os,shutil,subprocess,sys
from pathlib import Path
volume,binary,repo,mount,programs,uid,gid,command,size=sys.argv[1:]
uid,gid,size=int(uid),int(gid),int(size);volume=Path(volume);command=json.loads(command)
subprocess.run([mount,'-t','tmpfs','-o',f'size={size}m,mode=700,uid={uid},gid={gid}','tmpfs',str(volume)],check=True)
if os.geteuid()==0 and uid!=0:
 os.setgroups([]);os.setgid(gid);os.setuid(uid)
temporary=volume/'tmp';temporary.mkdir()
home=volume/'home';home.mkdir()
env={k:v for k,v in os.environ.items() if not k.startswith('GIT_')}
env.update(PATH=programs,TMPDIR=str(temporary),HOME=str(home),XDG_CONFIG_HOME=str(home/'config'),
           GIT_CONFIG_GLOBAL='/dev/null',GIT_CONFIG_NOSYSTEM='1')
clone=volume/'clone'
before=os.statvfs(volume).f_bavail*os.statvfs(volume).f_frsize
assert before<=size*1024*1024,before
result=subprocess.run(['git','clone','--no-local','--quiet',repo,str(clone)],env=env,capture_output=True)
assert result.returncode==0,result.stderr
direct=subprocess.run(command,cwd=clone,env=env,capture_output=True)
assert direct.returncode==0,direct.stderr
print('native clone + command passed on tmpfs',flush=True)
shutil.rmtree(clone)
assert os.statvfs(volume).f_bavail*os.statvfs(volume).f_frsize==before,'baseline did not restore initial free space'
result=subprocess.run([binary,'HEAD','--write-policy=all','--',*command],cwd=repo,env=env,capture_output=True)
assert result.returncode==0,(result.returncode,result.stderr)
assert result.stdout==direct.stdout
assert list(temporary.iterdir())==[]
print('git-on + command passed on tmpfs',flush=True)
'''
    result=subprocess.run([*prefix,sys.executable,'-I','-S','-c',driver,str(volume),str(binary),
                           str(repo),shutil.which('mount'),programs,str(uid),str(gid),
                           json.dumps(command),str(size_mib)],capture_output=True,timeout=30)
    test.assertEqual(result.returncode,0,result.stderr)
    test.assertIn(b'git-on + command passed on tmpfs',result.stdout)
