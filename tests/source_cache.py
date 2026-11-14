#!/usr/bin/env python3
"""Verify protected-source kernel caching without changing names or bytes."""
import argparse
import json
from pathlib import Path
from benchmark_support import source_identity
import shutil
import subprocess
import sys
from process_capture import capture


def child_code(count,shape):
    from scalability import CHILD
    # Full traversal validates every name and byte on each pass, with one small
    # JSON result per phase. No payload is serialized into the observer pipe.
    return ('import json,sys\n'
        'for label in ("first","repeat"):\n'
        f' sys.argv=["child","full",{str(count)!r},{json.dumps(shape)!r}]\n'
        f' exec({CHILD!r},{{}})\n'
        ' print(json.dumps(dict(completed=label)),flush=True)\n')


def directory_names(count):
    return [f'entry-{index:04d}-'+'x'*96 for index in range(count)]


def directory_reopen_code(count):
    # No rewind or seek within one handle: this isolates reuse across opens.
    return ('import json,os\n'
            f'expected=set({directory_names(count)!r})\n'
            'for phase in ("first","repeat"):\n'
            ' names=os.listdir("paged")\n'
            ' assert len(names)==len(expected) and set(names)==expected\n'
            ' assert os.listdir("empty")==[]\n'
            ' print(json.dumps(dict(completed=phase,directory_entries=len(expected))),flush=True)\n')


def directory_child_code(count):
    return 'expected='+repr([name.encode() for name in directory_names(count)])+'\n'+r'''
import ctypes,json,os
libc=ctypes.CDLL(None,use_errno=True)
class Dirent64(ctypes.Structure):
    _fields_=[('ino',ctypes.c_uint64),('offset',ctypes.c_int64),
              ('reclen',ctypes.c_ushort),('kind',ctypes.c_ubyte),('name',ctypes.c_char*256)]
libc.opendir.argtypes=[ctypes.c_char_p];libc.opendir.restype=ctypes.c_void_p
libc.readdir64.argtypes=[ctypes.c_void_p];libc.readdir64.restype=ctypes.POINTER(Dirent64)
libc.telldir.argtypes=[ctypes.c_void_p];libc.telldir.restype=ctypes.c_long
libc.seekdir.argtypes=[ctypes.c_void_p,ctypes.c_long]
libc.rewinddir.argtypes=[ctypes.c_void_p]
libc.closedir.argtypes=[ctypes.c_void_p];libc.closedir.restype=ctypes.c_int
expected=set(expected)|{b'.',b'..'}
def read_one(stream):
    ctypes.set_errno(0);entry=libc.readdir64(stream)
    if not entry:
        assert ctypes.get_errno()==0,ctypes.get_errno()
        return None
    entry=entry.contents;name=bytes(entry.name)
    assert name in expected and entry.ino>0,(name,entry.ino)
    assert entry.kind==(4 if name in {b'.',b'..'} else 8),(name,entry.kind)
    return name,entry.ino
def rest(stream):
    result=[]
    while (entry:=read_one(stream)) is not None:result.append(entry)
    return result
identities=None
for phase in ('first','repeat'):
    stream=libc.opendir(b'paged');assert stream,ctypes.get_errno()
    try:
        prefix=[read_one(stream) for _ in range(min(450,len(expected)-2))]
        assert None not in prefix
        cookie=libc.telldir(stream);assert cookie>=0
        suffix=rest(stream);complete=prefix+suffix
        assert len(complete)==len(expected) and {x[0] for x in complete}==expected
        assert read_one(stream) is None
        libc.seekdir(stream,cookie);assert rest(stream)==suffix
        libc.rewinddir(stream);assert rest(stream)==complete
        current=dict(complete)
        if identities is None:identities=current
        else:assert current==identities,'inode identity changed on reopen'
        assert set(os.listdir(b'paged'))==expected-{b'.',b'..'}
        assert os.listdir('empty')==[] and os.listdir('empty')==[]
    finally:assert libc.closedir(stream)==0
    print(json.dumps(dict(completed=phase,directory_entries=len(expected)-2)),flush=True)
'''


