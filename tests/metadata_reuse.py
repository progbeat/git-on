#!/usr/bin/env python3
"""Verify one-time preparation of wide directories across repeated closed reads."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
from benchmark_support import stat_abi_bytes
from process_capture import capture
from remote import Fixture

CHILD=r'''
import json,os,sys
count,operations,stat_bytes=map(int,sys.argv[1:])
for phase in ('first','repeat'):
 for index in range(operations):
  name=f'f{(index*7919)%count:06d}'
  assert os.stat(name).st_size==1
  with open(name,'rb') as file:assert file.read()==b'x'
 print(json.dumps(dict(completed=phase,operations=operations,payload_bytes=operations,metadata_result_bytes=operations*stat_bytes)),flush=True)
'''


def validate_runner(result,count,operations,stat_bytes,backend):
    phases=result['child_observations'];metrics=result['metrics']
    assert [p['completed'] for p in phases]==['first','repeat'],result
    assert all(p['operations']==operations and p['payload_bytes']==operations and p['metadata_result_bytes']==operations*stat_bytes for p in phases),result
    assert metrics['tree_reads']==1,metrics
    assert metrics['prepared_tree_entries']==count,metrics
    assert metrics['blob_loads']==1,metrics
    assert metrics['tree_cache_limit']==0 or metrics['tree_cache_bytes']<=metrics['tree_cache_limit'],metrics
    assert metrics['blob_cache_bytes']<=metrics['blob_cache_limit'],metrics
    if backend=='fuse':assert result['fuse_requests'].get('read',0)>0,'real FUSE dispatch missing'
    else:assert not result['fuse_requests'],'materialization did not bypass FUSE'


def run(options):
    head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
    dirty=bool(subprocess.check_output(['git','status','--porcelain']))
    if options.backend=='fuse':
        assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'),'real FUSE required'
    options.output.parent.mkdir(parents=True,exist_ok=True)
    with options.output.open('w') as output:
        def record(row):
            row.update(head=head,working_tree_dirty=dirty,backend=options.backend,source=options.source,
                source_cache='new invocation; source OS cache uncontrolled/warm from fixture creation',distinct_objects=3)
            output.write(json.dumps(row)+'\n');output.flush();print(json.dumps(row),flush=True)
        for count in options.sizes:
            f=Fixture()
            try:
                blob=f.git('hash-object','-w','--stdin',data=b'x')
                entries=''.join(f'100644 blob {blob}\tf{index:06d}\n' for index in range(count))
                tree=f.git('mktree',data=entries.encode())
                commit=f.git('commit-tree',tree,data=b'metadata reuse regression\n');f.git('update-ref','refs/heads/main',commit)
                stat_bytes=stat_abi_bytes(f.root)
                child=f.root/'command.py';child.write_text(CHILD)
                command=[sys.executable,'-I','-S',str(child),str(count),str(options.operations),str(stat_bytes)]
                source='file://'+str(f.repo) if options.source=='remote' else str(f.repo)
                env=dict(f.env)
                if options.backend=='materialized':
                    programs=f.root/'programs';programs.mkdir();(programs/'git').symlink_to(shutil.which('git'));env['PATH']=str(programs)
                argv=[str(options.binary.resolve())]
                if options.source=='remote':argv.append(source)
                argv+=['main','--write-policy=all','--',*command]
                result=capture(argv,f.repo if options.source=='local' else f.cwd,env,f.root,metrics=True)
                validate_runner(result,count,options.operations,stat_bytes,options.backend)
                record(dict(type='runner',files=count,operations=options.operations,stat_abi_bytes=stat_bytes,**result))
                assert not list(f.root.glob('git-on-*')),'private invocation leaked'
            finally:f.close()


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--backend',choices=['fuse','materialized'],required=True)
    parser.add_argument('--source',choices=['local','remote'],required=True)
    parser.add_argument('--sizes',type=lambda value:list(map(int,value.split(','))),default=[1000,100000])
    parser.add_argument('--operations',type=int,default=1000)
    parser.add_argument('--output',type=Path,required=True)
    run(parser.parse_args())
