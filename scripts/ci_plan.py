#!/usr/bin/env python3
"""Conservative pre-merge CI selection and aggregate job-status validation.

Workflows always run. Optional jobs may skip only after a successful plan; the
required aggregate independently checks the plan, outcomes and coverage files.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT=Path(__file__).resolve().parents[1]
KINDS=('scale','stress','lazy','resources')
CONTRACT_DOCS={'AGENTS.md','docs/specification.md','docs/interview.md','docs/fuse-coverage.md','docs/platforms.md','docs/remote.md','docs/releases.md'}
SCOPED={
    'tests/scalability.py':('scale',),
    'tests/test_scalability_harness.py':('scale',),
    'tests/stress.py':('stress',),
    'tests/benchmark.py':('lazy',),
    'tests/benchmark.md':('lazy',),
    'tests/pack_scaling.py':('resources',),
}


def matrix(profile):
    cases=[dict(runner='ubuntu-latest',backend='fuse',sources=source,policies='none,ignored,untracked,all',profile=profile) for source in ('local','remote')]
    cases += [dict(runner=host,backend='materialized',sources='local,remote',policies='all',profile=profile) for host in ('ubuntu-latest','macos-26')]
    return {'include':cases}


def select(paths,head,full=False,force_all=False,reason='changed paths'):
    required=dict.fromkeys(KINDS,False);reasons=[]
    for path in paths:
        if path in SCOPED:
            kinds=SCOPED[path];reasons.append(dict(path=path,kinds=list(kinds),reason='scoped test driver'))
        elif path not in CONTRACT_DOCS and (path in ('README.md','RELEASES.md','tests/README.md','demo.webp','demo.gif') or (path.startswith('docs/') and path.endswith('.md'))):
            kinds=();reasons.append(dict(path=path,kinds=[],reason='prose/media; mandatory base checks remain'))
        else:
            # Runtime, shared helpers, dependency/build/workflow changes, contract
            # documents, additions/deletions and all unknown paths fail closed.
            kinds=KINDS;reasons.append(dict(path=path,kinds=list(kinds),reason='shared, behavioral or unknown input'))
        for kind in kinds:required[kind]=True
    if force_all or full:required=dict.fromkeys(KINDS,True)
    profile='exhaustive' if full else 'matrix'
    return dict(version=1,head=head,paths=list(paths),forced=bool(force_all),full=bool(full),reason=reason,
        required=required,profile=profile,scale_matrix=matrix(profile),decisions=reasons)


def validate_plan(plan,head):
    if plan.get('version')!=1 or plan.get('head')!=head:raise ValueError('missing/stale CI plan')
    if not re.fullmatch(r'[0-9a-f]{40,64}',head):raise ValueError('invalid head identity')
    if type(plan.get('forced')) is not bool or type(plan.get('full')) is not bool:raise ValueError('plan mode must be boolean')
    if not isinstance(plan.get('paths'),list) or not all(isinstance(p,str) for p in plan['paths']):raise ValueError('invalid changed paths')
    expected=select(plan['paths'],head,plan['full'],plan['forced'],plan.get('reason',''))
    for key in ('required','profile','scale_matrix'):
        if plan.get(key)!=expected[key]:raise ValueError(f'inconsistent CI plan: {key}')
    if set(plan['required'])!=set(KINDS) or any(type(v) is not bool for v in plan['required'].values()):raise ValueError('requirements must be explicit booleans')


def validate_jobs(plan,needs):
    for job in ('changes','rust','real-fuse','release-policy','macos','package-build'):
        if needs.get(job,{}).get('result')!='success':raise ValueError(f'mandatory base job did not pass: {job}')
    for kind,job in (('scale','repository-scale'),('scale','webkit-scale'),('lazy','lazy-performance')):
        result=needs.get(job,{}).get('result')
        allowed=('success',) if plan['required'][kind] else ('success','skipped')
        if result not in allowed:raise ValueError(f'required {kind} coverage missing/failed: {result}')
    checks=(('resources',needs['rust'].get('outputs',{}).get('resources_step')),
            ('stress',needs['real-fuse'].get('outputs',{}).get('stress_step')))
    for kind,outcome in checks:
        if outcome not in (('success',) if plan['required'][kind] else ('success','skipped')):
            raise ValueError(f'required {kind} step missing/failed: {outcome}')
    outcome=needs['real-fuse'].get('outputs',{}).get('passthrough_scale')
    if outcome not in (('passed',) if plan['required']['scale'] else ('passed','not_required')):
        raise ValueError(f'passthrough scale coverage missing/failed: {outcome}')


def changed_paths(base,head):
    if not base or not re.fullmatch(r'[0-9a-f]{40,64}',base) or set(base)=={'0'}:raise ValueError('no usable comparison base')
    # No renames: both the old and new path participate in conservative rules.
    output=subprocess.check_output(['git','diff','--no-renames','--name-only','-z',f'{base}...{head}','--'],stderr=subprocess.PIPE)
    return [os.fsdecode(value) for value in output.split(b'\0') if value]


def build(event,event_name,head):
    if event_name=='workflow_dispatch':
        value=event.get('inputs',{}).get('full_matrix',True)
        if type(value) is not bool and (not isinstance(value,str) or value not in ('true','false')):raise ValueError('full_matrix must be boolean')
        return select([],head,full=value in (True,'true'),force_all=True,reason='explicit manual validation')
    if event_name=='pull_request':base=event.get('pull_request',{}).get('base',{}).get('sha')
    elif event_name=='push':base=event.get('before')
    else:return select([],head,force_all=True,reason='unknown event; conservative full affected coverage')
    try:paths=changed_paths(base,head)
    except (ValueError,subprocess.CalledProcessError):
        return select([],head,force_all=True,reason='comparison unavailable; conservative full affected coverage')
    return select(paths,head)


def main():
    parser=argparse.ArgumentParser(description=__doc__);commands=parser.add_subparsers(dest='command',required=True)
    create=commands.add_parser('build');create.add_argument('--event',type=Path,required=True);create.add_argument('--event-name',required=True);create.add_argument('--report',type=Path,required=True);create.add_argument('--github-output',type=Path)
    check=commands.add_parser('check');check.add_argument('--plan',type=Path,required=True);check.add_argument('--head',required=True);check.add_argument('--evidence',type=Path,required=True)
    args=parser.parse_args()
    if args.command=='build':
        head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
        plan=build(json.loads(args.event.read_text()),args.event_name,head);validate_plan(plan,head)
        args.report.write_text(json.dumps(plan,indent=2)+'\n')
        if args.github_output:
            with args.github_output.open('a') as output:
                for key,value in plan['required'].items():output.write(f'{key}={str(value).lower()}\n')
                output.write(f'profile={plan["profile"]}\nscale_matrix={json.dumps(plan["scale_matrix"],separators=(",",":"))}\n')
        print(json.dumps({key:plan[key] for key in ('head','required','profile','reason','decisions')},ensure_ascii=True))
    else:
        plan=json.loads(args.plan.read_text());validate_plan(plan,args.head)
        validate_jobs(plan,json.loads(os.environ['NEEDS_JSON']))
        sys.path.insert(0,str(ROOT/'tests'))
        from ci_evidence import validate_evidence
        summary=validate_evidence(plan,args.evidence)
        print(json.dumps(dict(head=args.head,required=plan['required'],validated=summary),sort_keys=True))


if __name__=='__main__':main()