def directory_cache_case(f,binary,record):
    # Missing leaf data and an unvisited missing subtree make accidental
    # preloading observable without timing assertions or a large fixture.
    count=640
    missing=f.git('hash-object','-w','--stdin',data=b'unrequested directory-cache payload')
    empty=f.git('mktree',data=b'')
    paged=f.git('mktree',data=b''.join(f'100644 blob {missing}\t{name}\n'.encode() for name in directory_names(count)))
    unvisited=f.git('mktree',data=f'100644 blob {missing}\tleaf\n'.encode())
    root=f.git('mktree',data=f'040000 tree {paged}\tpaged\n040000 tree {empty}\tempty\n040000 tree {unvisited}\tunvisited\n'.encode())
    commit=f.git('commit-tree',root,data=b'directory-cache fixture\n')
    f.git('update-ref','refs/heads/directory-cache',commit)
    for oid in (missing,unvisited):(f.repo/'objects'/oid[:2]/oid[2:]).unlink()
    original=f.git('show-ref');observations=[]
    child=[sys.executable,'-I','-S','-c',directory_reopen_code(count)]
    for uncached in (True,False):
        env=dict(f.env)
        if uncached:env['GIT_ON_TEST_UNCACHED_SOURCE']='1'
        else:env.pop('GIT_ON_TEST_UNCACHED_SOURCE',None)
        result=capture([str(binary.resolve()),'directory-cache','--write-policy=none','--',*child],f.repo,env,f.root,metrics=True)
        assert result['child_observations']==[dict(completed=phase,directory_entries=count) for phase in ('first','repeat')]
        metrics=result['metrics']
        assert metrics['blob_loads']==0 and metrics['header_reads']==0,metrics
        assert metrics['tree_reads']==3,metrics
        assert not list(f.root.glob('git-on-*')),'directory run leaked'
        assert f.git('show-ref')==original,'directory source refs changed'
        value=dict(mode='uncached-readonly-directory' if uncached else 'cached-readonly-directory',files=count,distinct_objects=6,**result)
        observations.append(value);record(value)
    off,on=observations
    assert 0<on['fuse_requests']['readdir']<off['fuse_requests']['readdir'],observations
    record(dict(mode='directory-cache-invariants',files=count,distinct_objects=6,result='passed',unchanged_names_on_reopen=True,unrequested_data_unavailable=True))


def run(options):
    from remote import Fixture
    from scalability import fixture
    assert sys.platform=='linux','Linux required'
    assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'),'real FUSE required'
    identity=source_identity()
    head,dirty=identity['head'],identity['working_tree_dirty']
    f=Fixture()
    try:
        key=f.root/'cache-signing-key'
        subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
        info=fixture(f,options.files,key)
        child=[sys.executable,'-I','-S','-c',child_code(options.files,info['fanouts'])]
        original=f.git('show-ref')
        observations=[]
        with options.output.open('w') as output:
            def record(value):
                value.update(head=head,working_tree_dirty=dirty)
                value.setdefault('files',options.files);value.setdefault('distinct_objects',info['distinct_objects'])
                observations.append(value)
                output.write(json.dumps(value)+'\n');output.flush();print(json.dumps(value),flush=True)
            for uncached in (True,False):
                env=dict(f.env)
                if uncached:env['GIT_ON_TEST_UNCACHED_SOURCE']='1'
                else:env.pop('GIT_ON_TEST_UNCACHED_SOURCE',None)
                argv=[str(options.binary.resolve()),'main','--',*child]
                # Compare delivered callbacks with and without source caching;
                # both executions validate the same complete namespace/bytes.
                record(dict(mode='uncached-source' if uncached else 'cached-source',**capture(argv,f.repo,env,f.root,metrics=True)))
                assert not list(f.root.glob('git-on-*')),'private run leaked'
                assert f.git('show-ref')==original,'source refs changed'
            off=next(r for r in observations if r['mode']=='uncached-source')
            on=next(r for r in observations if r['mode']=='cached-source')
            for opcode in ('lookup','read'):
                assert 0<on['fuse_requests'].get(opcode,0)<off['fuse_requests'].get(opcode,0),(opcode,on,off)
            record(dict(mode='kernel-cache-invariants',result='passed',
                        unchanged_payload_and_namespace=True))
            directory_cache_case(f,options.binary,record)
    finally:
        for directory in f.root.glob('git-on-*'):
            subprocess.run([str(options.binary.resolve()),'--cleanup',str(directory)],env=f.env,capture_output=True,timeout=20)
        f.close()


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--files',type=int,default=1000)
    parser.add_argument('--output',type=Path,default=Path('source-cache.jsonl'))
    run(parser.parse_args())
