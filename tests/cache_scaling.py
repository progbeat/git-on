#!/usr/bin/env python3
"""Linux syscall evidence for ordinary cache work in independent K/N cases."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

parser=argparse.ArgumentParser()
parser.add_argument('--probe',type=Path,required=True)
parser.add_argument('--before-resource-probe',type=Path)
parser.add_argument('--output',type=Path,default=Path('cache-scaling.jsonl'))
options=parser.parse_args()
from remote import Fixture


def main():
    assert sys.platform=='linux' and shutil.which('strace'),'strace on Linux is required'
    f=Fixture()
    try:
        payloads={}
        for index in range(32):
            data=f'cache-{index}\n'.encode();oid=f.git('hash-object','-w','--stdin',data=data)
            payloads[oid]=bytes([3])+data
        tree=f.git('mktree',data=''.join(f'100644 blob {oid}\tf{index:02d}\n' for index,oid in enumerate(payloads)).encode())
        commit=f.git('commit-tree',tree,data=b'cache scaling\n');f.git('update-ref','refs/heads/main',commit)
        for oid,kind in ((tree,2),(commit,1)):
            payloads[oid]=bytes([kind])+subprocess.check_output(['git','-C',str(f.repo),'cat-file',('tree' if kind==2 else 'commit'),oid],env=f.env)
        for capacity in (64,256):
            for operations in (8,32):
                for binary in ([options.before_resource_probe] if options.before_resource_probe else [])+[options.probe]:
                    home=f.root/f'cache-{capacity}-{operations}-{binary==options.probe}'
                    root=home/'git-on'/('objects-v2' if binary==options.probe else 'objects-v1');root.mkdir(parents=True)
                    values=dict(payloads)
                    for index in range(capacity-len(values)):
                        data=f'padding-{index}\n'.encode();oid=hashlib.sha1(f'blob {len(data)}\0'.encode()+data).hexdigest()
                        values[oid]=bytes([3])+data
                    assert len(values)==capacity
                    for oid,data in values.items(): (root/oid).write_bytes(data)
                    env=dict(f.env,XDG_CACHE_HOME=str(home))
                    # Pay migration once before tracing ordinary operations.
                    subprocess.run([str(binary.resolve()),'file://'+str(f.repo),'HEAD'],cwd=f.cwd,env=env,input=b'exit\n',capture_output=True,check=True)
                    trace=f.root/'trace'
                    argv=['strace','-ttt','-T','-yy','-e','trace=getdents64,newfstatat,statx,flock,close','-o',str(trace),str(binary.resolve()),'file://'+str(f.repo),'HEAD']
                    started=time.monotonic_ns()
                    output=subprocess.run(argv,cwd=f.cwd,env=env,input=(''.join(f'read f{i:02d}\n' for i in range(operations))+'exit\n').encode(),capture_output=True,check=True)
                    rows=[json.loads(x) for x in output.stdout.splitlines()]
                    assert sum('data' in row for row in rows)==operations
                    metadata=enumerations=0;held=0.;acquired=None
                    for line in trace.read_text().splitlines():
                        if str(root) not in line: continue
                        if re.search(r'\b(getdents64|newfstatat|statx)\(',line): metadata+=1
                        if 'getdents64(' in line: enumerations+=1
                        if 'flock(' in line and ' = 0 ' in line: acquired=float(line.split()[0])
                        elif acquired is not None and 'close(' in line and str(root/'lock') in line:
                            held+=float(line.split()[0])-acquired;acquired=None
                    if binary==options.probe:
                        assert enumerations==0,(capacity,operations,enumerations)
                        assert metadata<=12*(operations+2),(capacity,operations,metadata)
                    elif options.before_resource_probe:
                        assert enumerations>=operations*2,(capacity,operations,enumerations)
                        assert metadata>=capacity*operations,(capacity,operations,metadata)
                    result=dict(revision=subprocess.check_output(['git','rev-parse','HEAD']).decode().strip(),binary=str(binary),occupancy=capacity,operations=operations,
                        cache_metadata_syscalls=metadata,cache_directory_enumerations=enumerations,observed_lock_seconds=held,elapsed_ns=time.monotonic_ns()-started,
                        metrics=rows[-1].get('metrics'),cache_state='fully populated; migration excluded; all payload hits',platform=sys.platform)
                    print(json.dumps(result),flush=True)
                    with options.output.open('a') as file: file.write(json.dumps(result)+'\n')
    finally: f.close()

if __name__=='__main__': main()
