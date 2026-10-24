#!/usr/bin/env python3
"""Native package and macOS behavior regressions for the test workflow."""
import os
import json
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest

ROOT=Path(__file__).resolve().parents[1]
WORKFLOW=(subprocess.check_output(['git','show',os.environ['GIT_ON_CI_BEFORE_REF']+':.github/workflows/ci.yml'],cwd=ROOT,text=True) if 'GIT_ON_CI_BEFORE_REF' in os.environ else (ROOT/'.github/workflows/ci.yml').read_text())


def job(name):return re.search(r'^  '+name+r':\n(.*?)(?=^  [\w-]+:\n|\Z)',WORKFLOW,re.M|re.S)[1]


class CiContractTests(unittest.TestCase):
    def test_package_requires_real_linux_and_macos_builds(self):
        body=job('package-build')
        for expected in ['x86_64-unknown-linux-gnu','aarch64-apple-darwin',
                         'dist build --artifacts=all','tar -xzf','tests/dist_installer.py','tests/integration.py','tests/materialized.py']:
            self.assertIn(expected,body)
        self.assertIn('needs: [package-build]',job('package'))

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
        self.assertIn('needs: [rust, macos, real-fuse, lazy-performance, release-policy, package]',body)
        self.assertIn('actions: write',body)

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
