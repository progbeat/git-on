#!/usr/bin/env python3
"""Measure repeated direct HTTPS reads and trace private pack filesystem work."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time
from benchmark_support import source_identity
from remote import Fixture, git_invocations, record_git_invocations


def exercise(binary, count):
    f = Fixture()
    try:
        values = [f'object-{index}\n' for index in range(count)]
        oids = [f.git('hash-object', '-w', '--stdin', data=value.encode()) for value in values]
        entries = ''.join(f'100644 blob {oid}\tf{index:04d}\n'
                          for index, oid in enumerate(oids))
        tree = f.git('mktree', data=entries.encode())
        commit = f.git('commit-tree', tree, data=b'distinct demand fixture\n')
        f.git('update-ref', 'refs/heads/main', commit)
        url = f.https()
        environment, invocations = record_git_invocations(f.root, f.env)
        trace = f.root / 'trace'
        argv = ['strace', '-f', '-s', '512', '-e', 'trace=%file', '-o', str(trace),
                str(binary.resolve()), url, 'HEAD', '--metrics-only']
        commands = (''.join(f'read f{index:04d}\n' for index in range(count)) * 2
                    + 'report\nexit\n').encode()
        begin = time.monotonic_ns()
        result = subprocess.run(argv, cwd=f.cwd, env=environment, input=commands, capture_output=True,
                                check=True, timeout=90)
        elapsed = time.monotonic_ns() - begin
        rows = [json.loads(row) for row in result.stdout.splitlines()]
        assert [row['data'] for row in rows if 'data' in row] == values * 2
        reports = [row for row in rows if 'metrics' in row]
        second = reports[-1]['metrics']
        assert all('objects' not in row for row in reports[:-1])
        assert set(reports[-1]['objects']) == {commit, tree, *oids}
        # A read can return verified bytes just before the pack trailer is
        # counted. The server's request log counts actual network work.
        received = [oid for request in f.requests for oid in request['wants']]
        assert sorted(received) == sorted([commit, tree, *oids]), received
        assert second['blob_loads'] == count, second
        assert second['remote_fetch_operations'] == second['remote_repositories_created'] == 0, second
        assert second['remote_session_starts'] == 1, second
        assert not any('fetch' in args or 'cat-file' in args for args in git_invocations(invocations))
        pack_accesses = sum(any(name in line for name in ('remote-repository',
                            'received-pack-', 'objects/pack', 'pack-staging'))
                            for line in trace.read_text().splitlines())
        assert pack_accesses == 0, 'Git-on touched private pack storage'
        return dict(binary=str(binary), distinct_blobs=count, passes=2,
                    pack_metadata_syscalls=pack_accesses, requested_objects=len(received),
                    elapsed_ns=elapsed, metrics=second)
    finally:f.close()

def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe',type=Path,required=True)
    parser.add_argument('--sizes',default='32,128,512')
    parser.add_argument('--output',type=Path,default=Path('pack-scaling.jsonl'))
    options=parser.parse_args(argv)
    assert sys.platform=='linux' and shutil.which('strace'),'Linux strace required'
    identity=source_identity()
    with options.output.open('w') as output:
        for count in map(int,options.sizes.split(',')):
            row = dict(exercise(options.probe, count), **identity)
            output.write(json.dumps(row) + '\n')
            output.flush()
            print(json.dumps(row), flush=True)



if __name__=='__main__':main()
