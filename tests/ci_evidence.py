"""Validate completed coverage, not just green jobs, for the required CI gate."""
from collections import Counter
import json
import math
from pathlib import Path
from scalability import expected_demands, scenario_plan


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
            for kind in ('blob','tree'):
                if m[f'{kind}_cache_bytes']>m[f'{kind}_cache_limit']:raise ValueError('memory cache exceeds budget')
            if r['workload']=='full':
                if child['visited_leaves']!=r['files'] or child['payload_reads']!=r['files']+101:raise ValueError('incomplete full read')
                total[f'full_reads_{r["files"]}']+=1
            elif r['workload']=='enumerate':
                if child['visited_leaves']!=r['files'] or child['payload_reads']!=info['namespace_payload_reads']:raise ValueError('incomplete namespace/sampled-read control')
                total[f'namespace_{r["files"]}']+=1
            elif r['workload']=='noop':
                if child['payload_reads']!=0 or m['blob_loads']!=0 or m['tree_reads']>1:raise ValueError('non-lazy no-op')
            elif r['workload']=='sparse':
                if child['payload_reads']!=100 or m['blob_loads']!=1 or m['tree_reads']>info['depth']:raise ValueError('non-lazy repeated sparse access')
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
    if plan['required']['scale']:result['scale']=validate_scale(plan,root)
    if plan['required']['resources']:result['resources']=validate_resources(root)
    if plan['required']['stress']:result['stress']=validate_stress(root)
    if plan['required']['lazy']:result['lazy']=validate_lazy(plan,root)
    return result
