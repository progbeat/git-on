#!/usr/bin/env python3
"""Pre-merge behavior matrix and selective scale controls, with explicit phases.

A million-path namespace traversal is distinct from reading every payload. The
matrix uses smaller physical trees; exhaustive is an opt-in diagnostic profile.
Every invocation owns its own snapshot/ODB. No product size limit is introduced.
"""
import argparse
import json
import math
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
PARSER.add_argument('--profile',choices=['matrix','namespace','exhaustive'],default='matrix')
PARSER.add_argument('--sizes',help='override profile file counts; powers of ten')
PARSER.add_argument('--output',type=Path,default=Path('scale-results.jsonl'))
OPTIONS=None

CHILD=r'''
import time
start=time.monotonic_ns()
import os,json,sys
print(json.dumps({'child_started_ns':start}),flush=True)
mode,count,fanouts=sys.argv[1],int(sys.argv[2]),json.loads(sys.argv[3])
widths=[len(str(n-1)) for n in fanouts]
payloads=[f'{n:07d}\n'.encode() for n in range(16)]
path='/'.join('0'*width for width in widths)
reads=0;visited=None;phases={};generated_write=False
def sparse():
 global reads
 for _ in range(100):
  assert os.stat(path).st_size==8
  with open(path,'rb') as file: assert file.read()==payloads[0]
  reads+=1
if mode=='sparse': sparse()
elif mode in ('full','enumerate'):
 if 'GIT_ON_TREE_OID' in os.environ: assert not os.path.exists('.git')
 assert not os.path.lexists('out'),'previous case leaked generated files'
 if mode in ('full','enumerate'):
  begin=time.monotonic_ns();sparse();phases['repeated_sparse_ns']=time.monotonic_ns()-begin
  with open('.gitignore','rb') as file: assert file.read()==b'out/\n'
  reads+=1
 begin=time.monotonic_ns();actual=0
 for base,dirs,files in os.walk('.'):
  parts=[] if base=='.' else base[2:].split('/')
  level=len(parts); assert level<len(fanouts),(base,level)
  # Only the direct clone has legitimate root Git metadata. Snapshot metadata
  # is forbidden above; every other directory must match the pinned namespace.
  if not parts and 'GIT_ON_TREE_OID' not in os.environ and '.git' in dirs: dirs.remove('.git')
  expected={f'{n:0{widths[level]}d}' for n in range(fanouts[level])}
  expected_files={'.gitignore'} if not parts else set()
  if level==len(fanouts)-1:
   assert not dirs,(base,dirs)
   expected_files |= expected
  else:
   assert len(dirs)==len(expected) and set(dirs)==expected,(base,dirs)
  assert len(files)==len(expected_files) and set(files)==expected_files,(base,files)
  if level==len(fanouts)-1:
   actual+=len(expected)
   if mode=='full':
    variant=sum(map(int,parts))%2
    for name in files:
     if name=='.gitignore':continue
     with open(os.path.join(base,name),'rb') as file: assert file.read()==payloads[(int(name)+variant)%16]
     reads+=1
   else:
    # Visit every name, but sample one payload in each leaf directory. This
    # keeps the million-path namespace control distinct from a full read.
    total=sum(map(int,parts));index=(total*17+total//2)%fanouts[-1]
    name=f'{index:0{widths[-1]}d}'
    with open(os.path.join(base,name),'rb') as file: assert file.read()==payloads[(index+total%2)%16]
    reads+=1
 assert actual==count,(actual,count)
 visited=actual;phases['walk_ns']=time.monotonic_ns()-begin
 if len(sys.argv)>4 and sys.argv[4]=='write':
  os.mkdir('out')
  with open('out/generated','w+b') as file:
   file.write(b'generated');file.flush();os.fsync(file.fileno());file.seek(0);assert file.read()==b'generated'
  generated_write=True
print(json.dumps({'completed':mode,'count':count,'visited_leaves':visited,'payload_reads':reads,'generated_write':generated_write,'phases':phases,'duration_ns':time.monotonic_ns()-start}),flush=True)
'''

def record(value):
    print(json.dumps(value),flush=True)
    with OPTIONS.output.open('a') as output: output.write(json.dumps(value)+'\n')


def scenario_plan(backend,source,profile,policies,verification,sizes=None):
    modes={'both':(False,True),'unsigned':(False,),'signed':(True,)}[verification]
    # WebKit supplies the principal real scale/complete namespace and payload
    # control. The generated matrix preserves every behavior interaction once.
    counts=sizes or ([1_000_000] if profile=='namespace' else [1000] if profile=='matrix' else [1000,10000,1000000])
    cases=[]
    for count in counts:
        selected_policies=policies
        selected_modes=modes
        if profile=='namespace' or (profile=='matrix' and count>10000):
            selected_policies=['ignored' if 'ignored' in policies else policies[0]]
            assert selected_policies[0] in policies
            selected_modes=(True,) if True in modes else (False,)
        workloads=('enumerate',) if profile=='namespace' else ('noop','sparse','full') if profile=='exhaustive' or (backend=='fuse' and count<=1000) else ('full',)
        for policy in selected_policies:
            for verify in selected_modes:
                for workload in workloads:
                    invocations=('first','repeat') if source=='remote' and profile=='exhaustive' else ('first',) if source=='remote' else ('local',)
                    for invocation in invocations:
                        cases.append(dict(files=count,policy=policy,verify=verify,workload=workload,invocation=invocation))
    return cases


