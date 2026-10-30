#!/usr/bin/env python3
"""Bounded full-process I/O diagnostics; traced timings are never benchmarks.

Runs only generated fixtures. Captures waited-child CPU, object-file writes and
FUSE request opcodes, including Git subprocesses. No latency SLA is invented.
"""
import argparse
import ast
from collections import Counter,defaultdict
import json
import os
from pathlib import Path
import re
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time

OPCODES={1:'lookup',2:'forget',3:'getattr',14:'open',15:'read',16:'write',18:'release',25:'flush',26:'init',27:'opendir',28:'readdir',29:'releasedir',42:'batch_forget'}
STRINGS=re.compile(r'"(?:[^"\\]|\\.)*"')


def trace_summary(paths,root):
    requests=Counter();processes=Counter();writes=defaultdict(Counter)
    for path in paths:
        text=path.read_text();args=[]
        for line in text.splitlines():
            if 'execve(' in line:
                args=[ast.literal_eval(s) for s in STRINGS.findall(line)]
                break
        command=next((name for name in ('unpack-objects','index-pack','pack-objects','fetch','cat-file') if name in args),'other')
        processes[command]+=1;pending=None
        for line in text.splitlines():
            if '<unfinished ...>' in line:
                pending=line.split('<unfinished ...>')[0];continue
            if 'resumed>' in line and pending:
                line=pending+line.split('resumed>',1)[1];pending=None
            if '/dev/fuse' in line and re.search(r'\b(read|readv)\(',line):
                strings=STRINGS.findall(line)
                if strings:
                    data=ast.literal_eval(strings[0]).encode('latin1')
                    if len(data)>=8:
                        opcode=int.from_bytes(data[4:8],sys.byteorder)
                        requests[OPCODES.get(opcode,str(opcode))]+=1
            descriptor=re.search(r'\b(?:write|writev|pwrite64|fsync|fdatasync)\(\d+<([^>]*)>',line)
            if not descriptor or str(root) not in descriptor[1]:continue
            category='objects' if '/remote-repository/objects/' in descriptor[1] else 'other_fixture_files'
            match=re.search(r'\b(write|writev|pwrite64)\([^\n]* = (\d+)',line)
            if match:
                writes[command][category+'_write_calls']+=1
                writes[command][category+'_write_bytes']+=int(match[2])
            if re.search(r'\b(fsync|fdatasync)\(',line):writes[command][category+'_sync_calls']+=1
    return dict(fuse_requests=dict(requests),processes=dict(processes),file_io_by_command={k:dict(v) for k,v in writes.items()})


def capture(argv,cwd,env,root,trace=None,metrics=False,resource_observer=None):
    with tempfile.TemporaryFile() as metrics_output:
        return capture_process(argv,cwd,env,root,trace,metrics,metrics_output,resource_observer)


def capture_process(argv,cwd,env,root,trace,metrics,metrics_output,resource_observer):
    rss_output=root/'observer-rss'
    if resource_observer:
        argv=[str(resource_observer),str(rss_output),*argv]
    if trace is not None:
        argv=['strace','--kill-on-exit','-ff','-qq','-ttt','-T','-yy','-s','32','-e',
              'trace=process,read,readv,write,writev,pread64,pwrite64,fsync,fdatasync','-o',str(trace),*argv]
    if metrics:env=dict(env,GIT_ON_TEST_METRICS_FD=str(metrics_output.fileno()))
    before=resource.getrusage(resource.RUSAGE_CHILDREN);begin=time.monotonic_ns()
    process=subprocess.Popen(argv,cwd=cwd,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True,
        pass_fds=(metrics_output.fileno(),) if metrics else ())
    try:
        stdout,stderr=process.communicate(timeout=60)
    except BaseException:
        try:os.killpg(process.pid,signal.SIGTERM)
        except ProcessLookupError:pass
        try:process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid,signal.SIGKILL);process.communicate(timeout=5)
        raise
    if process.returncode:raise RuntimeError(f'profile command failed ({process.returncode}): {stderr.decode(errors="replace")}')
    elapsed=time.monotonic_ns()-begin;after=resource.getrusage(resource.RUSAGE_CHILDREN)
    rows=[json.loads(line) for line in stdout.splitlines()]
    value=dict(traced=trace is not None,elapsed_ns=elapsed,
        waited_user_cpu_s=after.ru_utime-before.ru_utime,waited_system_cpu_s=after.ru_stime-before.ru_stime,
        phases=[r for r in rows if 'profile_phase' in r],
        child_observations=[r for r in rows if 'completed' in r])
    if metrics:
        metrics_output.seek(0);value['metrics']=json.load(metrics_output)
        value['fuse_requests']=value['metrics']['fuse_requests']
    if resource_observer:
        value['runner_and_waited_children_peak_resident_kib']=int(rss_output.read_text())
        value['resource_observer']='wait4 observer included; largest waited process RSS, not summed concurrent RSS'
    if trace is not None:value.update(trace_summary(trace.parent.glob(trace.name+'.*'),root))
    return value


