#!/usr/bin/env python3
"""Pinned real-repository coverage, with full materialization and direct baselines.

The immutable source is acquired once, outside the measured invocations. Each
runner owns fresh execution storage. OS caches are deliberately not flushed and
are reported as uncontrolled; 'first' means first access in this invocation.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import resource
import signal
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time

PIN = dict(repository='https://github.com/WebKit/WebKit.git',
    commit='4b125afe35a2e95bf764d7a5e2efe7dc574a0653',
    tree='be3bad9ff779597380c7ba741ff250ef8a14225f',
    blob_paths=472895, logical_blob_bytes=5522313781, gitlinks=0)
MODULES = 'Tools/Scripts/webkitpy/style/'
LIGHT_TESTS = ('webkitcorepy.tests.version_unittest', 'webkitcorepy.tests.string_utils_unittest',
    'webkitcorepy.tests.filtered_call_unittest')


def environment():
    from benchmark import environment as isolated_environment
    return isolated_environment()


def is_module(path):
    # Test input fixtures intentionally include invalid Python; compile the
    # production checker package, not those parser/style-test inputs.
    return path.startswith(MODULES.encode()) and path.endswith(b'.py') and b'_unittest' not in path.rsplit(b'/', 1)[-1]


def git(repo, *args, **kwargs):
    return subprocess.check_output(['git', '-C', str(repo), *args], env=environment(), **kwargs)


def manifest(repo, revision, destination):
    """Keep byte-exact names; never materialize a Python object per source path."""
    result = dict(blob_paths=0, logical_blob_bytes=0, gitlinks=0, directories=0,
        executable_paths=0, symlink_paths=0, python_modules=0, syntax_path_bytes=0)
    blobs={}; modules={}
    with destination.open('wb') as output, tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(['git', '-C', str(repo), 'ls-tree', '-r', '-t', '-l', '-z', revision],
            env=environment(), stdout=output, stderr=errors)
        if process.wait():
            errors.seek(0)
            raise RuntimeError(errors.read().decode(errors='replace'))
    with destination.with_name('syntax.manifest').open('wb') as selected:
        for entry in entries(destination):
            mode, kind, oid, size, name = entry
            if kind == 'blob':
                blobs[oid]=size
                result['blob_paths'] += 1
                result['logical_blob_bytes'] += size
                result['executable_paths'] += mode == '100755'
                result['symlink_paths'] += mode == '120000'
                result['python_modules'] += is_module(name) and mode != '120000'
                if is_module(name) and mode != '120000':
                    modules[oid]=size;result['syntax_path_bytes']+=size
                    selected.write(f'{mode} {kind} {oid} {size}\t'.encode()+name+b'\0')
            elif kind == 'tree': result['directories'] += 1
            elif kind == 'commit': result['gitlinks'] += 1
            else: raise ValueError('unsupported manifest object kind')
    result.update(unique_blob_objects=len(blobs), unique_blob_bytes=sum(blobs.values()),
        nonempty_unique_blob_objects=sum(size>0 for size in blobs.values()),
        syntax_unique_blobs=len(modules), syntax_blob_bytes=sum(modules.values()),
        syntax_nonempty_unique_blobs=sum(size>0 for size in modules.values()))
    return result


def entries(path):
    """Stream Git's NUL records, including embedded newlines/non-UTF8 names."""
    with open(path, 'rb') as source:
        remaining = b''
        while chunk := source.read(65536):
            records = (remaining + chunk).split(b'\0')
            remaining = records.pop()
            for record in records:
                header, name = record.split(b'\t', 1)
                mode, kind, oid, size = header.split()
                if not name or b'\0' in name or name.startswith(b'/') or any(p in (b'', b'.', b'..') for p in name.split(b'/')):
                    raise ValueError('unsafe manifest path')
                yield mode.decode(), kind.decode(), oid.decode(), int(size) if size != b'-' else 0, name
        if remaining: raise ValueError('truncated manifest')


