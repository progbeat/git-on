#!/usr/bin/env python3
"""Increasing distinct-object demand: observe retained packs and pack-index work.

strace observes only the loader, excluding native Git subprocesses. Source-probe
reports enumerate available OIDs, so wall time includes that test-helper cost;
the pack-directory observations are separated from it and from historical global cache work.
"""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
parser=argparse.ArgumentParser()
parser.add_argument('--probe',type=Path,required=True)
parser.add_argument('--before-probe',type=Path)
parser.add_argument('--sizes',default='32,128,512')
parser.add_argument('--output',type=Path,default=Path('pack-scaling.jsonl'))
options=parser.parse_args()
from remote import Fixture


def exercise(binary,count,before=False):
    f=Fixture()
    try:
        entries=[]
        for index in range(count):
            blob=f.git('hash-object','-w','--stdin',data=f'object-{index}\n'.encode())
            entries.append(f'100644 blob {blob}\tf{index:04d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'distinct demand fixture\n');f.git('update-ref','refs/heads/main',commit)
        trace=f.root/'trace'
        argv=['strace','-ttt','-T','-yy','-e','trace=getdents64,newfstatat,statx','-o',str(trace),str(binary.resolve()),'file://'+str(f.repo),'HEAD']
        commands=(''.join(f'read f{index:04d}\n' for index in range(count))*2+'exit\n').encode()
        begin=time.monotonic_ns()
        result=subprocess.run(argv,cwd=f.cwd,env=f.env,input=commands,capture_output=True,check=True)
        rows=[json.loads(row) for row in result.stdout.splitlines()]
        values=[row['data'] for row in rows if 'data' in row]
        assert values==[f'object-{i}\n' for i in range(count)]*2
        reports=[row for row in rows if 'metrics' in row]
        first=reports[count]['metrics'];second=reports[-1]['metrics']
        if before:
            assert second['disk_cache']['misses']==first['disk_cache']['misses']
            assert second['disk_cache']['stores']==first['disk_cache']['stores']
        else:
            assert first['remote_fetches']==count+2,first
            assert second['remote_fetches']==first['remote_fetches'],'available run objects fetched again'
        pack_bytes=pack_metadata=0
        for line in trace.read_text().splitlines():
            if '/remote-repository/objects/pack' not in line: continue
            if re.search(r'\b(getdents64|newfstatat|statx)\(',line): pack_metadata+=1
            if 'getdents64(' in line:
                returned=re.search(r'= (\d+) ',line)
                if returned: pack_bytes+=int(returned.group(1))
        # source-probe owns and cleans its private run. Explicitly sampled live
        # retained pack counts are covered by the interactive permanent test.
        observation=dict(binary=str(binary),distinct_blobs=count,passes=2,pack_directory_bytes_enumerated=pack_bytes,
            pack_metadata_syscalls=pack_metadata,elapsed_ns=time.monotonic_ns()-begin,metrics=second,
            baseline=before,object_storage='private run ODB; second pass same run' if not before else 'historical global cache',measurement='loader-only strace, wall time includes helper OID enumeration')
        if not before:
            assert pack_bytes<=2048*(count+2),observation
            assert pack_metadata<=64*(count+2),observation
        print(json.dumps(observation),flush=True)
        with options.output.open('a') as output:output.write(json.dumps(observation)+'\n')
    finally:f.close()

if __name__=='__main__':
    assert sys.platform=='linux' and shutil.which('strace'),'Linux strace required'
    for count in map(int,options.sizes.split(',')):
        if options.before_probe:exercise(options.before_probe,count,True)
        exercise(options.probe,count)
