"""Compare clone + command and Git-on under the same private tmpfs quota."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


# Shared child-side accounting for mounted runs and the native checkout oracle.
STORAGE_OBSERVATION=r'''
import os
from pathlib import Path
root=Path.cwd() if Path('.git').exists() else Path.cwd().parent
if root!=Path.cwd():assert os.path.ismount('.'),'real mount required'
def allocated():
 while True:
  try:return _allocated()
  except FileNotFoundError:
   # FUSE may reclaim source files after close. Rescan instead of dropping
   # the entry: a renamed payload must still count at its new location.
   pass
def _allocated():
 total=0;seen=set()
 for directory,dirs,files in os.walk(root):
  if root!=Path.cwd() and Path(directory)==root:
   dirs[:]=[name for name in dirs if root/name!=Path.cwd()]
  for name in dirs+files:
   st=(Path(directory)/name).lstat();key=(st.st_dev,st.st_ino)
   if key not in seen:total+=st.st_blocks*512;seen.add(key)
 return total
'''


def private_mount_namespace(test):
    unshare=shutil.which('unshare')
    if not unshare:
        if os.environ.get('GITHUB_ACTIONS')=='true':test.fail('unshare is required for mount namespace coverage')
        test.skipTest('unshare is unavailable')
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
    return prefix, uid, gid


def compare_on_tmpfs(test, root, binary, repo, programs, command, size_mib=9, source=None,
                     write_policy='all', git_config=None, checkout_copies=1, tls_ca=None,
                     revision='HEAD', runner_size_mib=None, service_on_volume=False):
    runner_size_mib = size_mib if runner_size_mib is None else runner_size_mib
    assert 0 < runner_size_mib <= size_mib
    prefix, uid, gid = private_mount_namespace(test)
    volume=root/'quota';volume.mkdir(exist_ok=True)
    test.assertEqual(list(volume.iterdir()),[],'quota mountpoint must be empty')
    driver=r'''
from contextlib import ExitStack
import json,os,shutil,subprocess,sys,tempfile
from pathlib import Path
volume,binary,repo,mount,programs,uid,gid,command,size,runner_size,source,policy,config,copies,tls_ca,revision,service_on_volume=sys.argv[1:]
uid,gid,size,runner_size=map(int,(uid,gid,size,runner_size));volume=Path(volume);command=json.loads(command)
subprocess.run([mount,'-t','tmpfs','-o',f'size={size}m,mode=700,uid={uid},gid={gid}','tmpfs',str(volume)],check=True)
if os.geteuid()==0 and uid!=0:
 os.setgroups([]);os.setgid(gid);os.setuid(uid)
temporary=volume/'tmp';temporary.mkdir()
home=volume/'home';home.mkdir()
env={k:v for k,v in os.environ.items() if not k.startswith('GIT_')}
env.update(PATH=programs,TMPDIR=str(temporary),HOME=str(home),XDG_CONFIG_HOME=str(home/'config'),
           GIT_CONFIG_GLOBAL='/dev/null',GIT_CONFIG_NOSYSTEM='1')
if tls_ca:env['GIT_SSL_CAINFO']=tls_ca
config=json.loads(config)
env['GIT_CONFIG_COUNT']=str(len(config))
for index,(key,value) in enumerate(config.items()):
 env[f'GIT_CONFIG_KEY_{index}']=key;env[f'GIT_CONFIG_VALUE_{index}']=value
clone=volume/'clone'
before=os.statvfs(volume).f_bavail*os.statvfs(volume).f_frsize
assert before<=size*1024*1024,before
result=subprocess.run(['git','clone','--no-local','--quiet',source or repo,str(clone)],env=env,capture_output=True)
assert result.returncode==0,result.stderr
if revision!='HEAD':
 selected=subprocess.check_output(['git','rev-parse','--verify',revision+'^{commit}'],cwd=clone,env=env)
 checked_out=subprocess.check_output(['git','rev-parse','--verify','HEAD^{commit}'],cwd=clone,env=env)
 assert checked_out==selected,'native comparator checked out a different revision'
spare=volume/'second-checkout'
if copies=='2':shutil.copytree(clone,spare,symlinks=True,ignore=shutil.ignore_patterns('.git'))
direct=subprocess.run(command,cwd=clone,env=env,capture_output=True)
assert direct.returncode==0,direct.stderr
print('native clone + command passed on tmpfs',flush=True)
shutil.rmtree(clone)
if copies=='2':shutil.rmtree(spare)
assert os.statvfs(volume).f_bavail*os.statvfs(volume).f_frsize==before,'baseline did not restore initial free space'
operands=[source,revision] if source else [revision]
if service_on_volume=='1':env['GIT_ON_SERVICE_TMPDIR']=str(temporary)
with ExitStack() as stack:
 if runner_size<size:
  # Keep the smaller budget in force during the run, including short-lived files.
  reservation=stack.enter_context(tempfile.TemporaryFile(dir=volume))
  os.posix_fallocate(reservation.fileno(),0,(size-runner_size)*1024*1024)
 result=subprocess.run([binary,*operands,'--write-policy='+policy,'--',*command],cwd=repo,env=env,capture_output=True)
assert result.returncode==0,(result.returncode,result.stderr)
assert result.stdout==direct.stdout
assert list(temporary.iterdir())==[]
print('git-on + command passed on tmpfs',flush=True)
'''
    try:
        result=subprocess.run([*prefix,sys.executable,'-I','-S','-c',driver,str(volume),str(binary),
                               str(repo),shutil.which('mount'),programs,str(uid),str(gid),
                               json.dumps(command),str(size_mib),str(runner_size_mib),source or '',write_policy,
                               json.dumps(git_config or {}),str(checkout_copies),str(tls_ca or ''),revision,
                               str(int(service_on_volume))],
                              capture_output=True,timeout=30)
    except subprocess.TimeoutExpired as error:
        raise AssertionError(
            f'tmpfs comparison timed out with stdout={error.stdout!r} and stderr={error.stderr!r}'
        ) from None
    test.assertEqual(result.returncode,0,result.stderr)
    test.assertIn(b'git-on + command passed on tmpfs',result.stdout)
