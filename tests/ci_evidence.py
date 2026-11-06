"""Validate completed coverage, not just green jobs, for the required CI gate."""
from collections import Counter
import json
import math
from pathlib import Path
from scalability import expected_demands, scenario_plan
from benchmark import validate_cache_budgets


def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def case_key(row):return tuple(row[k] for k in ('files','policy','verify','workload','invocation'))


def validate_scale(plan,root):
    expected={}
    for group in plan['scale_matrix']['include']:
        host='macos' if group['runner'].startswith('macos') else 'linux'
        for source in group['sources'].split(','):
            expected[host,group['backend'],source,group['profile']]=group['policies'].split(',')
    for source in ('local','remote'):expected['linux','passthrough',source,plan['profile']]=['ignored']
    observed={};total=Counter()
    for path in root.rglob('scale-*.jsonl'):
        data=rows(path);environments=[r for r in data if r.get('type')=='environment']
        if len(environments)!=1:raise ValueError(f'missing/duplicate scale environment: {path}')
        env=environments[0]
        host='macos' if env['platform'].lower().startswith('macos') else 'linux' if env['platform'].lower().startswith('linux') else 'unknown'
        key=(host,env['backend'],env['source'],env['profile'])
        if key not in expected or key in observed:raise ValueError(f'unexpected/duplicate scale group: {key}')
        if env['revision']!=plan['head']:raise ValueError(f'stale scale head: {path}')
        cases=scenario_plan(env['backend'],env['source'],env['profile'],expected[key],'both')
        if env['scenario_plan']!=cases:raise ValueError(f'wrong declared scenario plan: {key}')
        runs=[r for r in data if r['type']=='runner'];transports=[r for r in data if r['type']=='transport']
        wanted_keys=[case_key(c) for c in cases]
        if [case_key(r) for r in runs]!=wanted_keys or [case_key(r) for r in transports]!=wanted_keys:raise ValueError(f'missing/repeated actual scale cases: {key}')
        fixture_rows=[r for r in data if r['type']=='fixture'];fixtures={r['count']:r for r in fixture_rows}
        sizes={r['files'] for r in cases}
        if len(fixture_rows)!=len(sizes) or set(fixtures)!=sizes:raise ValueError(f'missing fixtures: {key}')
        allocations=[r for r in data if r['type']=='allocation-sample']
        if len(allocations)!=len(sizes) or {r['files'] for r in allocations}!=sizes or any(r['logical_bytes']!=8 or r['allocated_bytes']<0 or r['filesystem_fragment_bytes']<=0 for r in allocations):raise ValueError('missing/invalid filesystem allocation observations')
        phases=[r for r in data if r['type']=='phase']
        expected_phases={(stage,count) for count in sizes for stage in ('fixture','clone','clone-cleanup')}|{('fixture-cleanup',None)}
        if {(r['phase'],r.get('files')) for r in phases}!=expected_phases or len(phases)!=len(expected_phases) or any(r['outcome']!='passed' for r in phases):raise ValueError(f'incomplete phase evidence: {key}')
        baselines=[r for r in data if r['type']=='direct-baseline']
        if [(r['files'],r['workload']) for r in baselines]!=list(dict.fromkeys((r['files'],r['workload']) for r in cases)):raise ValueError(f'incomplete direct baselines: {key}')
        for r in baselines:
            if r['backend']!='disk' or r['target_backend']!=env['backend']:raise ValueError('baseline must execute directly from disk')
            if r['child_observation']['generated_write']!=(env['backend']=='passthrough' and r['workload']=='full'):raise ValueError('baseline/runner child commands differ')
        for r,t in zip(runs,transports):
            info=fixtures[r['files']];m=r['metrics'];child=r['child_observation']
            if math.prod(info['fanouts'])!=r['files'] or info['logical_payload_bytes']!=r['files']*8+5:raise ValueError('wrong logical fixture size')
            if r['elapsed_ns']!=r['child_start_ns']+r['child_duration_ns']+r['after_child_ns']:raise ValueError('incoherent phase timing')
            if (r['backend'],r['source'],r['profile'])!=(env['backend'],env['source'],env['profile']):raise ValueError('wrong scenario identity')
            if type(r['verify']) is not bool:raise ValueError('ambiguous verification mode')
            wants=expected_demands(info,env['backend'],r['workload']) if env['source']=='remote' else set()
            if set(t['objects'])!=wants or m['remote_fetches']!=len(wants):raise ValueError('wrong demanded/fetched object set')
            validate_cache_budgets(m)
            if r['workload']=='full':
                if child['visited_leaves']!=r['files'] or child['payload_reads']!=r['files']+101:raise ValueError('incomplete full read')
                total[f'full_reads_{r["files"]}']+=1
            elif r['workload']=='enumerate':
                if child['visited_leaves']!=r['files'] or child['payload_reads']!=info['namespace_payload_reads']:raise ValueError('incomplete namespace/sampled-read control')
                total[f'namespace_{r["files"]}']+=1
            elif r['workload']=='noop':
                if child['payload_reads']!=0:raise ValueError('wrong no-op child reads')
                if env['backend']!='materialized' and (m['blob_loads']!=0 or m['tree_reads']>1):raise ValueError('non-lazy no-op')
            elif r['workload']=='sparse':
                if child['payload_reads']!=100:raise ValueError('wrong repeated sparse child reads')
                if env['backend']!='materialized' and (m['blob_loads']!=1 or m['tree_reads']>info['depth']):raise ValueError('non-lazy repeated sparse access')
            if env['backend']=='passthrough' and r['workload']=='full' and (m['passthrough_handles']<1 or not child['generated_write']):raise ValueError('actual passthrough path not exercised')
        observed[key]=len(runs);total['runner_invocations']+=len(runs)
    if set(observed)!=set(expected):raise ValueError(f'missing scale groups: {set(expected)-set(observed)}')
    return dict(groups=len(observed),**total)


