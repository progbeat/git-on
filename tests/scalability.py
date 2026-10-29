#!/usr/bin/env python3
"""Cross-backend scale acceptance. No numerical SLA or repository-size ceiling.

The full traversal fixture deliberately reuses objects, separately from the
existing million-distinct-blob lazy test. Every logical leaf is read and checked.
Run one matrix cell per CI runner; preparation and clone are outside runner timing.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import subprocess
import sys
import threading
import time

PARSER=argparse.ArgumentParser()
PARSER.add_argument('--binary',type=Path,required=True)
PARSER.add_argument('--backend',choices=['fuse','materialized','passthrough'],required=True)
PARSER.add_argument('--source',choices=['local','remote'],required=True)
PARSER.add_argument('--policies',default='none,ignored,untracked,all')
PARSER.add_argument('--verification',choices=['both','unsigned','signed'],default='both')
PARSER.add_argument('--sizes',default='1000,10000,1000000')
PARSER.add_argument('--output',type=Path,default=Path('scale-results.jsonl'))
OPTIONS=None

CHILD=r'''
import time
start=time.monotonic_ns()
import os,json,sys
print(json.dumps({'child_started_ns':start}),flush=True)
mode,count,depth=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
if mode=='noop': pass
elif mode=='sparse':
 for _ in range(100):
  path='/'.join('0'*depth)
  assert os.stat(path).st_size==8
  with open(path,'rb') as file: assert file.read()==b'payload\n'
else:
 actual=0
 if 'GIT_ON_TREE_OID' in os.environ: assert not os.path.exists('.git')
 for base,dirs,files in os.walk('.'):
  dirs[:]=[x for x in dirs if x not in ('out','.git')]
  for name in files:
   path=os.path.join(base,name)
   with open(path,'rb') as file: data=file.read()
   if path=='./.gitignore': assert data==b'out/\n'
   else:
    assert len(path[2:].split('/'))==depth,path
    assert all(len(part)==1 and part in '0123456789' for part in path[2:].split('/')),path
    assert data==b'payload\n',path
    actual+=1
 assert actual==count,(actual,count)
 if len(sys.argv)>4 and sys.argv[4]=='write':
  os.mkdir('out')
  with open('out/generated','w+b') as file:
   file.write(b'generated');file.flush();file.seek(0);assert file.read()==b'generated'
print(json.dumps({'completed':mode,'count':count,'duration_ns':time.monotonic_ns()-start}),flush=True)
'''

def record(value):
    print(json.dumps(value),flush=True)
    with OPTIONS.output.open('a') as output: output.write(json.dumps(value)+'\n')

def fixture(f,count,key):
    depth=len(str(count))-1
    assert 10**depth==count
    blob=f.git('hash-object','-w','--stdin',data=b'payload\n')
    ignore=f.git('hash-object','-w','--stdin',data=b'out/\n')
    child=blob; kind='blob'; mode='100644'; trees=[]
    for level in range(depth):
        rows=''.join(f'{mode} {kind} {child}\t{x}\n' for x in range(10))
        if level==depth-1: rows=f'100644 blob {ignore}\t.gitignore\n'+rows
        child=f.git('mktree',data=rows.encode()); trees.append(child); kind='tree'; mode='040000'
    # Sign every fixture; each matrix cell exercises both verified/unverified.
    signed=f.git('-c','gpg.format=ssh','-c',f'user.signingkey={key}','commit-tree','-S',child,data=b'scale fixture\n')
    f.git('update-ref','refs/heads/main',signed)
    return dict(count=count,depth=depth,tree=child,commit=signed,distinct_objects=depth+3,
        distinct_blobs=2,leaf_bytes=8,logical_payload_bytes=count*8+5,
        modes={'leaves':'100644','directories':'040000'},shape='ten-way digit paths; shared subtrees and one shared leaf blob',
        git_objects=[signed,*trees,blob,ignore])

def sample_resources(pid,root,available,samples):
    try:
        if sys.platform=='linux':
            samples['runner_peak_descriptors']=max(samples['runner_peak_descriptors'] or 0,len(list(Path(f'/proc/{pid}/fd').iterdir())))
            for row in Path(f'/proc/{pid}/status').read_text().splitlines():
                if row.startswith('VmHWM:'): samples['runner_peak_resident_kib']=max(samples['runner_peak_resident_kib'] or 0,int(row.split()[1]))
    except PermissionError: samples['runner_observation_denied']=True
    except (FileNotFoundError,ProcessLookupError): pass
    # /proc access can be denied for a capability-bearing daemon while its
    # host filesystem remains observable. Neither sample gates the other.
    try:
        space=os.statvfs(root)
        samples['filesystem_allocated_delta_bytes']=max(samples['filesystem_allocated_delta_bytes'],available-space.f_bavail*space.f_frsize)
    except PermissionError: samples['filesystem_observation_denied']=True
    except (FileNotFoundError,ProcessLookupError): pass

def execute(f,argv,env,description,metrics=False):
    read_fd,write_fd=os.pipe()
    if metrics: env=dict(env,GIT_ON_TEST_METRICS_FD=str(write_fd))
    started=time.monotonic_ns(); stop=threading.Event(); samples={'runner_peak_descriptors':None,'runner_peak_resident_kib':None,'filesystem_allocated_delta_bytes':0,'runner_observation_denied':False,'filesystem_observation_denied':False}
    available=os.statvfs(f.root).f_bavail*os.statvfs(f.root).f_frsize
    process=subprocess.Popen(argv,cwd=f.repo,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,pass_fds=(write_fd,) if metrics else ())
    os.close(write_fd)
    def sample():
        while not stop.wait(.02):
            if time.monotonic_ns()-started>3_600_000_000_000:
                process.terminate()
                return
            sample_resources(process.pid,f.root,available,samples)
    monitor=threading.Thread(target=sample);monitor.start()
    first=process.stdout.readline()
    _,status,usage=os.wait4(process.pid,0)
    process.returncode=os.waitstatus_to_exitcode(status)
    stop.set();monitor.join()
    output=first+process.stdout.read();errors=process.stderr.read()
    with os.fdopen(read_fd) as pipe: observed=pipe.read()
    assert process.returncode==0,(description,process.returncode,output,errors)
    if description.get('verify'):
        assert b'Good \"git\" signature for fixture@example.invalid' in errors,(description,errors)
    else: assert errors==b'',(description,errors)
    rows=[json.loads(x) for x in output.splitlines()]
    assert rows[-1]['completed']==description['workload'],rows
    peak=usage.ru_maxrss/(1024 if sys.platform=='darwin' else 1)
    result=dict(description,elapsed_ns=time.monotonic_ns()-started,child_start_ns=rows[0]['child_started_ns']-started,
        child_duration_ns=rows[-1]['duration_ns'],runner_and_waited_children_peak_resident_kib=peak,**samples)
    if metrics:
        result['metrics']=json.loads(observed)
        m=result['metrics']
        validate_lazy_noop(m,description)
        if description['backend']=='passthrough' and description['workload']=='full':
            assert m['passthrough_handles']>0,m
    record(result)
    return result

def validate_lazy_noop(metrics,description):
    if description['backend']!='materialized' and description['workload']=='noop':
        # Python startup imports/stat checks can look up names in cwd, decoding
        # the root directory. They must not read any subtree or payload.
        assert metrics['tree_reads']<=1 and metrics['blob_loads']==0,metrics


def main():
    global OPTIONS
    OPTIONS=PARSER.parse_args();OPTIONS.binary=OPTIONS.binary.resolve()
    from remote import Fixture
    OPTIONS.output.parent.mkdir(parents=True,exist_ok=True)
    f=Fixture()
    try:
        native_git=shutil.which('git')
        key=f.root/'scale-signing-key'
        subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
        allowed=f.root/'allowed-signers';allowed.write_text('fixture@example.invalid '+key.with_suffix('.pub').read_text())
        f.env.update(GIT_CONFIG_COUNT='2',GIT_CONFIG_KEY_0='gpg.format',GIT_CONFIG_VALUE_0='ssh',
            GIT_CONFIG_KEY_1='gpg.ssh.allowedSignersFile',GIT_CONFIG_VALUE_1=str(allowed))
        # Force only the requested backend. A missing real-FUSE prerequisite is
        # an error, never a skip or an implicit materialization pass.
        env=dict(f.env)
        if OPTIONS.backend=='materialized':
            tools=f.root/'git-only';tools.mkdir();(tools/'git').symlink_to(native_git);(tools/'ssh-keygen').symlink_to(shutil.which('ssh-keygen'));env['PATH']=str(tools)
            assert OPTIONS.policies=='all','materialization supports only explicit all'
        else:
            assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3')
        source=f.https() if OPTIONS.source=='remote' else None
        env.update({k:v for k,v in f.env.items() if k not in ('PATH',)})
        record(dict(type='environment',revision=subprocess.check_output(['git','rev-parse','HEAD']).decode().strip(),
            platform=platform.platform(),machine=platform.machine(),cpu=platform.processor(),cpu_count=os.cpu_count(),
            physical_memory_bytes=os.sysconf('SC_PHYS_PAGES')*os.sysconf('SC_PAGE_SIZE'),
            rlimit_nofile=resource.getrlimit(resource.RLIMIT_NOFILE),backend=OPTIONS.backend,
            source=OPTIONS.source,verification=OPTIONS.verification,storage='fixture and private per-invocation ODB on host temporary filesystem',
            measurement_limits='filesystem space delta is host-wide and sampled; Linux runner /proc samples exclude subprocesses; wait4 peak includes waited descendants; macOS runner-only descriptors/RSS unavailable'))
        for count in map(int,OPTIONS.sizes.split(',')):
            info=fixture(f,count,key);record(dict(type='fixture',**info))
            before=f.git('show-ref')
            clone=f.root/'clone'
            begin=time.monotonic_ns()
            subprocess.run([native_git,'clone','--quiet','--no-hardlinks',source or str(f.repo),str(clone)],env=f.env,check=True)
            clone_time=time.monotonic_ns()-begin
            # Direct execution is timed separately from clone technical success.
            original=f.repo;f.repo=clone
            for workload in ('noop','sparse','full'):
                execute(f,[sys.executable,'-c',CHILD,workload,str(count),str(info['depth'])],f.env,
                    dict(type='direct-baseline',backend='disk',source='clone',workload=workload,files=count,clone_ns=clone_time))
            f.repo=original;shutil.rmtree(clone)
            for policy in OPTIONS.policies.split(','):
                for verify in {'both':(False,True),'unsigned':(False,),'signed':(True,)}[OPTIONS.verification]:
                    for workload in ('noop','sparse','full'):
                        first_demands=None
                        for invocation in (('first','repeat') if source else ('local',)):
                            first_request=len(f.requests)
                            command=[str(OPTIONS.binary),*([source] if source else []),'main',f'--write-policy={policy}',*(['--verify'] if verify else []),'--',sys.executable,'-c',CHILD,workload,str(count),str(info['depth']),*(['write'] if OPTIONS.backend=='passthrough' and workload=='full' else [])]
                            result=execute(f,command,env,dict(type='runner',backend=OPTIONS.backend,source=OPTIONS.source,policy=policy,verify=verify,invocation=invocation,workload=workload,files=count),True)
                            requests=f.requests[first_request:];wants={oid for request in requests for oid in request.get('wants',[])}
                            assert result['metrics']['remote_fetches']==len(wants),result
                            if source:
                                if first_demands is None: first_demands=wants
                                else: assert wants==first_demands,(first_demands,wants)
                            assert not (f.root/'cache').exists(),'persistent cache created'
                            assert wants<=set(info['git_objects']),wants
                            record(dict(type='transport',files=count,workload=workload,policy=policy,verify=verify,invocation=invocation,
                                initial_resolution_fetches=sum(info['commit'] in r.get('wants',[]) for r in requests),
                                exact_object_fetches=sum(bool(r.get('wants')) and info['commit'] not in r['wants'] for r in requests),
                                response_bytes=sum(r.get('response_bytes',0) for r in requests),http_requests=len(requests),objects=sorted(wants)))
                            assert not list(f.root.glob('git-on-*')),'run cleanup failed'
                            assert f.git('show-ref')==before,'source refs changed'
    finally:
        if hasattr(f,'server'): f.server.shutdown()
        f.close()

if __name__=='__main__': main()
