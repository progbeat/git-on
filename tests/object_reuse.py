#!/usr/bin/env python3
"""Check distinct remote imports and reuse through the production CLI."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
from collections import Counter
import os
from process_capture import capture
from benchmark_support import build_stat_observer, stat_abi_bytes
from remote import Fixture


CHILD=r'''
import hashlib,json,os,sys
count,size,stat_bytes=map(int,sys.argv[1:])
for label in ('first','repeat'):
 data_bytes=0;metadata_bytes=0
 for index in range(count):
  name=f'f{index:05d}'
  assert os.stat(name).st_size==size
  metadata_bytes+=stat_bytes
  with open(name,'rb') as file: actual=file.read()
  assert actual==hashlib.shake_256(f'object-reuse:{index}'.encode()).digest(size)
  data_bytes+=len(actual)
 print(json.dumps(dict(completed=label,files=count,payload_bytes=data_bytes,metadata_result_bytes=metadata_bytes)),flush=True)
'''


def native_command_audit(root,env,materialized):
    """Record actual Git argv before exec; never emulate the Git implementation."""
    git=shutil.which('git',path=env['PATH']);assert git
    programs=root/'programs';programs.mkdir()
    log=root/'native-commands.jsonl'
    wrapper=programs/'git'
    wrapper.write_text(f'''#!{sys.executable}
import json,os,sys
fd=os.open({str(log)!r},os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)
try:os.write(fd,(json.dumps(sys.argv[1:])+"\\n").encode())
finally:os.close(fd)
os.execv({git!r},[{git!r},*sys.argv[1:]])
''')
    wrapper.chmod(0o700)
    return dict(env,PATH=str(programs) if materialized else str(programs)+os.pathsep+env['PATH']),log


def native_commands(log):
    counts=Counter()
    for line in log.read_text().splitlines():
        argv=json.loads(line)
        for name in ('fetch','cat-file','unpack-objects'):
            if name in argv:counts[name]+=1;break
    return dict(counts)


def run(options):
    head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
    dirty=bool(subprocess.check_output(['git','diff','--name-only','HEAD']))
    if options.backend=='fuse':
        assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'),'real FUSE required'
    options.output.parent.mkdir(parents=True,exist_ok=True)
    with options.output.open('w') as output:
        def record(row):
            row.update(head=head,working_tree_dirty=dirty,backend=options.backend,source='file:// generated private server',source_cache='new invocation; source OS cache uncontrolled/warm from fixture creation')
            output.write(json.dumps(row)+'\n');output.flush();print(json.dumps(row),flush=True)
        for count in options.sizes:
            f=Fixture()
            try:
                entries=[]
                for index in range(count):
                    data=hashlib.shake_256(f'object-reuse:{index}'.encode()).digest(options.blob_bytes)
                    oid=f.git('hash-object','-w','--stdin',data=data)
                    entries.append(f'100644 blob {oid}\tf{index:05d}\n')
                tree=f.git('mktree',data=''.join(entries).encode())
                commit=f.git('commit-tree',tree,data=b'object reuse regression\n');f.git('update-ref','refs/heads/main',commit)
                child=f.root/'command.py';child.write_text(CHILD)
                stat_bytes=stat_abi_bytes(build_stat_observer(f.root/'stat-abi'))
                command=[sys.executable,'-I','-S',str(child),str(count),str(options.blob_bytes),str(stat_bytes)]
                source='file://'+str(f.repo)
                env,log=native_command_audit(f.root,f.env,options.backend=='materialized')
                argv=[str(options.binary.resolve()),source,'HEAD','--write-policy=all','--',*command]
                result=capture(argv,f.cwd,env,f.root,metrics=True)
                result['native_commands']=native_commands(log)
                validate_runner(result,count,options.blob_bytes,stat_bytes,options.backend)
                record(dict(type='runner',files=count,blob_bytes=options.blob_bytes,stat_abi_bytes=stat_bytes,**result))
                assert not list(f.root.glob('git-on-*')),'private invocation leaked'
            finally:f.close()


def validate_runner(result,count,size,stat_bytes,backend=None):
    phases=result['child_observations'];metrics=result['metrics']
    assert [p['completed'] for p in phases]==['first','repeat'],result
    assert all(p['files']==count and p['payload_bytes']==count*size and p['metadata_result_bytes']==count*stat_bytes for p in phases),result
    assert metrics['remote_fetches']==count+2,metrics
    assert metrics['remote_raw_blob_imports']==count and metrics['remote_raw_blob_bytes']==count*size,metrics
    assert metrics['blob_loads']==count,'source blob was decoded again after close/reopen/eviction'
    assert metrics['blob_cache_bytes']<=metrics['blob_cache_limit'],metrics
    if backend:
        operations=2+(count+255)//256 if backend=='materialized' else count+2
        session_fetches=metrics.get('remote_session_fetch_operations',0)
        session_decodes=metrics.get('remote_session_decode_operations',0)
        assert metrics['remote_fetch_operations']+session_fetches==operations,metrics
        assert metrics['remote_decode_operations']+session_decodes==operations,metrics
        if session_fetches:assert metrics['remote_session_starts']==1,metrics
    if backend:
        native=result['native_commands']
        assert not native.get('unpack-objects',0),'raw import re-encodes loose objects'
        assert native.get('cat-file',0)==metrics['remote_decode_operations'],result
        assert native.get('fetch',0)==metrics['remote_fetch_operations'],result
    if backend=='fuse':assert result['fuse_requests'].get('read',0)>0,'real FUSE dispatch missing'
    if backend=='materialized':assert not result['fuse_requests'],'materialization did not bypass FUSE'


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--backend',choices=['fuse','materialized'],required=True)
    parser.add_argument('--sizes',type=lambda v:list(map(int,v.split(','))),default=[32,160])
    parser.add_argument('--blob-bytes',type=int,default=8192)
    parser.add_argument('--output',type=Path,default=Path('object-reuse.jsonl'))
    run(parser.parse_args())