def fixture_shape(count):
    assert count>=1 and 10**(len(str(count))-1)==count,'fixture counts must be powers of ten'
    fanouts=[];remaining=count
    while remaining>1000:
        fanouts.append(100);remaining//=100
    if not fanouts and remaining>=100:
        fanouts.append(10);remaining//=10
    return [*fanouts,remaining]


def fixture(f,count,key):
    fanouts=fixture_shape(count);widths=[len(str(n-1)) for n in fanouts]
    blobs={};trees=[];selected=[];children=None
    def blob(value):
        if value not in blobs: blobs[value]=f.git('hash-object','-w','--stdin',data=f'{value:07d}\n'.encode())
        return blobs[value]
    ignore=f.git('hash-object','-w','--stdin',data=b'out/\n')
    for level in range(len(fanouts)-1,-1,-1):
        current=[]
        for variant in range(1 if level==0 else 2):
            entries=[]
            if level==0:entries.append(f'100644 blob {ignore}\t.gitignore\n')
            for index in range(fanouts[level]):
                name=f'{index:0{widths[level]}d}'
                if children is None:oid=blob((index+variant)%16);kind='blob';mode='100644'
                else:oid=children[(index+variant)%2];kind='tree';mode='040000'
                entries.append(f'{mode} {kind} {oid}\t{name}\n')
            current.append(f.git('mktree',data=''.join(entries).encode()))
        trees.extend(current);selected.append(current[0]);children=current
    tree=children[0]
    signed=f.git('-c','gpg.format=ssh','-c',f'user.signingkey={key}','commit-tree','-S',tree,data=b'scale fixture\n')
    f.git('update-ref','refs/heads/main',signed)
    objects=list(dict.fromkeys([signed,*trees,*blobs.values(),ignore]))
    return dict(count=count,fanouts=fanouts,depth=len(fanouts),tree=tree,commit=signed,
        distinct_objects=len(objects),distinct_blobs=len(blobs)+1,leaf_bytes=8,logical_payload_bytes=count*8+5,
        modes={'leaves':'100644','directories':'040000'},shape='wide numeric paths; two alternating subtree variants; up to sixteen distinct 8-byte payloads',
        git_objects=objects,tree_objects=list(dict.fromkeys(trees)),payload_objects=list(blobs.values()),ignore=ignore,
        first_path='/'.join('0'*width for width in widths),sparse_objects=[signed,*selected,blobs[0]],
        namespace_payload_reads=math.prod(fanouts[:-1])+101,
        namespace_objects=[signed,*dict.fromkeys(trees),ignore,*{blobs[((total*17+total//2)%fanouts[-1]+total%2)%16] for total in range(sum(n-1 for n in fanouts[:-1])+1)},blobs[0]])


def expected_demands(info,backend,workload):
    if backend=='materialized' or workload=='full':return set(info['git_objects'])
    if workload=='enumerate':return set(info['namespace_objects'])
    if workload=='sparse':return set(info['sparse_objects'])
    return {info['commit'],info['tree']}


def timed_phase(f,phase,operation,**description):
    before=os.statvfs(f.root);available=before.f_bavail*before.f_frsize
    begin=time.monotonic_ns();success=False
    try:
        value=operation();success=True;return value
    finally:
        after=os.statvfs(f.root.parent if not f.root.exists() else f.root)
        record(dict(type='phase',phase=phase,elapsed_ns=time.monotonic_ns()-begin,
            outcome='passed' if success else 'failed',filesystem_available_delta_bytes=available-after.f_bavail*after.f_frsize,**description))


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
    elapsed_ns=time.monotonic_ns()-started
    result=dict(description,elapsed_ns=elapsed_ns,child_start_ns=rows[0]['child_started_ns']-started,
        child_duration_ns=rows[-1]['duration_ns'],child_observation=rows[-1],
        after_child_ns=elapsed_ns-(rows[0]['child_started_ns']-started)-rows[-1]['duration_ns'],
        runner_and_waited_children_peak_resident_kib=peak,**samples)
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
    policies=OPTIONS.policies.split(',')
    if OPTIONS.backend=='materialized':assert policies==['all'],'materialization supports explicit all only'
    if OPTIONS.profile=='namespace':assert OPTIONS.backend=='fuse','namespace-only profile measures lazy FUSE'
    cases=scenario_plan(OPTIONS.backend,OPTIONS.source,OPTIONS.profile,policies,OPTIONS.verification,
        [int(n) for n in OPTIONS.sizes.split(',')] if OPTIONS.sizes else None)
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
        env=dict(f.env)
        if OPTIONS.backend=='materialized':
            tools=f.root/'git-only';tools.mkdir();(tools/'git').symlink_to(native_git);(tools/'ssh-keygen').symlink_to(shutil.which('ssh-keygen'));env['PATH']=str(tools)
        else:assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3')
        source=f.https() if OPTIONS.source=='remote' else None
        env.update({k:v for k,v in f.env.items() if k!='PATH'})
        record(dict(type='environment',revision=subprocess.check_output(['git','rev-parse','HEAD']).decode().strip(),
            platform=platform.platform(),machine=platform.machine(),cpu=platform.processor(),cpu_count=os.cpu_count(),
            physical_memory_bytes=os.sysconf('SC_PHYS_PAGES')*os.sysconf('SC_PAGE_SIZE'),
            rlimit_nofile=resource.getrlimit(resource.RLIMIT_NOFILE),backend=OPTIONS.backend,
            source=OPTIONS.source,profile=OPTIONS.profile,verification=OPTIONS.verification,scenario_plan=cases,
            storage='fixture and private per-invocation ODB on host temporary filesystem',
            measurement_limits='filesystem delta is host-wide; sampled /proc excludes subprocesses; wait4 peak includes waited descendants; macOS runner-only /proc unavailable; sample-file allocation is not a universal block-size assumption'))
        for count in dict.fromkeys(case['files'] for case in cases):
            description=dict(backend=OPTIONS.backend,source=OPTIONS.source,profile=OPTIONS.profile,files=count)
            info=timed_phase(f,'fixture',lambda:fixture(f,count,key),**description);record(dict(type='fixture',**info))
            before=f.git('show-ref');clone=f.root/'clone'
            timed_phase(f,'clone',lambda:subprocess.run([native_git,'clone','--quiet','--no-hardlinks',source or str(f.repo),str(clone)],env=f.env,check=True),**description)
            sample=(clone/info['first_path']).stat()
            record(dict(type='allocation-sample',files=count,logical_bytes=sample.st_size,allocated_bytes=sample.st_blocks*512,filesystem_fragment_bytes=os.statvfs(clone).f_frsize))
            selected=[case for case in cases if case['files']==count]
            original=f.repo;f.repo=clone
            for workload in dict.fromkeys(case['workload'] for case in selected):
                child=[sys.executable,'-c',CHILD,workload,str(count),json.dumps(info['fanouts']),*(['write'] if OPTIONS.backend=='passthrough' and workload=='full' else [])]
                execute(f,child,f.env,dict(type='direct-baseline',**{**description,'backend':'disk','target_backend':OPTIONS.backend},workload=workload))
            f.repo=original;timed_phase(f,'clone-cleanup',lambda:shutil.rmtree(clone),**description)
            for case in selected:
                first_request=len(f.requests)
                child=[sys.executable,'-c',CHILD,case['workload'],str(count),json.dumps(info['fanouts']),*(['write'] if OPTIONS.backend=='passthrough' and case['workload']=='full' else [])]
                command=[str(OPTIONS.binary),*([source] if source else []),'main',f"--write-policy={case['policy']}",*(['--verify'] if case['verify'] else []),'--',*child]
                result=execute(f,command,env,dict(type='runner',backend=OPTIONS.backend,source=OPTIONS.source,profile=OPTIONS.profile,**case),True)
                requests=f.requests[first_request:];wants={oid for request in requests for oid in request.get('wants',[])}
                assert wants==(expected_demands(info,OPTIONS.backend,case['workload']) if source else set()),(case,wants,info)
                assert result['metrics']['remote_fetches']==len(wants),result
                assert not (f.root/'cache').exists(),'persistent cache created'
                record(dict(type='transport',**case,initial_resolution_fetches=sum(info['commit'] in r.get('wants',[]) for r in requests),
                    exact_object_fetches=sum(bool(r.get('wants')) and info['commit'] not in r['wants'] for r in requests),
                    response_bytes=sum(r.get('response_bytes',0) for r in requests),http_requests=len(requests),objects=sorted(wants)))
                assert not list(f.root.glob('git-on-*')),'run cleanup failed'
                assert f.git('show-ref')==before,'source refs changed'
    finally:
        def cleanup_fixture():
            if hasattr(f,'server'):f.server.shutdown()
            f.close()
        timed_phase(f,'fixture-cleanup',cleanup_fixture,backend=OPTIONS.backend,source=OPTIONS.source,profile=OPTIONS.profile)


if __name__=='__main__':main()
