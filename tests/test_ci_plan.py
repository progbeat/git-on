#!/usr/bin/env python3
"""Conservative change selection and fail-closed pre-merge coverage gates."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts'))
from ci_plan import KINDS, build, changed_paths, matrix, select, validate_jobs, validate_plan
from ci_evidence import validate_evidence, validate_lazy, validate_resources, validate_scale, validate_stress
HEAD='a'*40


def outcomes(plan):
    required=plan['required']
    result={key:dict(result='success',outputs={}) for key in ('changes','rust','real-fuse','release-policy','macos','package-build')}
    result['repository-scale']=dict(result='success' if required['scale'] else 'skipped')
    result['lazy-performance']=dict(result='success' if required['lazy'] else 'skipped')
    result['rust']['outputs']['resources_step']='success' if required['resources'] else 'skipped'
    result['real-fuse']['outputs'].update(stress_step='success' if required['stress'] else 'skipped',passthrough_scale='passed' if required['scale'] else 'not_required')
    return result


class CiPlanTests(unittest.TestCase):
    def test_docs_keep_fast_base_and_unknown_shared_dependency_workflow_inputs_require_all(self):
        documentation=select(['README.md','docs/ci-performance.md','docs/new-guide.md'],HEAD)
        self.assertEqual(documentation['required'],dict.fromkeys(KINDS,False))
        for path in ('src/cli.rs','src/backend/linux.rs','Cargo.lock','Cargo.toml','.cargo/config.toml',
                     '.github/workflows/ci.yml','tests/remote.py','tests/support/source_probe.rs','tests/test_ci_contract.py',
                     'scripts/ci_plan.py','AGENTS.md','docs/specification.md','vendor/unknown/source.c','new.config','unknown\nfile'):
            with self.subTest(path=path):self.assertEqual(select([path],HEAD)['required'],dict.fromkeys(KINDS,True))

    def test_specific_drivers_select_their_distinct_coverage_and_union_changes(self):
        for path,kind in (('tests/stress.py','stress'),('tests/scalability.py','scale'),('tests/test_scalability_harness.py','scale'),('tests/benchmark.py','lazy'),('tests/pack_scaling.py','resources')):
            self.assertEqual(select([path],HEAD)['required'],{k:k==kind for k in KINDS})
        self.assertEqual(select(['tests/stress.py','tests/benchmark.py'],HEAD)['required'],dict(scale=False,stress=True,lazy=True,resources=False))

    def test_manual_full_and_unknown_comparisons_cannot_skip(self):
        for value in (True,'true'):
            plan=build(dict(inputs=dict(full_matrix=value)),'workflow_dispatch',HEAD)
            self.assertEqual(plan['required'],dict.fromkeys(KINDS,True))
            self.assertEqual(plan['profile'],'exhaustive')
            self.assertEqual(len(plan['scale_matrix']['include']),4)
        plan=build(dict(inputs=dict(full_matrix=False)),'workflow_dispatch',HEAD)
        self.assertEqual(plan['profile'],'matrix');self.assertTrue(all(plan['required'].values()))
        for event,name in (({},'pull_request'),({'before':'0'*40},'push'),({},'unexpected')):
            plan=build(event,name,HEAD);self.assertTrue(all(plan['required'].values()))
        with self.assertRaises(ValueError):build(dict(inputs=dict(full_matrix='maybe')),'workflow_dispatch',HEAD)

    def test_matrix_groups_keep_every_source_policy_signature_interaction(self):
        for profile in ('matrix','exhaustive'):
            groups=matrix(profile)['include']
            interactions={(g['runner'],g['backend'],source,policy,verify) for g in groups if g['profile']!='namespace' for source in g['sources'].split(',') for policy in g['policies'].split(',') for verify in (False,True)}
            required={('ubuntu-latest','fuse',s,p,v) for s in ('local','remote') for p in ('none','ignored','untracked','all') for v in (False,True)}
            required.update((h,'materialized',s,'all',v) for h in ('ubuntu-latest','macos-26') for s in ('local','remote') for v in (False,True))
            self.assertEqual(interactions,required)
        self.assertEqual(matrix('matrix')['include'][-1]['profile'],'namespace')
        from scalability import scenario_plan
        cases=[]
        for g in matrix('exhaustive')['include']:
            for source in g['sources'].split(','):cases+=scenario_plan(g['backend'],source,'exhaustive',g['policies'].split(','),'both')
        for source in ('local','remote'):cases+=scenario_plan('passthrough',source,'exhaustive',['ignored'],'both')
        self.assertEqual(len(cases),378)
        self.assertEqual(sum(c['files']==1000000 and c['workload']=='full' for c in cases),42)

    def test_plan_schema_rejects_stale_heads_missing_requirements_and_dropped_matrix_cells(self):
        plan=select(['src/main.rs'],HEAD);validate_plan(plan,HEAD)
        for change in ('stale','missing','false-string','cell','forced'):
            broken=copy.deepcopy(plan)
            if change=='stale':broken['head']='b'*40
            elif change=='missing':del broken['required']['scale']
            elif change=='false-string':broken['required']['scale']='true'
            elif change=='cell':broken['scale_matrix']['include'].pop()
            else:broken['forced']='false'
            with self.subTest(change=change),self.assertRaises(ValueError):validate_plan(broken,HEAD)

    def test_aggregate_allows_skips_only_when_not_required_and_requires_every_base_job(self):
        plan=select(['README.md'],HEAD);needs=outcomes(plan);validate_jobs(plan,needs)
        with tempfile.TemporaryDirectory() as directory:self.assertEqual(validate_evidence(plan,Path(directory)),{})
        for job in ('changes','rust','real-fuse','release-policy','macos','package-build'):
            broken=copy.deepcopy(needs);broken[job]['result']='skipped'
            with self.subTest(job=job),self.assertRaises(ValueError):validate_jobs(plan,broken)
        del needs['changes']
        with self.assertRaises(ValueError):validate_jobs(plan,needs)

    def test_required_job_or_step_skip_never_passes_even_when_other_jobs_are_green(self):
        plan=select(['Cargo.lock'],HEAD);needs=outcomes(plan);validate_jobs(plan,needs)
        for job in ('lazy-performance','repository-scale'):
            broken=copy.deepcopy(needs);broken[job]['result']='skipped'
            with self.subTest(job=job),self.assertRaises(ValueError):validate_jobs(plan,broken)
        for job,field,value in (('rust','resources_step','skipped'),('real-fuse','stress_step','skipped'),('real-fuse','passthrough_scale','not_required')):
            broken=copy.deepcopy(needs);broken[job]['outputs'][field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):validate_jobs(plan,broken)

    def test_successful_job_without_required_files_does_not_satisfy_coverage(self):
        plan=select(['src/main.rs'],HEAD);validate_jobs(plan,outcomes(plan))
        with tempfile.TemporaryDirectory() as directory:
            for validator in (lambda:validate_scale(plan,Path(directory)),lambda:validate_resources(Path(directory)),lambda:validate_stress(Path(directory))):
                with self.assertRaises(ValueError):validator()

    def test_pack_report_requires_all_sizes_and_nonzero_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);path=root/'pack-scaling.jsonl'
            data=[dict(distinct_blobs=n,passes=2,baseline=False,metrics=dict(remote_fetches=n+2),pack_metadata_syscalls=n,pack_directory_bytes_enumerated=n*64) for n in (32,128,512)]
            def write():path.write_text(''.join(json.dumps(r)+'\n' for r in data))
            write();validate_resources(root)
            data.pop();write()
            with self.assertRaises(ValueError):validate_resources(root)
            data[0]['pack_metadata_syscalls']=0;data.append(dict(data[0],distinct_blobs=512));write()
            with self.assertRaises(ValueError):validate_resources(root)

    def test_stress_header_without_completed_operations_is_not_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            for seed in (0x5eed,0xc0ffee):
                (Path(directory)/f'seed-{seed}.jsonl').write_text(json.dumps(dict(type='header',seed=seed,operations=400,max_nodes=64,max_bytes=65536))+'\n')
            with self.assertRaises(ValueError):validate_stress(Path(directory))

    def test_synthetic_complete_scale_reports_pass_and_missing_case_fails(self):
        # This tests the evidence parser only, never claims backend execution.
        from scalability import fixture_shape, scenario_plan, expected_demands
        import math
        plan=select(['tests/scalability.py'],HEAD)
        expected=[]
        for group in plan['scale_matrix']['include']:
            host='macos' if group['runner'].startswith('macos') else 'linux'
            for source in group['sources'].split(','):expected.append((host,group['backend'],source,group['profile'],group['policies'].split(',')))
        expected.extend(('linux','passthrough',source,'matrix',['ignored']) for source in ('local','remote'))
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for index,(host,backend,source,profile,policies) in enumerate(expected):
                cases=scenario_plan(backend,source,profile,policies,'both')
                data=[dict(type='environment',platform=host,backend=backend,source=source,profile=profile,revision=HEAD,scenario_plan=cases)]
                infos={}
                for count in dict.fromkeys(c['files'] for c in cases):
                    shape=fixture_shape(count)
                    info=dict(count=count,fanouts=shape,depth=len(shape),logical_payload_bytes=count*8+5,git_objects=['c','r','t','b','i'],commit='c',tree='r',sparse_objects=['c','r','t','b'],namespace_objects=['c','r','t','b','i'],namespace_payload_reads=math.prod(shape[:-1])+101)
                    infos[count]=info;data.append(dict(type='fixture',**info))
                    for phase in ('fixture','clone','clone-cleanup'):data.append(dict(type='phase',phase=phase,files=count,outcome='passed'))
                    data.append(dict(type='allocation-sample',files=count,logical_bytes=8,allocated_bytes=0,filesystem_fragment_bytes=512))
                for count,workload in dict.fromkeys((c['files'],c['workload']) for c in cases):
                    data.append(dict(type='direct-baseline',files=count,workload=workload,backend='disk',target_backend=backend,child_observation=dict(generated_write=backend=='passthrough' and workload=='full')))
                for case in cases:
                    info=infos[case['files']];wants=expected_demands(info,backend,case['workload']) if source=='remote' else set()
                    child=dict(generated_write=backend=='passthrough' and case['workload']=='full',visited_leaves=case['files'] if case['workload'] in ('full','enumerate') else None,payload_reads=case['files']+101 if case['workload']=='full' else info['namespace_payload_reads'] if case['workload']=='enumerate' else 100 if case['workload']=='sparse' else 0)
                    metrics=dict(remote_fetches=len(wants),tree_reads=0,blob_loads=int(case['workload']=='sparse'),blob_cache_bytes=0,blob_cache_limit=1,tree_cache_bytes=0,tree_cache_limit=1,passthrough_handles=int(backend=='passthrough'))
                    data.extend([dict(type='runner',backend=backend,source=source,profile=profile,**case,elapsed_ns=3,child_start_ns=1,child_duration_ns=1,after_child_ns=1,child_observation=child,metrics=metrics),dict(type='transport',**case,objects=sorted(wants))])
                data.append(dict(type='phase',phase='fixture-cleanup',outcome='passed'))
                (root/f'scale-{index}.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in data))
            result=validate_scale(plan,root)
            self.assertEqual(result['runner_invocations'],97)
            self.assertEqual(result['full_reads_100000'],8)
            self.assertEqual(result['namespace_1000000'],1)
            path=root/'scale-0.jsonl';data=[json.loads(l) for l in path.read_text().splitlines()]
            data.pop(next(i for i,r in enumerate(data) if r['type']=='runner'))
            path.write_text(''.join(json.dumps(r)+'\n' for r in data))
            with self.assertRaisesRegex(ValueError,'missing/repeated actual scale'):validate_scale(plan,root)

    def test_git_diff_includes_both_renamed_paths_and_handles_real_doc_only_diff(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'src').mkdir();(root/'src/x.rs').write_text('source')
            env={k:v for k,v in os.environ.items() if not k.startswith('GIT_')};env.update(GIT_CONFIG_GLOBAL=os.devnull,GIT_CONFIG_NOSYSTEM='1')
            def git(*args):return subprocess.check_output(['git',*args],cwd=root,env=env,text=True).strip()
            git('init','--quiet');git('config','user.name','CI fixture');git('config','user.email','ci@example.invalid');git('add','.');git('commit','--quiet','-m','base');base=git('rev-parse','HEAD')
            (root/'src/x.rs').rename(root/'README.md');git('add','-A');git('commit','--quiet','-m','rename');head=git('rev-parse','HEAD')
            old=Path.cwd()
            try:
                os.chdir(root);paths=changed_paths(base,head)
                self.assertEqual(set(paths),{'README.md','src/x.rs'})
                self.assertTrue(all(select(paths,head)['required'].values()))
                (root/'README.md').write_text('new prose');git('add','.');git('commit','--quiet','-m','docs');doc_head=git('rev-parse','HEAD')
                paths=changed_paths(head,doc_head)
                self.assertEqual(paths,['README.md'])
                self.assertFalse(any(select(paths,doc_head)['required'].values()))
                pr=build({'pull_request':{'base':{'sha':base}}},'pull_request',doc_head)
                self.assertTrue(all(pr['required'].values()),'a docs-only follow-up must not hide earlier runtime changes in the PR')
            finally:os.chdir(old)


if __name__=='__main__':unittest.main(verbosity=2)