def validate_resources(root):
    paths=list(root.rglob('pack-scaling.jsonl'))
    if len(paths)!=1:raise ValueError('missing/duplicate pack-scaling evidence')
    data=rows(paths[0])
    if [r['distinct_blobs'] for r in data]!=[32,128,512]:raise ValueError('missing distinct-demand sizes')
    for r in data:
        n=r['distinct_blobs']
        if r['baseline'] or r['passes']!=2 or r['metrics']['remote_fetches']!=n+2:raise ValueError('wrong pack demand control')
        if not 0<r['pack_directory_bytes_enumerated']<=2048*(n+2) or not 0<r['pack_metadata_syscalls']<=64*(n+2):raise ValueError('pack metadata work exceeds linear gate')
    return dict(distinct_demand_sizes=[32,128,512])


def validate_stress(root):
    traces=list(root.rglob('seed-*.jsonl'))
    if len(traces)!=2:raise ValueError('missing/duplicate stress seed traces')
    observed=set()
    for path in traces:
        data=rows(path);header=data[0];summary=data[-1]
        seed=header['seed']
        if header['type']!='header' or seed in observed or seed not in (0x5eed,0xc0ffee):raise ValueError('wrong stress seed')
        if (header['operations'],header['max_nodes'],header['max_bytes'])!=(400,64,65536):raise ValueError('stress bounds/operation coverage changed')
        operations=[r for r in data if r['type']=='operation'];results=[r for r in data if r['type']=='result']
        if [r['index'] for r in operations]!=list(range(400)) or [r['index'] for r in results]!=list(range(400)):raise ValueError('missing stress operations/results')
        if summary.get('type')!='summary' or summary['operations']!=400 or sum(summary['counts'].values())!=400:raise ValueError('stress did not complete')
        if any(r['actual']!=r['expected'] for r in results):raise ValueError('stress oracle mismatch')
        observed.add(seed)
    return dict(seeds=sorted(observed),operations_per_seed=400,write_policy='ignored')