def child_code(count,shape):
    from scalability import CHILD
    # Full traversal validates every name and byte on each pass, with one small
    # JSON result per phase. No payload is serialized into the observer pipe.
    return ('import json,sys,time\n'
        'for label in ("first","repeat"):\n'
        ' begin=time.monotonic_ns()\n'
        f' sys.argv=["child","full",{str(count)!r},{json.dumps(shape)!r}]\n'
        f' exec({CHILD!r},{{}})\n'
        ' print(json.dumps(dict(profile_phase=label,elapsed_ns=time.monotonic_ns()-begin)),flush=True)\n')


def run(options):
    from remote import Fixture
    from scalability import fixture
    assert sys.platform=='linux','Linux required'
    assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'),'real FUSE required'
    f=Fixture()
    try:
        key=f.root/'profile-signing-key'
        subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
        info=fixture(f,options.files,key)
        child=[sys.executable,'-I','-S','-c',child_code(options.files,info['fanouts'])]
        clone=f.root/'clone'
        subprocess.run(['git','clone','--quiet',str(f.repo),str(clone)],env=f.env,check=True)
        original=f.git('show-ref')
        observations=[]
        with options.output.open('w') as output:
            def record(value):
                value.update(head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),files=options.files,distinct_objects=info['distinct_objects'])
                observations.append(value)
                output.write(json.dumps(value)+'\n');output.flush();print(json.dumps(value),flush=True)
            record(dict(mode='direct',**capture(child,clone,f.env,f.root)))
            for uncached in (True,False):
                env=dict(f.env)
                if uncached:env['GIT_ON_TEST_UNCACHED_SOURCE']='1'
                else:env.pop('GIT_ON_TEST_UNCACHED_SOURCE',None)
                argv=[str(options.binary.resolve()),'main','--',*child]
                # Instrument callback delivery directly: ptrace suppresses the
                # setuid mount helper's privileges. No security settings or
                # elevated tracer are needed; waited CPU includes descendants.
                record(dict(mode='uncached-source' if uncached else 'cached-source',**capture(argv,f.repo,env,f.root,metrics=True)))
                assert not list(f.root.glob('git-on-*')),'private run leaked'
                assert f.git('show-ref')==original,'source refs changed'
            off=next(r for r in observations if r['mode']=='uncached-source')
            on=next(r for r in observations if r['mode']=='cached-source')
            for opcode in ('lookup','read'):
                assert 0<on['fuse_requests'].get(opcode,0)<off['fuse_requests'].get(opcode,0),(opcode,on,off)
            record(dict(mode='kernel-cache-invariants',result='passed',
                        unchanged_payload_and_namespace=True))
    finally:
        for directory in f.root.glob('git-on-*'):
            subprocess.run([str(options.binary.resolve()),'--cleanup',str(directory)],env=f.env,capture_output=True,timeout=20)
        f.close()


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--files',type=int,default=1000)
    parser.add_argument('--output',type=Path,default=Path('io-profile.jsonl'))
    run(parser.parse_args())
