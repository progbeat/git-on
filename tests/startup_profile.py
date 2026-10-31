#!/usr/bin/env python3
"""Fixed source, varied demand, full native baseline; setup is never subtracted."""
import argparse,hashlib,json,random,shutil,subprocess,sys,time
from pathlib import Path
from import_profile import CHILD,stat_abi_bytes
from io_profile import capture
from remote import Fixture

PHASES=['execute_begin','source_begin','source_ready','backend_begin','backend_fuse_ready',
    'command_spawn_begin','command_started','command_done','unmount_begin','unmount_done',
    'transport_cleanup_begin','transport_cleanup_done','filesystem_cleanup_begin','filesystem_cleanup_done','execute_end']

def validate_timing(row):
    events=row['runner_timeline']
    assert [e['phase'] for e in events]==PHASES,'incomplete invocation timeline'
    times=[row['observed_start_monotonic_ns'],*[e['monotonic_ns'] for e in events],row['observed_end_monotonic_ns']]
    assert times==sorted(times),'timeline clocks/order differ'
    assert times[-1]-times[0]==row['elapsed_ns'],'full cost not preserved'
    return {f'{a}_to_{b}_ns':events[i+1]['monotonic_ns']-events[i]['monotonic_ns'] for i,(a,b) in enumerate(zip(PHASES,PHASES[1:]))}

def run(options):
    assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'),'real FUSE required'
    head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
    dirty=bool(subprocess.check_output(['git','diff','--name-only','HEAD']))
    f=Fixture()
    try:
        total=max(options.sizes);size=options.blob_bytes;entries=[]
        for index in range(total):
            oid=f.git('hash-object','-w','--stdin',data=hashlib.shake_256(f'import-profile:{index}'.encode()).digest(size))
            entries.append(f'100644 blob {oid}\tf{index:05d}\n')
        tree=f.git('mktree',data=''.join(entries).encode());commit=f.git('commit-tree',tree,data=b'fixed-source startup profile\n');f.git('update-ref','refs/heads/main',commit)
        stat_bytes=stat_abi_bytes(f.root);source='file://'+str(f.repo)
        plan=[(trial,source_kind,policy,count) for trial in range(options.trials) for source_kind in options.sources for policy in options.policies for count in [-1,*options.sizes]]
        random.Random(4261).shuffle(plan)
        options.output.parent.mkdir(parents=True,exist_ok=True)
        with options.output.open('w') as output:
            for order,(trial,source_kind,policy,count) in enumerate(plan):
                command=[shutil.which('true')] if count<0 else [sys.executable,'-I','-S','-c',CHILD,str(count),str(size),str(stat_bytes)]
                if source_kind=='local':argv=[str(options.binary.resolve()),'HEAD']
                else:argv=[str(options.binary.resolve()),source,'HEAD']
                argv+=[f'--write-policy={policy}','--',*command]
                runner_env=dict(f.env)
                if options.profile_spans:runner_env['GIT_ON_TEST_PROFILE_SPANS']='1'
                def runner():return capture(argv,f.repo if source_kind=='local' else f.cwd,runner_env,f.root,metrics=True,timing=True,resource_observer=f.root/'resource-observer')
                def baseline():
                    clone=f.root/'clone'
                    materialize=capture(['git','clone','--quiet',source,str(clone)],f.cwd,f.env,f.root,resource_observer=f.root/'resource-observer')
                    direct=capture(command,clone,f.env,f.root,resource_observer=f.root/'resource-observer')
                    begin=time.monotonic_ns();cpu=time.process_time_ns();shutil.rmtree(clone)
                    cleanup=dict(elapsed_ns=time.monotonic_ns()-begin,parent_cpu_ns=time.process_time_ns()-cpu)
                    return dict(materialize=materialize,command=direct,cleanup=cleanup,total_elapsed_ns=materialize['elapsed_ns']+direct['elapsed_ns']+cleanup['elapsed_ns'])
                if order%2:before=baseline();after=runner()
                else:after=runner();before=baseline()
                intervals=validate_timing(after)
                assert after['child_observations']==before['command']['child_observations'],'different child workload'
                if count>=0:
                    assert [o['completed'] for o in after['child_observations']]==['first','repeat']
                    assert all(o['files']==count and o['payload_bytes']==count*size and o['metadata_result_bytes']==count*stat_bytes for o in after['child_observations'])
                assert not list(f.root.glob('git-on-*')),'run not cleaned'
                row=dict(type='startup_profile',head=head,working_tree_dirty=dirty,backend='fuse',source=source_kind,policy=policy,profile_spans_enabled=options.profile_spans,source_files=total,demanded_files=count,blob_bytes=size,stat_abi_bytes=stat_bytes,trial=trial,order=order,source_cache='same fixed source; source OS cache warm/uncontrolled; fresh clone and invocation each trial',baseline=before,runner=after,intervals=intervals)
                output.write(json.dumps(row)+'\n');output.flush();print(json.dumps(row),flush=True)
    finally:f.close()

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--sizes',type=lambda x:list(map(int,x.split(','))),default=[0,1,16,64,160])
    parser.add_argument('--sources',type=lambda x:x.split(','),default=['local','remote'])
    parser.add_argument('--policies',type=lambda x:x.split(','),default=['all'])
    parser.add_argument('--profile-spans',action='store_true',help='inclusive diagnostic spans; do not compare instrumented timings with uninstrumented trials')
    parser.add_argument('--blob-bytes',type=int,default=8192)
    parser.add_argument('--trials',type=int,default=3)
    parser.add_argument('--output',type=Path,required=True)
    run(parser.parse_args())
