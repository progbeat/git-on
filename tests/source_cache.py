#!/usr/bin/env python3
"""Verify protected-source kernel caching without changing names or bytes."""
import argparse
import json
from pathlib import Path
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


def run(options):
    from remote import Fixture
    from scalability import fixture
    assert sys.platform=='linux','Linux required'
    assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'),'real FUSE required'
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
                value.update(head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),files=options.files,distinct_objects=info['distinct_objects'])
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
