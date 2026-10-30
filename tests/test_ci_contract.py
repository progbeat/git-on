#!/usr/bin/env python3
"""Native package and macOS behavior regressions for the test workflow."""
import os
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts'))
from ci_plan import matrix
WORKFLOW=(subprocess.check_output(['git','show',os.environ['GIT_ON_CI_BEFORE_REF']+':.github/workflows/ci.yml'],cwd=ROOT,text=True) if 'GIT_ON_CI_BEFORE_REF' in os.environ else (ROOT/'.github/workflows/ci.yml').read_text())


def job(name):return re.search(r'^  '+name+r':\n(.*?)(?=^  [\w-]+:\n|\Z)',WORKFLOW,re.M|re.S)[1]


class CiContractTests(unittest.TestCase):
    def test_python_regression_suites_are_registered(self):
        # These standalone suites use explicit workflow commands, not discovery.
        # Keep newly added negative harness tests from silently staying local.
        for suite in sorted((ROOT/'tests').glob('test_*.py')):
            with self.subTest(suite=suite.name):
                self.assertIn('tests/'+suite.name,WORKFLOW)

    def test_package_requires_real_linux_and_macos_builds(self):
        body=job('package-build')
        for expected in ['x86_64-unknown-linux-gnu','aarch64-apple-darwin',
                         'dist build --artifacts=all','tar -xzf','tests/dist_installer.py','tests/integration.py','tests/materialized.py']:
            self.assertIn(expected,body)
        dependencies=re.search(r'needs: \[(.*?)\]',job('package'))[1].split(', ')
        self.assertEqual(set(dependencies),{'changes','rust','real-fuse','lazy-performance','repository-scale','release-policy','macos','package-build'})
        self.assertIn('if: always()',job('package'))
        self.assertIn('python3 scripts/ci_plan.py check',job('package'))
        self.assertIn('NEEDS_JSON: ${{ toJSON(needs) }}',job('package'))
        self.assertIn('name: ci-plan',job('package'))

    def test_native_package_probe_uses_current_cargo_version(self):
        body=job('package-build')
        self.assertIn('VERSION=$(cargo metadata',body)
        self.assertIn('= "git-on $VERSION"',body)
        self.assertNotRegex(body,r"= ['\"]git-on [0-9]+\.[0-9]+\.[0-9]+['\"]")

    def test_macos_runs_behavior_and_installer_tests(self):
        body=job('macos')
        for check in ['cargo test --locked','tests/integration.py','tests/materialized.py','tests/test_installer.py']:
            self.assertIn(check,body)

    def test_release_trigger_is_only_after_validated_master_checks(self):
        body=job('release')
        self.assertIn("github.event_name == 'push' && github.ref == 'refs/heads/master'",body)
        self.assertIn("needs.release-policy.outputs.release_required == 'true'",body)
        self.assertIn('needs: [rust, macos, real-fuse, lazy-performance, repository-scale, release-policy, package]',body)
        self.assertIn('actions: write',body)

    def test_repository_scale_requires_complete_real_backend_matrix(self):
        body=job('repository-scale')
        self.assertIn("if: needs.changes.outputs.scale == 'true'",body)
        self.assertIn('matrix: ${{ fromJSON(needs.changes.outputs.scale_matrix) }}',body)
        cells=matrix('matrix')['include']
        self.assertEqual(len(cells),5)
        interactions={(g['runner'],g['backend'],source,policy,verify) for g in cells if g['profile']=='matrix' for source in g['sources'].split(',') for policy in g['policies'].split(',') for verify in (False,True)}
        self.assertEqual(len(interactions),24)
        for expected in ['tests/scalability.py','--features test-metrics','--profile "$PROFILE"','scale-results-$SOURCE.jsonl','matrix.profile','matrix.sources']:
            self.assertIn(expected,body)
        self.assertIn('workflow_dispatch:',WORKFLOW)
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}",WORKFLOW)
        self.assertIn('|| github.run_id',WORKFLOW)
        self.assertIn('full_matrix:',WORKFLOW)
        self.assertNotIn('paths:',WORKFLOW[:WORKFLOW.index('jobs:')],'never path-filter required workflows')
        self.assertIn("if: needs.changes.outputs.lazy == 'true'",job('lazy-performance'))
        self.assertIn("if: needs.changes.outputs.resources == 'true'",job('rust'))
        self.assertIn("if: needs.changes.outputs.stress == 'true'",job('real-fuse'))
        self.assertIn('scale_coverage=passed',job('real-fuse'))
        self.assertIn('tests/scalability.py',job('real-fuse'))
        self.assertIn('--backend passthrough',job('real-fuse'))
        self.assertIn('tests/pack_scaling.py',job('rust'))
        self.assertIn('--million',job('lazy-performance'))
        self.assertIn('1000000',job('lazy-performance'))
        self.assertNotIn('--sizes',body,'profile defaults define and guard each scale control')
        self.assertNotIn('github.event_name',body,'scale correctness stays before merge')
        self.assertIn('- run: cargo test --locked --all-targets\n',job('rust'))
        self.assertEqual(job('rust').count('--skip'),1)
        self.assertIn('--features test-metrics -- --skip policy::tests::adversarial_patterns_match_git_byte_for_byte',job('rust'))

    def test_missing_intermediate_baseline_keeps_current_pack_scaling_gate(self):
        body=job('rust')
        script=re.search(r'      - name: Measure distinct-object pack work.*?        run: ([^\n]+)',body,re.S)[1]
        self.assertNotIn('git worktree',body,'historical red binaries are opt-in, not recurring builds')
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);tools=root/'tools';tools.mkdir();calls=root/'calls'
            subprocess.run(['git','init','--quiet',str(root/'repo')],check=True)
            (tools/'python3').write_text('#!'+sys.executable+'\nimport json,os,sys\nopen(os.environ["CALLS"],"w").write(json.dumps(sys.argv[1:]))\n')
            (tools/'python3').chmod(0o755)
            result=subprocess.run(['bash','-euo','pipefail','-c',script],cwd=root/'repo',capture_output=True,text=True,
                env={**os.environ,'PATH':str(tools)+os.pathsep+os.environ['PATH'],'CALLS':str(calls)})
            self.assertEqual(result.returncode,0,result.stderr)
            arguments=json.loads(calls.read_text())
            self.assertEqual(arguments[:3],['tests/pack_scaling.py','--probe','target/debug/examples/source-probe'])
            self.assertNotIn('--before-probe',arguments)

    def test_trigger_pins_the_tested_commit_and_never_moves_existing_tags(self):
        script=textwrap.dedent(re.search(r'        run: \|\n(.*)',job('release'),re.S)[1])
        sha='a'*40
        for existing,create_status in [('',0),(sha,0),('b'*40,0),('',1)]:
            with self.subTest(existing=existing,create_status=create_status),tempfile.TemporaryDirectory() as directory:
                root=Path(directory); fake=root/'gh'; calls=root/'calls.jsonl'
                fake.write_text('''#!/usr/bin/env python3
import json,os,sys
with open(os.environ['CALLS'],'a') as f:f.write(json.dumps(sys.argv[1:])+'\\n')
if '/git/ref/' in sys.argv[2]:
 value=os.environ['EXISTING']
 if not value:sys.exit(1)
 print(value)
elif '/git/refs' in sys.argv[2]:sys.exit(int(os.environ['CREATE_STATUS']))
''')
                fake.chmod(0o755)
                result=subprocess.run(['bash','-e','-o','pipefail','-c',script],capture_output=True,text=True,
                    env={**os.environ,'PATH':str(root)+os.pathsep+os.environ['PATH'],
                         'GITHUB_REPOSITORY':'owner/repo','GITHUB_SHA':sha,'VERSION':'1.3.0',
                         'CALLS':str(calls),'EXISTING':existing,'CREATE_STATUS':str(create_status)})
                recorded=[json.loads(line) for line in calls.read_text().splitlines()]
                dispatches=[call for call in recorded if call[:2]==['workflow','run']]
                success=existing in ('',sha) and create_status==0
                self.assertEqual(result.returncode==0,success,result.stderr)
                self.assertEqual(len(dispatches),int(success))
                if success:
                    self.assertEqual(dispatches[0],['workflow','run','release.yml','--repo','owner/repo','--ref','refs/tags/v1.3.0','-f','tag=v1.3.0'])
                creates=[call for call in recorded if '--method' in call]
                self.assertEqual(bool(creates),not existing)
                if creates:
                    self.assertIn('sha='+sha,creates[0])
                    self.assertIn('ref=refs/tags/v1.3.0',creates[0])


if __name__=='__main__':unittest.main(verbosity=2)
