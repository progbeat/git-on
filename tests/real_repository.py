#!/usr/bin/env python3
"""Run a small upstream command on pinned public WebKit through real FUSE.

No source fixture, full clone, recursive manifest or materialization baseline is
prepared. Every invocation starts without downloaded source objects. Complete
invocation time includes network access, the command and cleanup. Network and
OS-cache conditions are uncontrolled; this is not the complete #20 acceptance.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import subprocess
import sys
import tempfile
import time

PIN = dict(repository='https://github.com/WebKit/WebKit.git',
    commit='4b125afe35a2e95bf764d7a5e2efe7dc574a0653',
    tree='be3bad9ff779597380c7ba741ff250ef8a14225f',
    blob_paths=472895, logical_blob_bytes=5522313781, gitlinks=0)
LIGHT_TESTS = ('webkitcorepy.tests.version_unittest', 'webkitcorepy.tests.string_utils_unittest',
    'webkitcorepy.tests.filtered_call_unittest')
LIGHT_TEST_COUNT = 32
# Workload budgets for this pinned command, not repository/product size limits.
# The command normally demands 35 blobs (293,353 bytes) and ten trees.
DEMAND_LIMITS = dict(tree_reads=32, prepared_tree_entries=4096, header_reads=128,
    blob_loads=128, blob_bytes=1024*1024, remote_raw_blob_imports=128,
    remote_raw_blob_bytes=1024*1024)
PACK_LIMIT = 1024*1024


def command():
    return [sys.executable, '-I', '-S', str(Path(__file__).resolve()), 'child']


def validate_test_output(code, output):
    if code or not re.search(rf'(?m)^Ran {LIGHT_TEST_COUNT} tests in [0-9.]+s$',output) or not output.strip().endswith('\nOK'):
        raise ValueError(f'upstream unit tests did not all pass: {code}\n{output}')


def child():
    tree = os.environ.get('GIT_ON_TREE_OID')
    if tree != PIN['tree']:
        raise ValueError('command did not run on the pinned WebKit tree')
    env = dict(os.environ, PYTHONPATH='Tools/Scripts/libraries/webkitcorepy',
        DISABLE_WEBKITCOREPY_AUTOINSTALLER='1')
    for phase in ('first', 'repeat'):
        begin = time.monotonic_ns()
        result = subprocess.run([sys.executable, '-B', '-S', '-m', 'unittest', *LIGHT_TESTS],
            env=env, capture_output=True, text=True, check=False)
        validate_test_output(result.returncode, result.stderr)
        print(json.dumps(dict(completed=phase, tree_oid=tree, test_count=LIGHT_TEST_COUNT,
            test_modules=LIGHT_TESTS, test_output=result.stderr,
            elapsed_ns=time.monotonic_ns()-begin)), flush=True)


def validate_reports(data, head):
    """Reject stale evidence, incomplete commands and eager source acquisition."""
    from benchmark import snapshot_metrics
    if len(data) != 1:
        raise ValueError('expected one sparse public WebKit invocation')
    row = data[0]
    if (row.get('type') != 'real_repository' or row.get('head') != head
            or row.get('working_tree_dirty') is not False or row.get('pin') != PIN):
        raise ValueError('stale, dirty or wrong real repository identity')
    runner = row['runner']
    argv = runner['argv']
    if argv[1:5] != [PIN['repository'], PIN['commit'], '--write-policy=none', '--'] or argv[5:] != command():
        raise ValueError('wrong public-source command')
    observed = runner['child_observations']
    if [r['completed'] for r in observed] != ['first', 'repeat']:
        raise ValueError('missing or duplicate command pass')
    for result in observed:
        if (result['tree_oid'] != PIN['tree'] or result['test_count'] != LIGHT_TEST_COUNT
                or tuple(result['test_modules']) != LIGHT_TESTS
                or type(result['elapsed_ns']) is not int or result['elapsed_ns'] <= 0):
            raise ValueError('wrong upstream command or missing pass timing')
        validate_test_output(0, result['test_output'])
    elapsed = runner['elapsed_ns']
    if type(elapsed) is not int or elapsed < sum(r['elapsed_ns'] for r in observed):
        raise ValueError('missing complete invocation timing')
    for name in ('waited_user_cpu_s', 'waited_system_cpu_s'):
        value = runner.get(name)
        if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
            raise ValueError('missing complete descendant CPU accounting')
    metrics = runner['metrics']
    snapshot_metrics(metrics)
    for name, limit in DEMAND_LIMITS.items():
        value = metrics.get(name)
        if type(value) is not int or not 0 <= value <= limit:
            raise ValueError(f'excessive or missing sparse-workload demand: {name}')
    for name in ('remote_native_pack_bytes', 'remote_session_pack_bytes'):
        if type(metrics.get(name)) is not int or metrics[name] < 0:
            raise ValueError('missing received pack accounting')
    received = metrics['remote_native_pack_bytes'] + metrics['remote_session_pack_bytes']
    if not 0 < received <= PACK_LIMIT:
        raise ValueError('public command transferred too much source data')
    if (not 0 < metrics['blob_loads'] == metrics['remote_raw_blob_imports']
            or not 0 < metrics['blob_bytes'] == metrics['remote_raw_blob_bytes']
            or type(metrics['fuse_requests'].get('read')) is not int
            or metrics['fuse_requests']['read'] <= 0):
        raise ValueError('missing real FUSE reads or repeated payload preparation')
    return dict(public_https_invocations=1, passes=2, upstream_tests_per_pass=LIGHT_TEST_COUNT,
        elapsed_ns=elapsed, blob_loads=metrics['blob_loads'], received_pack_bytes=received)


def run(options):
    from benchmark_support import source_identity
    from benchmark import environment
    from process_capture import capture
    if not (sys.platform == 'linux' and Path('/dev/fuse').is_char_device() and shutil.which('fusermount3')):
        raise RuntimeError('real Linux FUSE required')
    identity=source_identity()
    head,dirty=identity['head'],identity['working_tree_dirty']
    binary = options.binary.resolve()
    with tempfile.TemporaryDirectory(prefix='git-on-webkit-') as temporary:
        root = Path(temporary)
        argv = [str(binary), PIN['repository'], PIN['commit'], '--write-policy=none', '--', *command()]
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        begin = time.monotonic_ns()
        result = capture(argv, root, dict(environment(), TMPDIR=temporary), root, metrics=True)
        elapsed = time.monotonic_ns()-begin
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        if list(root.iterdir()):
            raise ValueError('invocation storage was not cleaned up')
    result.update(argv=argv, elapsed_ns=elapsed, waited_user_cpu_s=after.ru_utime-before.ru_utime,
        waited_system_cpu_s=after.ru_stime-before.ru_stime)
    row = dict(type='real_repository', head=head, working_tree_dirty=dirty, pin=PIN,
        binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), runner=result,
        source_cache='fresh invocation; public HTTPS; no prepared source; OS/network caches uncontrolled')
    options.output.parent.mkdir(parents=True, exist_ok=True)
    # Retain actual observations even if the demand gate rejects them.
    options.output.write_text(json.dumps(row)+'\n')
    validated = validate_reports([row], head)
    print(json.dumps(dict(head=head, **validated)), flush=True)
    if options.summary:
        m = result['metrics']
        options.summary.write_text(
            '### Sparse command on public WebKit\n\n'
            f'Pinned source: {PIN["blob_paths"]:,} blob paths, {PIN["logical_blob_bytes"]:,} logical bytes. '
            'No full clone, checkout or source preparation.\n\n'
            '| Measurement | Result |\n| --- | ---: |\n'
            f'| Complete invocation, including network and cleanup | {elapsed/1e9:.3f} s |\n'
            f'| Upstream tests | {LIGHT_TEST_COUNT} × 2 passes |\n'
            f'| Decoded blobs | {m["blob_loads"]} |\n'
            f'| Decoded payload | {m["blob_bytes"]:,} bytes |\n'
            f'| Received Git pack payload | {validated["received_pack_bytes"]:,} bytes |\n\n'
            'Received pack bytes exclude protocol/TLS framing. Source-size figures describe the pinned revision; '
            'this test does not scan or download it in full. Network latency is uncontrolled; '
            'no full-clone speed ratio or completion of #20 is claimed.\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='action', required=True)
    execute = commands.add_parser('run')
    execute.add_argument('--binary', type=Path, required=True)
    execute.add_argument('--output', type=Path, required=True)
    execute.add_argument('--summary', type=Path)
    commands.add_parser('child')
    options = parser.parse_args()
    if options.action == 'run':
        run(options)
    else:
        child()


if __name__ == '__main__':
    main()
