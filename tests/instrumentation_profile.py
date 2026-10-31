#!/usr/bin/env python3
"""Bounded same-head instrumentation attribution, not WebKit acceptance."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import resource
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--checkout', type=Path, required=True)
    parser.add_argument('--plain', type=Path, required=True)
    parser.add_argument('--metrics', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, help='optional ordinary binary for a paired scoped-fix control')
    parser.add_argument('--pair', action='store_true', help='compare only the two ordinary binaries')
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--backend', choices=['fuse', 'materialized'], required=True)
    parser.add_argument('--counts', default='128,512')
    parser.add_argument('--blob-bytes', type=int, default=65536)
    parser.add_argument('--trials', type=int, default=3)
    options = parser.parse_args()
    counts = [int(value) for value in options.counts.split(',')]
    assert 0 < min(counts) <= max(counts) <= 512
    assert 0 < options.blob_bytes <= 65536 and 0 < options.trials <= 5
    if options.backend == 'fuse':
        assert Path('/dev/fuse').is_char_device() and shutil.which('fusermount3')
    else:
        assert not Path('/dev/fuse').exists(), 'materialized control requires a host without FUSE'
    options.work.mkdir(parents=True, exist_ok=True)
    assert not options.output.exists(), 'refuse to overwrite evidence'
    os.environ['TMPDIR'] = str(options.work.resolve())
    tempfile.tempdir = str(options.work.resolve())
    sys.path.insert(0, str(options.checkout.resolve()/'tests'))
    from remote import Fixture
    from import_profile import CHILD, stat_abi_bytes
    from io_profile import capture
    from real_repository import observe
    if options.backend == 'materialized':
        # The existing observer deliberately validates the FUSE sequence.
        # This separate control changes only the expected backend-ready label.
        import startup_profile
        startup_profile.PHASES = [name.replace('backend_fuse_ready','backend_materialized_ready')
                                 for name in startup_profile.PHASES]
    timeout = shutil.which('timeout')
    assert timeout, 'GNU timeout required for a 25s graceful + 5s hard bound'
    def bounded(argv):
        return [timeout,'--signal=TERM','--kill-after=5s','25s',*map(str,argv)]
    head = subprocess.check_output(['git', '-C', str(options.checkout), 'rev-parse', 'HEAD'], text=True).strip()
    binaries = dict(plain=options.plain.resolve(), metrics=options.metrics.resolve())
    if options.candidate is not None: binaries['candidate']=options.candidate.resolve()
    identity = {name:dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())
                for name,path in binaries.items()}
    modes = [('plain-bare', 'plain', False, False, False),
             ('plain-observed', 'plain', False, False, True),
             ('metrics-silent', 'metrics', False, False, False),
             ('metrics-report', 'metrics', True, False, False),
             ('metrics-spans', 'metrics', True, True, False),
             ('metrics-observed', 'metrics', True, True, True)]
    if options.pair:
        assert 'candidate' in binaries
        modes=[('before-bare','plain',False,False,False),('after-bare','candidate',False,False,False)]
    fixture_begin = time.monotonic_ns()
    f = Fixture()
    try:
        entries = []
        for index in range(max(counts)):
            data = hashlib.shake_256(f'import-profile:{index}'.encode()).digest(options.blob_bytes)
            oid = f.git('hash-object', '-w', '--stdin', data=data)
            entries.append(f'100644 blob {oid}\tf{index:05d}\n')
        tree = f.git('mktree', data=''.join(entries).encode())
        commit = f.git('commit-tree', tree, data=b'instrumentation attribution fixture\n')
        f.git('update-ref', 'refs/heads/main', commit)
        f.git('repack', '-a', '-d')
        stat_bytes = stat_abi_bytes(f.root)
        refs = f.git('show-ref')
        options.output.parent.mkdir(parents=True, exist_ok=True)
        with options.output.open('x') as output:
            def record(row):
                output.write(json.dumps(row)+'\n'); output.flush()
                print(json.dumps(row),flush=True)
            record(dict(type='environment', head=head, working_tree_dirty=bool(subprocess.check_output(['git','-C',str(options.checkout),'diff','--name-only','HEAD'])), binaries=identity, backend=options.backend,
                source_files=max(counts), blob_bytes=options.blob_bytes, source_kind='local packed complete ODB',
                setup_elapsed_ns=time.monotonic_ns()-fixture_begin,
                source_cache='warm/uncontrolled; no flush/advice; same fixed complete ODB in every fresh invocation',
                filesystem=subprocess.check_output(['stat','-f','-c','%T',str(f.root)],text=True).strip(),
                kernel=os.uname().release, cpu_affinity=sorted(os.sched_getaffinity(0)),
                baseline='one fresh shared clone + identical command + cleanup per demand/trial; explicitly shared by the declared variants',
                modes=[mode[0] for mode in modes],
                timeout_wrapper='each measured command includes identical GNU timeout (TERM25s/KILL30s); this diagnostic is not acceptance'))
            for trial in range(options.trials):
                for count in counts:
                    command=[sys.executable,'-I','-S','-c',CHILD,str(count),str(options.blob_bytes),str(stat_bytes)]
                    def check(result):
                        observations = result.get('child_observations', result.get('command_observations'))
                        completed = [row for row in observations if 'completed' in row]
                        assert [row['completed'] for row in completed] == ['first','repeat']
                        assert all(row['files']==count and row['payload_bytes']==count*options.blob_bytes
                                   and row['metadata_result_bytes']==count*stat_bytes for row in completed)
                        assert not list(f.root.glob('git-on-*')), 'invocation storage leaked'
                        assert f.git('show-ref') == refs, 'source refs changed'
                    def baseline():
                        clone=f.root/'clone'
                        materialize=capture(bounded(['git','clone','--shared','--quiet',str(f.repo),str(clone)]),f.cwd,f.env,f.root)
                        direct=capture(bounded(command),clone,f.env,f.root)
                        begin=time.monotonic_ns();shutil.rmtree(clone);cleanup_ns=time.monotonic_ns()-begin
                        check(direct)
                        record(dict(type='baseline',trial=trial,files=count,materialize=materialize,command=direct,
                                    cleanup_ns=cleanup_ns,elapsed_ns=materialize['elapsed_ns']+direct['elapsed_ns']+cleanup_ns))
                    if trial%2 == 0: baseline()
                    order=list(modes);random.Random(7600+trial*512+count).shuffle(order)
                    for name,binary,report,spans,sampled in order:
                        env=dict(f.env)
                        if spans: env['GIT_ON_TEST_PROFILE_SPANS']='1'
                        policy='none' if options.backend=='fuse' else 'all'
                        argv=bounded([str(binaries[binary]),commit,'--write-policy='+policy,'--',*command])
                        before=resource.getrusage(resource.RUSAGE_SELF);begin=time.monotonic_ns()
                        if sampled:
                            result=observe(argv,f.repo,env,30,metrics=report,resource_observer=f.root/'resource-observer')
                        else:
                            result=capture(argv,f.repo,env,f.root,metrics=report,timing=report)
                        after=resource.getrusage(resource.RUSAGE_SELF)
                        enclosing_ns=time.monotonic_ns()-begin
                        check(result)
                        record(dict(type='runner',head=head,trial=trial,files=count,mode=name,
                            elapsed_ns=result['elapsed_ns'],enclosing_elapsed_ns=enclosing_ns,
                            observer_parent_user_s=after.ru_utime-before.ru_utime,
                            observer_parent_system_s=after.ru_stime-before.ru_stime,result=result))
                    if trial%2: baseline()
    finally:
        # Do not recursively remove a possibly mounted execution tree after an
        # unexpected timeout. The normal checked cleanup owns that decision.
        try:
            for directory in f.root.glob('git-on-*'):
                subprocess.run([str(binaries['plain']),'--cleanup',str(directory)],cwd=f.cwd,
                    env=f.env,capture_output=True,timeout=20,check=True)
            assert not list(f.root.glob('git-on-*')), 'execution tree remains'
        except BaseException:
            f.temp._finalizer.detach()
            print('retained failed fixture for checked cleanup: '+str(f.root),file=sys.stderr)
            raise
        f.close()
    assert not list(options.work.iterdir()), 'fixture/counters not cleaned'


if __name__ == '__main__':
    main()