def validate_lazy(plan,root):
    from benchmark import assert_lazy_reads
    files=list(root.rglob('results.jsonl'));paths={p.parent.name:p for p in files}
    if len(files)!=3 or set(paths)!={'benchmark-results','benchmark-small','benchmark-secondary'}:raise ValueError('missing lazy benchmark groups')
    counts={}
    for name,path in paths.items():
        data=rows(path);metadata=[r for r in data if r.get('event')=='metadata'];samples=[r for r in data if r.get('event')=='sample']
        if len(metadata)!=1 or metadata[0]['hardware_and_source']['source_revision']!=plan['head']:raise ValueError('missing/stale benchmark identity')
        if any(not r.get('success') or r['warmup'] for r in samples):raise ValueError('failed/ambiguous benchmark sample')
        expected={'digits-1000000':3} if name=='benchmark-results' else {'digits-1000':3} if name=='benchmark-small' else {'scale-0':3,'scale-1000':3,'scale-1000000':3,'repeat-1000':3,'large-1048576':3,'large-16777216':3,'large-67108864':3}
        if Counter(r['case']['case'] for r in samples)!=expected:raise ValueError('missing lazy sizes/repetitions')
        for r in samples:assert_lazy_reads(r['case'],r['snapshot_metrics'])
        counts[name]=len(samples)
    remote=list(root.rglob('million-remote.json'))
    if len(remote)!=1:raise ValueError('missing remote million-file real-FUSE evidence')
    value=json.loads(remote[0].read_text())
    if value['files']!=1000000 or value['head']!=plan['head'] or len(value['objects'])!=8:raise ValueError('wrong remote distinct-file evidence')
    runs=value['fuse_runs']
    commands=[['true'],['cat','/'.join(value['selected'])],['cat','/'.join(value['selected'])]]
    if len(runs)!=3 or [r['command'] for r in runs]!=commands or any(r['returncode']!=0 for r in runs):raise ValueError('missing real-FUSE million commands')
    if any(r['stdout']!=value['selected']+'\n' for r in runs[1:]) or runs[0]['stdout']!='':raise ValueError('wrong remote million stdout')
    if any(set(r['objects'])!=set(value['objects']) for r in runs[1:]) or len(runs[0]['objects'])!=2 or not set(runs[0]['objects'])<=set(value['objects']):raise ValueError('wrong lazy/no-global-cache network demand')
    return dict(benchmark_samples=counts,remote_real_fuse_commands=3)


def validate_evidence(plan,root):
    result={}
    if plan['required']['scale']:
        from real_repository import validate_reports
        result['scale']=validate_scale(plan,root)
        paths=list(root.rglob('webkit-results.jsonl'))
        if len(paths)!=1:raise ValueError('missing/duplicate WebKit scale evidence')
        result['webkit']=validate_reports(rows(paths[0]),plan['head'])
    if plan['required']['resources']:
        result['resources']=validate_resources(root)
        result['imports']=validate_object_reuse(plan,root)
        result['caches']=validate_cache_reports(plan,root)
    if plan['required']['stress']:result['stress']=validate_stress(root)
    if plan['required']['lazy']:result['lazy']=validate_lazy(plan,root)
    return result


