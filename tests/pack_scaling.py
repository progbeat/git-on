#!/usr/bin/env python3
"""Increasing distinct-object demand: observe retained packs and pack-index work.

strace observes the loader and its children. Source-probe
reports only counters between reads, then enumerates available OIDs once.
The optional historical replay uses local transport and enumerates per read.
"""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
from benchmark_support import source_identity
from remote import Fixture


PACK_PATH = re.compile(r'/objects/pack(?:[/"> ]|$)|\.(?:pack|idx)(?:["> ]|$)')
PACK_METADATA = re.compile(r'\b(?:getdents64|newfstatat|statx|openat|open|creat|rename(?:at2?)?)\(')


def exercise(binary,count,before=False):
    f=Fixture()
    try:
        entries=[];oids=[]
        for index in range(count):
            blob=f.git('hash-object','-w','--stdin',data=f'object-{index}\n'.encode())
            entries.append(f'100644 blob {blob}\tf{index:04d}\n');oids.append(blob)
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'distinct demand fixture\n');f.git('update-ref','refs/heads/main',commit)
        trace=f.root/'trace'
        source = 'file://' + str(f.repo) if before else f.https()
        argv=['strace','-f','-yy','-e','trace=execve,getdents64,newfstatat,statx,openat,open,creat,rename,renameat,renameat2',
              '-o',str(trace),str(binary.resolve()),source,'HEAD']
        if not before: argv.append('--metrics-only')
        commands=(''.join(f'read f{index:04d}\n' for index in range(count))*2+'report\nexit\n').encode()
        begin=time.monotonic_ns()
        result=subprocess.run(argv,cwd=f.cwd,env=f.env,input=commands,capture_output=True,check=True)
        elapsed=time.monotonic_ns()-begin
        rows=[json.loads(row) for row in result.stdout.splitlines()]
        values=[row['data'] for row in rows if 'data' in row]
        assert values==[f'object-{i}\n' for i in range(count)]*2
        reports=[row for row in rows if 'metrics' in row]
        first=reports[count]['metrics'];second=reports[-1]['metrics']
        assert set(reports[-1]['objects'])=={commit,tree,*oids},'private ODB lost available objects'
        if not before: assert all('objects' not in row for row in reports[:-1]),'per-read ODB enumeration returned'
        if before:
            assert second['disk_cache']['misses']==first['disk_cache']['misses']
            assert second['disk_cache']['stores']==first['disk_cache']['stores']
        else:
            assert first['remote_fetches']==count+2,first
            assert second['remote_fetches']==first['remote_fetches'],'available run objects fetched again'
        pack_bytes=pack_metadata=0
        for line in trace.read_text().splitlines():
            if not before and 'execve(' in line:
                assert not re.search(r'"/[^"]*/git-fetch",|"/[^"]*/git", \[[^]]*"fetch"', line), line
            if not PACK_PATH.search(line): continue
            if PACK_METADATA.search(line): pack_metadata+=1
            if 'getdents64(' in line:
                returned=re.search(r'= (\d+) ',line)
                if returned: pack_bytes+=int(returned.group(1))
        # source-probe owns and cleans its private run. Explicitly sampled live
        # retained pack counts are covered by the interactive permanent test.
        observation=dict(binary=str(binary),distinct_blobs=count,passes=2,pack_directory_bytes_enumerated=pack_bytes,
            pack_metadata_syscalls=pack_metadata,metrics=second,baseline=before,
            transport='file' if before else 'https',
            object_storage='streamed objects; second pass same run' if not before else 'historical global cache',
            measurement='strace including children; final OID enumeration once' if not before else 'historical helper enumerates OIDs per read')
        if not before:
            observation['elapsed_ns']=elapsed
            assert pack_bytes == pack_metadata == 0, observation
        return observation
    finally:f.close()

def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe',type=Path,required=True)
    parser.add_argument('--before-probe',type=Path)
    parser.add_argument('--sizes',default='32,128,512')
    parser.add_argument('--output',type=Path,default=Path('pack-scaling.jsonl'))
    options=parser.parse_args(argv)
    assert sys.platform=='linux' and shutil.which('strace'),'Linux strace required'
    identity=source_identity()
    with options.output.open('w') as output:
        for count in map(int,options.sizes.split(',')):
            for binary,before in ([(options.before_probe,True)] if options.before_probe else [])+[(options.probe,False)]:
                row=dict(exercise(binary,count,before),**identity)
                output.write(json.dumps(row)+'\n');output.flush()
                print(json.dumps(row),flush=True)


if __name__=='__main__':main()
