#!/usr/bin/env python3
"""Bounded optimized-CLI profiling of distinct remote objects and same-run reuse.

Times the entire invocation and waited descendants; traces are separate. The
application API boundary is the generated Python command: read payloads and
explicit stat results use a fixed native stat ABI size. Interpreter/runtime
internal operations are included in total time, never added to application N.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time
from io_profile import capture
from remote import Fixture


def observe(argv,cwd,env,root,**kwargs):
    return capture(argv,cwd,env,root,resource_observer=root/'resource-observer',**kwargs)

CHILD=r'''
import hashlib,json,os,sys,time
count,size,stat_bytes=map(int,sys.argv[1:])
for label in ('first','repeat'):
 begin=time.monotonic_ns();data_bytes=0;metadata_bytes=0
 for index in range(count):
  name=f'f{index:05d}'
  assert os.stat(name).st_size==size
  metadata_bytes+=stat_bytes
  with open(name,'rb') as file: actual=file.read()
  assert actual==hashlib.shake_256(f'import-profile:{index}'.encode()).digest(size)
  data_bytes+=len(actual)
 print(json.dumps(dict(completed=label,files=count,payload_bytes=data_bytes,metadata_result_bytes=metadata_bytes)),flush=True)
 print(json.dumps(dict(profile_phase=label,elapsed_ns=time.monotonic_ns()-begin)),flush=True)
'''


def stat_abi_bytes(root):
    source=root/'resource-observer.c';binary=root/'resource-observer'
    source.write_text(r'''#define _DEFAULT_SOURCE
#include <sys/stat.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
int main(int argc,char **argv){
 if(argc==2 && !strcmp(argv[1],"--stat-size")){printf("%zu",sizeof(struct stat));return 0;}
 if(argc<3)return 125;
 pid_t pid=fork();if(pid<0)return 125;
 if(pid==0){execvp(argv[2],argv+2);_exit(127);}
 struct rusage usage;int status;
 while(wait4(pid,&status,0,&usage)<0){if(errno!=EINTR)return 125;}
 FILE *out=fopen(argv[1],"w");if(!out)return 125;
 long resident=usage.ru_maxrss;
#ifdef __APPLE__
 resident=(resident+1023)/1024;
#endif
 fprintf(out,"%ld\n",resident);if(fclose(out))return 125;
 return WIFEXITED(status)?WEXITSTATUS(status):128+WTERMSIG(status);
}
''')
    subprocess.run(['cc',str(source),'-o',str(binary)],check=True,capture_output=True)
    return int(subprocess.check_output([str(binary),'--stat-size']))


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
                    data=hashlib.shake_256(f'import-profile:{index}'.encode()).digest(options.blob_bytes)
                    oid=f.git('hash-object','-w','--stdin',data=data)
                    entries.append(f'100644 blob {oid}\tf{index:05d}\n')
                tree=f.git('mktree',data=''.join(entries).encode())
                commit=f.git('commit-tree',tree,data=b'import profile\n');f.git('update-ref','refs/heads/main',commit)
                child=f.root/'command.py';child.write_text(CHILD)
                stat_bytes=stat_abi_bytes(f.root)
                command=[sys.executable,'-I','-S',str(child),str(count),str(options.blob_bytes),str(stat_bytes)]
                source='file://'+str(f.repo)
                clone=f.root/'clone'
                materialize=observe(['git','clone','--quiet',source,str(clone)],f.cwd,f.env,f.root)
                direct=observe(command,clone,f.env,f.root)
                begin=time.monotonic_ns();cpu=time.process_time_ns();shutil.rmtree(clone)
                cleanup=dict(elapsed_ns=time.monotonic_ns()-begin,parent_cpu_ns=time.process_time_ns()-cpu)
                record(dict(type='baseline',files=count,blob_bytes=options.blob_bytes,stat_abi_bytes=stat_bytes,materialize=materialize,command=direct,cleanup=cleanup,total_elapsed_ns=materialize['elapsed_ns']+direct['elapsed_ns']+cleanup['elapsed_ns']))
                env=dict(f.env)
                if options.backend=='materialized':
                    programs=f.root/'no-fuse-programs';programs.mkdir();(programs/'git').symlink_to(shutil.which('git'))
                    if options.trace:(programs/'strace').symlink_to(shutil.which('strace'))
                    env['PATH']=str(programs)
                if options.before_binary:
                    before=observe([str(options.before_binary.resolve()),source,'HEAD','--write-policy=all','--',*command],f.cwd,env,f.root,metrics=True)
                    record(dict(type='runner',variant='before',files=count,blob_bytes=options.blob_bytes,**before))
                argv=[str(options.binary.resolve()),source,'HEAD','--write-policy=all','--',*command]
                result=observe(argv,f.cwd,env,f.root,metrics=True)
                validate_runner(result,count,options.blob_bytes,stat_bytes,options.backend)
                record(dict(type='runner',variant='current',files=count,blob_bytes=options.blob_bytes,**result))
                if options.trace:
                    assert options.backend=='materialized','ptrace must not suppress FUSE helper privileges'
                    traced=observe(argv,f.cwd,env,f.root,trace=f.root/'trace',metrics=True)
                    validate_runner(traced,count,options.blob_bytes,stat_bytes,options.backend)
                    assert traced['processes'].get('unpack-objects',0)==0,traced
                    assert traced['processes'].get('cat-file',0)>0,traced
                    record(dict(type='runner',variant='current',files=count,blob_bytes=options.blob_bytes,**traced))
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
        assert metrics['remote_fetch_operations']==operations,metrics
        assert metrics['remote_decode_operations']==operations,metrics
    if backend=='fuse':assert result['fuse_requests'].get('read',0)>0,'real FUSE dispatch missing'
    if backend=='materialized':assert not result['fuse_requests'],'materialization did not bypass FUSE'


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--before-binary',type=Path)
    parser.add_argument('--backend',choices=['fuse','materialized'],required=True)
    parser.add_argument('--sizes',type=lambda v:list(map(int,v.split(','))),default=[32,160])
    parser.add_argument('--blob-bytes',type=int,default=8192)
    parser.add_argument('--trace',action='store_true')
    parser.add_argument('--output',type=Path,default=Path('import-profile.jsonl'))
    run(parser.parse_args())