def validate_cache_reports(plan,root):
    from metadata_reuse import validate_runner
    def report(name):
        paths=list(root.rglob(name))
        if len(paths)!=1:raise ValueError(f'missing/duplicate cache evidence: {name}')
        data=rows(paths[0])
        if not data or any(r.get('head')!=plan['head'] or r.get('working_tree_dirty') is not False for r in data):
            raise ValueError(f'missing/stale/dirty cache identity: {name}')
        return data
    try:
        data=report('source-cache.jsonl');by_mode={r['mode']:r for r in data}
        expected={'uncached-source':1000,'cached-source':1000,'kernel-cache-invariants':1000,
                  'uncached-readonly-directory':640,'cached-readonly-directory':640,'directory-cache-invariants':640}
        if len(data)!=len(expected) or set(by_mode)!=set(expected):raise ValueError('incomplete source cache modes')
        for mode,count in expected.items():
            row=by_mode[mode]
            if row['files']!=count:raise ValueError('wrong source cache workload')
            if mode.endswith('invariants'):
                fields=('unchanged_payload_and_namespace',) if mode=='kernel-cache-invariants' else ('unchanged_names_on_reopen','unrequested_data_unavailable')
                if row.get('result')!='passed' or any(row.get(k) is not True for k in fields):raise ValueError('failed cache invariants')
            else:
                phases=[r for r in row['child_observations'] if r['completed'] in ('first','repeat')]
                if [r['completed'] for r in phases]!=['first','repeat']:raise ValueError('incomplete source cache command')
                if mode in ('uncached-source','cached-source'):
                    full=[r for r in row['child_observations'] if r['completed']=='full']
                    if len(full)!=2 or any(r['count']!=count or r['visited_leaves']!=count or r['payload_reads']!=count+101 for r in full):
                        raise ValueError('incomplete source cache traversal')
                if 'directory' in mode:
                    if any(r['directory_entries']!=count for r in phases):raise ValueError('incomplete directory cache command')
                    m=row['metrics']
                    if m['blob_loads']!=0 or m['header_reads']!=0 or m['tree_reads']!=3:raise ValueError('nonlazy directory cache')
        for off,on,ops in (('uncached-source','cached-source',('lookup','read')),
                           ('uncached-readonly-directory','cached-readonly-directory',('readdir',))):
            for op in ops:
                if not 0<by_mode[on]['fuse_requests'][op]<by_mode[off]['fuse_requests'][op]:raise ValueError('missing kernel cache effect')
        for source,sizes in (('local',[1000,100000]),('remote',[100000])):
            data=report(f'metadata-reuse-{source}.jsonl')
            if [r['files'] for r in data]!=sizes:raise ValueError('incomplete metadata reuse sizes')
            for row in data:
                if (row.get('type')!='runner' or row.get('backend')!='fuse' or row.get('source')!=source
                    or row.get('operations')!=1000 or type(row.get('stat_abi_bytes')) is not int or row['stat_abi_bytes']<=0):
                    raise ValueError('wrong metadata reuse workload')
                validate_runner(row,row['files'],1000,row['stat_abi_bytes'],'fuse')
    except (AssertionError,KeyError,TypeError) as error:
        raise ValueError('invalid cache evidence') from error
    return dict(source_cache_modes=len(expected),metadata_groups=3)


def validate_object_reuse(plan,root):
    from object_reuse import validate_runner
    result={}
    for backend,count in (('fuse',160),('materialized',32)):
        name=f'object-reuse-{backend}.jsonl';paths=list(root.rglob(name))
        if len(paths)!=1:raise ValueError(f'missing/duplicate object reuse evidence: {name}')
        data=rows(paths[0])
        if len(data)!=1:raise ValueError('missing/duplicate object reuse invocation')
        row=data[0]
        if (row.get('head')!=plan['head'] or row.get('working_tree_dirty') is not False
            or row.get('type')!='runner' or row.get('backend')!=backend
            or row.get('files')!=count or row.get('blob_bytes')!=8192
            or type(row.get('stat_abi_bytes')) is not int or row['stat_abi_bytes']<=0):
            raise ValueError('wrong object reuse identity')
        try:validate_runner(row,count,8192,row['stat_abi_bytes'],backend)
        except (AssertionError,KeyError,TypeError) as error:raise ValueError('incomplete object reuse execution') from error
        result[backend]=dict(distinct_blobs=count,invocations=1,application_passes=2)
    return result