def prepare(directory, pin=PIN):
    directory.mkdir(parents=True, exist_ok=True)
    repo = directory/'repository.git'
    if not repo.exists():
        subprocess.run(['git', 'init', '--bare', '--quiet', '--template=', str(repo)], env=environment(), check=True)
    # A corrupt/incomplete cache cannot turn into a different revision. Fetch
    # only the selected history depth; no dependency bootstrap or LFS hydration.
    if subprocess.run(['git', '-C', str(repo), 'cat-file', '-e', pin['commit']+'^{commit}'],
            env=environment(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
        subprocess.run(['git', '-C', str(repo), 'fetch', '--quiet', '--depth=1', '--no-tags',
            pin['repository'], pin['commit']], env=environment(), check=True)
    actual = git(repo, 'rev-parse', pin['commit']+'^{tree}').strip().decode()
    if actual != pin['tree']: raise ValueError('pinned root tree mismatch')
    git(repo, 'fsck', '--full', '--no-reflogs', '--no-dangling', stderr=subprocess.PIPE)
    git(repo, 'update-ref', 'refs/heads/fixture', pin['commit'])
    git(repo, 'symbolic-ref', 'HEAD', 'refs/heads/fixture')
    # Only the private test server is configured; caller repositories and Git
    # configuration are untouched. The native source permits reachable demands.
    git(repo, 'config', 'uploadpack.allowFilter', 'true')
    git(repo, 'config', 'uploadpack.allowReachableSHA1InWant', 'true')
    git(repo, 'config', 'gc.auto', '0')
    counts = manifest(repo, pin['commit'], directory/'tree.manifest')
    for key in ('blob_paths', 'logical_blob_bytes', 'gitlinks'):
        if counts[key] != pin[key]: raise ValueError(f'pinned {key} mismatch: {counts[key]}')
    if counts['gitlinks']: raise ValueError('fixture scope must have no gitlinks')
    value = dict(pin=pin, counts=counts, scope='one Git repository; raw Git blobs; no external dependencies or LFS hydration',
        os_cache='uncontrolled; no privileged cache flush', git_version=git(repo, '--version').decode().strip())
    (directory/'fixture.json').write_text(json.dumps(value, indent=2)+'\n')
    return value


def process_snapshot(pid):
    """Simultaneous RSS sum of observable descendants; shared pages count twice.

    Short-lived children may fall between samples. CPU uses waited accounting,
    not these samples. PID identity is checked before/after reading each status.
    """
    seen = set(); queue = [pid]; rss = 0; descriptors = 0; complete = True; fd_complete = True
    while queue:
        current = queue.pop()
        if current in seen: continue
        seen.add(current)
        base = Path('/proc')/str(current)
        try:
            identity = (base/'stat').read_text().rsplit(')', 1)[1].split()[19]
            status = (base/'status').read_text()
            resident = next((int(line.split()[1]) for line in status.splitlines() if line.startswith('VmRSS:')), 0)
            try: count = len(list((base/'fd').iterdir()))
            except PermissionError: count = 0; fd_complete = False
            children = []
            for task in (base/'task').iterdir():
                try: children.extend((task/'children').read_text().split())
                except FileNotFoundError:
                    if task.exists(): complete = False
            if (base/'stat').read_text().rsplit(')', 1)[1].split()[19] != identity: continue
            rss += resident; descriptors += count
            queue.extend(map(int, children))
        except (FileNotFoundError, ProcessLookupError): pass
    return rss, descriptors, complete, fd_complete


def observe(argv, cwd, env, timeout, metrics=False, resource_observer=None):
    """Include launch, native descendants and process exit; no traced timings."""
    with tempfile.TemporaryFile() as counters:
        if resource_observer:
            rss_output=resource_observer.with_name('observed-rss')
            argv=[str(resource_observer),str(rss_output),*argv]
        if metrics: env = dict(env, GIT_ON_TEST_METRICS_FD=str(counters.fileno()))
        before = resource.getrusage(resource.RUSAGE_CHILDREN); begin = time.monotonic_ns()
        # These controlled commands emit bounded JSON/quiet Git diagnostics.
        # Pipe EOF wakes communicate on exit; wait(timeout) alone busy-polls
        # and can add a 50 ms measurement floor to short commands.
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,start_new_session=True,
            pass_fds=(counters.fileno(),) if metrics else ())
        stop = threading.Event(); samples = dict(count=0, peak_rss_sum_kib=0, peak_fd_sum=0, unavailable=False,descriptors_unavailable=False)
        def sample():
            while not stop.wait(.02):
                try:
                    rss, fds, complete, fd_complete = process_snapshot(process.pid)
                    samples['peak_rss_sum_kib'] = max(samples['peak_rss_sum_kib'], rss)
                    samples['peak_fd_sum'] = max(samples['peak_fd_sum'], fds)
                    samples['count'] += int(rss>0)
                    samples['unavailable'] |= not complete
                    samples['descriptors_unavailable'] |= not fd_complete
                except PermissionError: samples['unavailable'] = True
        thread = threading.Thread(target=sample); thread.start()
        try:
            output,errors=process.communicate(timeout=timeout)
            code=process.returncode
        except BaseException:
            # The root has not been reaped at a timeout, so its process-group
            # identity cannot have been reused. Stop descendants before wait.
            if process.returncode is None:
                try: os.killpg(process.pid,signal.SIGTERM)
                except ProcessLookupError: pass
                deadline=time.monotonic()+10
                while time.monotonic()<deadline:
                    info=os.waitid(os.P_PID,process.pid,os.WEXITED|os.WNOHANG|os.WNOWAIT)
                    if info is not None: break
                    time.sleep(.01)
                # Even an already-exited root stays unreaped until this final
                # stop, including children that ignored the graceful signal.
                try: os.killpg(process.pid,signal.SIGKILL)
                except ProcessLookupError: pass
                process.wait()
            for stream in (process.stdout,process.stderr):
                if stream is not None: stream.close()
            raise
        finally: stop.set(); thread.join()
        elapsed = time.monotonic_ns()-begin; after = resource.getrusage(resource.RUSAGE_CHILDREN)
        if code: raise RuntimeError(f'command exited {code}: {errors[-65536:].decode(errors="replace")}')
        rows = [json.loads(line) for line in output.splitlines()]
        value = dict(elapsed_ns=elapsed, waited_user_cpu_s=after.ru_utime-before.ru_utime,
            waited_system_cpu_s=after.ru_stime-before.ru_stime, memory=samples,
            waited_input_block_bytes=(after.ru_inblock-before.ru_inblock)*512,
            waited_output_block_bytes=(after.ru_oublock-before.ru_oublock)*512,
            waited_process_peak_rss_kib=int(rss_output.read_text()) if resource_observer else None,
            memory_limits='wait4 peak is the largest waited process, including native descendants; 20 ms process-tree RSS sums are non-atomic, shared pages double counted, fast/protected descendants may be missed',
            stderr=errors.decode(errors='replace'), command_observations=rows)
        if metrics:
            from benchmark import validate_cache_budgets
            counters.seek(0); value['metrics'] = json.load(counters)
            validate_cache_budgets(value['metrics'])
        return value


def command(manifest_path, workload, stat_bytes):
    if workload == 'syntax': manifest_path=manifest_path.with_name('syntax.manifest')
    return [sys.executable, '-I', '-S', str(Path(__file__).resolve()), 'child',
        '--manifest', str(manifest_path), '--workload', workload, '--stat-bytes', str(stat_bytes)]


def child(manifest_path, workload, stat_bytes):
    """Syntax-check the real style package, or verify the complete namespace/data."""
    import ast
    def walk(root=b''):
        with os.scandir(root or b'.') as stream:
            listing = sorted(stream, key=lambda e: e.name+(b'/' if e.is_dir(follow_symlinks=False) else b''))
        for entry in listing:
            if not root and entry.name == b'.git' and 'GIT_ON_TREE_OID' not in os.environ: continue
            path = root+b'/'+entry.name if root else entry.name
            yield path
            if entry.is_dir(follow_symlinks=False): yield from walk(path)
    for phase in ('first', 'repeat'):
        begin = time.monotonic_ns(); files = 0; payload = 0; metadata = 0; syntax = 0
        if workload == 'tests':
            env=dict(os.environ,PYTHONPATH='Tools/Scripts/libraries/webkitcorepy',DISABLE_WEBKITCOREPY_AUTOINSTALLER='1')
            result=subprocess.run([sys.executable,'-B','-S','-m','unittest',*LIGHT_TESTS],env=env,capture_output=True,text=True)
            validate_test_output(result.returncode,result.stderr)
            print(json.dumps(dict(completed=phase,workload=workload,test_count=32,test_modules=LIGHT_TESTS,
                test_output=result.stderr,elapsed_ns=time.monotonic_ns()-begin)),flush=True)
            continue
        if workload == 'syntax':
            paths=list(Path(MODULES).rglob('*.py'));selected=[]
            # Fixed rglob-result representation: path_length:u32 + raw path.
            metadata += sum(4+len(os.fsencode(path)) for path in paths)
            for path in paths:
                if '_unittest' in path.name: continue
                attributes=path.lstat();metadata += stat_bytes
                if not stat.S_ISLNK(attributes.st_mode): selected.append(os.fsencode(path))
            expected=sorted(row[-1] for row in entries(manifest_path))
            assert sorted(selected)==expected, 'unexpected/missing production Python module'
        actual = iter(walk()) if workload == 'full' else None
        for mode, kind, oid, size, path in entries(manifest_path):
            if workload == 'syntax' and not (kind == 'blob' and mode != '120000' and is_module(path)): continue
            if actual is not None:
                observed = next(actual, None)
                assert observed == path, (observed, path)
                # Fixed result representation: inode:u64, type:u8,
                # name_length:u32, raw basename bytes, in addition to lstat ABI.
                metadata += 13+len(path.rsplit(b'/', 1)[-1])
            attributes = os.lstat(path); metadata += stat_bytes
            if kind == 'tree':
                assert stat.S_ISDIR(attributes.st_mode), path
                continue
            assert kind == 'blob' and attributes.st_size == size, path
            hashed = hashlib.sha1(f'blob {size}\0'.encode())
            if mode == '120000':
                assert stat.S_ISLNK(attributes.st_mode), path
                data = os.readlink(path); hashed.update(data); payload += len(data)
            else:
                assert stat.S_ISREG(attributes.st_mode), path
                assert bool(attributes.st_mode & 0o111) == (mode == '100755'), path
                with open(path, 'rb') as source:
                    if workload == 'syntax':
                        data = source.read(); hashed.update(data); payload += len(data)
                        ast.parse(data, filename=os.fsdecode(path)); syntax += 1
                    else:
                        while data := source.read(1024*1024): hashed.update(data); payload += len(data)
            assert hashed.hexdigest() == oid, path
            files += 1
        if actual is not None: assert next(actual, None) is None, 'unexpected final namespace entry'
        assert files > 0, 'empty workload'
        print(json.dumps(dict(completed=phase, workload=workload, files=files, syntax_modules=syntax,
            payload_bytes=payload, metadata_result_bytes=metadata, elapsed_ns=time.monotonic_ns()-begin)), flush=True)


def validate_test_output(code, output):
    if code or not re.search(r'(?m)^Ran 32 tests in [0-9.]+s$',output) or not output.strip().endswith('\nOK'):
        raise ValueError(f'upstream unit tests did not all pass: {code}\n{output}')


def validate_reports(data, head, pin=PIN, trials=3):
    """Fail closed on stale heads, missing work or incomplete baseline accounting."""
    from benchmark import validate_cache_budgets
    baselines=[r for r in data if r.get('type')=='real_repository_baseline']
    runs=[r for r in data if r.get('type')=='real_repository']
    if len(data)!=2+1+2*trials or len(baselines)!=2 or {r['source'] for r in baselines}!={'local','remote'}:
        raise ValueError('missing/duplicate real repository baselines')
    wanted={('full','local',0)}|{('tests',source,trial) for source in ('local','remote') for trial in range(trials)}
    if len(runs)!=len(wanted) or {(r['workload'],r['source'],r['trial']) for r in runs}!=wanted:
        raise ValueError('missing/duplicate real repository commands')
    for row in data:
        if row.get('head')!=head or row.get('working_tree_dirty') is not False or row['fixture']['pin']!=pin:
            raise ValueError('stale/dirty real repository identity')
        counts=row['fixture']['counts']
        if any(counts.get(k)!=pin[k] for k in ('blob_paths','logical_blob_bytes','gitlinks')):
            raise ValueError('wrong real repository scope')
    by_source={r['source']:r for r in baselines}
    for row in runs:
        baseline=row['baseline']; runner=row['runner']; counts=row['fixture']['counts']
        common=by_source[row['source']]
        if any(baseline.get(k)!=common[k] for k in ('materialize','cleanup_ns','shared_materialization_samples','stored_received_pack_bytes')):
            raise ValueError('baseline components differ from measured source')
        if baseline['shared_materialization_samples']!=1 or len(baseline['materialize']['commands'])!=2:
            raise ValueError('ambiguous baseline preparation')
        if baseline['total_elapsed_ns']!=baseline['materialize']['elapsed_ns']+baseline['direct_command']['elapsed_ns']+baseline['cleanup_ns']:
            raise ValueError('baseline omitted setup or cleanup')
        observed=runner['command_observations']; direct=baseline['direct_command']['command_observations']
        strip=lambda rr:[{k:v for k,v in r.items() if k not in ('elapsed_ns','test_output')} for r in rr]
        if strip(observed)!=strip(direct) or [r['completed'] for r in observed]!=['first','repeat']:
            raise ValueError('incomplete/mismatched command execution')
        if any(r['workload']!=row['workload'] or r['elapsed_ns']<=0 for r in observed):
            raise ValueError('missing command metadata/timings')
        full=row['workload']=='full'
        if full:
            if any(r['files']!=pin['blob_paths'] or r['metadata_result_bytes']<=0 or r['payload_bytes']!=pin['logical_blob_bytes'] for r in observed):
                raise ValueError('incomplete full namespace/payload read')
        else:
            for result in observed+direct:
                if result['test_count']!=32 or tuple(result['test_modules'])!=LIGHT_TESTS:raise ValueError('wrong upstream test selection')
                validate_test_output(0,result['test_output'])
        m=runner['metrics']; validate_cache_budgets(m)
        if (full and m['blob_loads']!=counts['nonempty_unique_blob_objects']) or not 0<m['blob_loads']<=counts['nonempty_unique_blob_objects'] or m['fuse_requests'].get('read',0)<=0:
            raise ValueError('repeated decode or missing real FUSE execution')
        transfer=row.get('transfer_evidence',{})
        transfer_fields=dict(remote_verified_blob_bytes='remote_raw_blob_bytes',remote_verified_blobs='remote_raw_blob_imports',
            session_pack_payload_bytes='remote_session_pack_bytes',native_stored_received_pack_bytes='remote_native_pack_bytes')
        if any(type(m.get(metric)) is not int or m[metric]<0 or transfer.get(field)!=m[metric] for field,metric in transfer_fields.items()):
            raise ValueError('missing/inconsistent received-data evidence')
        if row['source']=='remote':
            if m['remote_session_pack_bytes']+m['remote_native_pack_bytes']<=0 or m['remote_raw_blob_bytes']<=0:
                raise ValueError('remote transfer was not observed')
            if m['remote_raw_blob_imports']>=counts['unique_blob_objects']:
                raise ValueError('test workload fetched the complete source')
        for index,result in enumerate([runner,baseline['direct_command'],*baseline['materialize']['commands']]):
            if result['elapsed_ns']<=0 or any(result.get(k,-1)<0 for k in ('waited_user_cpu_s','waited_system_cpu_s','waited_input_block_bytes','waited_output_block_bytes')):
                raise ValueError('missing complete process resources')
            memory=result['memory']
            # Fast setup helpers may exit between samples; never invent their
            # peak. Runner/command and at least one preparation must be seen.
            if type(result.get('waited_process_peak_rss_kib')) is not int or result['waited_process_peak_rss_kib']<=0:
                raise ValueError('required waited-process peak RSS unavailable')
            if type(memory.get('unavailable')) is not bool or (index<2 and (memory['count']<=0 or memory['peak_rss_sum_kib']<=0)):
                raise ValueError('required real repository memory observation unavailable')
        if not any(r['memory']['count']>0 and r['memory']['peak_rss_sum_kib']>0 for r in baseline['materialize']['commands']):
            raise ValueError('preparation memory was not observed')
    partial=sum(r['runner']['memory']['unavailable'] for r in runs)
    return dict(pin=pin['commit'],full_local_passes=2,test_invocations=2*trials,upstream_tests_per_pass=32,baseline_materializations=2,
        partial_process_tree_rss_samples=partial,memory_primary='wait4 largest waited process peak, not aggregate physical memory')


def run(options):
    from import_profile import stat_abi_bytes
    assert sys.platform == 'linux' and Path('/dev/fuse').is_char_device() and shutil.which('fusermount3'), 'real Linux FUSE required'
    directory = options.fixture.resolve(); metadata = json.loads((directory/'fixture.json').read_text())
    assert metadata['pin'] == PIN, 'unrecognized fixture pin'
    repo = directory/'repository.git'; tree = directory/'tree.manifest'
    assert git(repo, 'rev-parse', PIN['commit']+'^{tree}').strip().decode() == PIN['tree']
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    dirty = bool(subprocess.check_output(['git', 'status', '--porcelain']))
    options.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='git-on-real-benchmark-') as temporary, options.output.open('w') as output:
        root = Path(temporary); stat_bytes = stat_abi_bytes(root); env = environment()
        env.update(TMPDIR=str(root), GIT_LFS_SKIP_SMUDGE='1')
        def measured(argv,cwd,env,timeout,metrics=False):
            return observe(argv,cwd,env,timeout,metrics,resource_observer=root/'resource-observer')
        def record(row):
            row.update(head=head, working_tree_dirty=dirty, fixture=metadata, source_cache='complete immutable local ODB; OS caches uncontrolled; fresh invocation storage',
                source_scope='local or file:// private server; not a public-network latency benchmark')
            output.write(json.dumps(row)+'\n'); output.flush(); print(json.dumps(row), flush=True)
        plan = [('full', 'local', 0)] + [('tests', source, trial) for trial in range(options.trials) for source in ('local', 'remote')]
        random.Random(260531).shuffle(plan)
        baselines={}
        # One complete materialization per source, then independent direct
        # commands on that readonly copy. Reuse is explicit: composed baseline
        # totals are not represented as independent clone timing samples.
        for source in ('local','remote'):
            clone=root/'baseline'; start=time.monotonic_ns()
            clone_argv=['git','clone','--quiet','--no-checkout']
            clone_argv += ['--shared',str(repo)] if source == 'local' else [repo.as_uri()]
            preparation=[measured([*clone_argv,str(clone)],root,env,options.timeout)]
            (clone/'.git/info/attributes').write_text('** -text -ident -filter -working-tree-encoding\n')
            preparation.append(measured(['git','-C',str(clone),'checkout','--quiet','--detach',PIN['commit']],root,env,options.timeout))
            prep=dict(elapsed_ns=time.monotonic_ns()-start,commands=preparation)
            direct={}
            for workload,selected_source,trial in plan:
                if selected_source == source:
                    direct[workload,trial]=measured(command(tree,workload,stat_bytes),clone,env,options.timeout)
            received_pack_bytes=sum(p.stat().st_size for p in (clone/'.git/objects/pack').glob('*.pack'))
            start=time.monotonic_ns();cpu=time.process_time_ns();shutil.rmtree(clone);cleanup=time.monotonic_ns()-start
            common=dict(materialize=prep,cleanup_ns=cleanup,cleanup_parent_cpu_ns=time.process_time_ns()-cpu,
                stored_received_pack_bytes=received_pack_bytes,shared_materialization_samples=1,
                method='one native same-source clone/checkout and cleanup per source; these measured components are reused in composed per-command totals')
            record(dict(type='real_repository_baseline',source=source,**common))
            for (workload,trial),value in direct.items():
                baselines[workload,source,trial]=dict(common,direct_command=value,
                    total_elapsed_ns=prep['elapsed_ns']+value['elapsed_ns']+cleanup)
        for order, (workload, source, trial) in enumerate(plan):
            child_argv = command(tree, workload, stat_bytes)
            def runner():
                argv = [str(options.binary.resolve())]
                if source == 'remote': argv.append(repo.as_uri())
                argv += [PIN['commit'], '--write-policy=none', '--', *child_argv]
                return measured(argv, repo if source == 'local' else root, env, options.timeout, metrics=True)
            before=baselines[workload,source,trial];after=runner()
            expected = [{k:v for k,v in row.items() if k not in ('elapsed_ns','test_output')} for row in before['direct_command']['command_observations']]
            observed = [{k:v for k,v in row.items() if k not in ('elapsed_ns','test_output')} for row in after['command_observations']]
            assert expected == observed and [r['completed'] for r in observed] == ['first','repeat'], 'baseline command differs'
            m=after['metrics']
            if workload=='full':assert m['blob_loads']==metadata['counts']['nonempty_unique_blob_objects'],'prepared data decoded repeatedly or required data missing'
            assert m['fuse_requests'].get('read', 0) > 0, 'no actual FUSE reads'
            if workload == 'tests':
                assert all(r['test_count']==32 for r in observed)
                assert m['blob_loads'] < metadata['counts']['blob_paths'], 'small workload materialized whole tree'
            else: assert all(r['files'] == PIN['blob_paths'] and r['payload_bytes'] == PIN['logical_blob_bytes'] for r in observed)
            assert not list(root.glob('git-on-*')), 'invocation cleanup incomplete'
            record(dict(type='real_repository', workload=workload, source=source, trial=trial, order=order,
                baseline=before, runner=after, method='full two-pass real namespace/data; 32 upstream webkitcorepy unit tests through stdlib unittest without SDK/autoinstall',
                transfer_evidence=dict(remote_verified_blob_bytes=m.get('remote_raw_blob_bytes', 0),
                    remote_verified_blobs=m.get('remote_raw_blob_imports', 0),
                    session_pack_payload_bytes=m.get('remote_session_pack_bytes',0),
                    native_stored_received_pack_bytes=m.get('remote_native_pack_bytes',0),
                    limit='session packs are exact compressed Git payloads; native packs may be index-pack normalized; neither includes framing/TLS/wire overhead')))


def main():
    parser=argparse.ArgumentParser(description=__doc__); commands=parser.add_subparsers(dest='action', required=True)
    fixture=commands.add_parser('prepare'); fixture.add_argument('--directory', type=Path, required=True)
    execute=commands.add_parser('run'); execute.add_argument('--fixture', type=Path, required=True); execute.add_argument('--binary', type=Path, required=True)
    execute.add_argument('--trials', type=int, default=3); execute.add_argument('--timeout', type=int, default=1200); execute.add_argument('--output', type=Path, required=True)
    child_parser=commands.add_parser('child'); child_parser.add_argument('--manifest', type=Path, required=True)
    child_parser.add_argument('--workload', choices=['full','syntax','tests'], required=True); child_parser.add_argument('--stat-bytes', type=int, required=True)
    options=parser.parse_args()
    if options.action == 'prepare': print(json.dumps(prepare(options.directory)), flush=True)
    elif options.action == 'child': child(options.manifest, options.workload, options.stat_bytes)
    else: run(options)


if __name__ == '__main__': main()
