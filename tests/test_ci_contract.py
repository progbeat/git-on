#!/usr/bin/env python3
"""Publication ordering and future-version regressions for the real workflow."""
import os
from pathlib import Path
import re
import subprocess
import unittest

ROOT=Path(__file__).resolve().parents[1]
WORKFLOW=(subprocess.check_output(['git','show',os.environ['GIT_ON_CI_BEFORE_REF']+':.github/workflows/ci.yml'],cwd=ROOT,text=True) if 'GIT_ON_CI_BEFORE_REF' in os.environ else (ROOT/'.github/workflows/ci.yml').read_text())


def job(name):return re.search(r'^  '+name+r':\n(.*?)(?=^  [\w-]+:\n|\Z)',WORKFLOW,re.M|re.S)[1]


class CiContractTests(unittest.TestCase):
    def test_native_integrity_precedes_global_generation_and_rehash(self):
        body=job('package-assemble')
        self.assertIn('python3 scripts/verify_local_dist.py',body)
        self.assertLess(body.index('python3 scripts/verify_local_dist.py'),body.index('build --artifacts=global'))
        self.assertLess(body.index('build --artifacts=global'),body.index('harden_dist_installer.py'))

    def test_native_package_probe_uses_current_cargo_version(self):
        body=job('package-build')
        self.assertIn('VERSION=$(cargo metadata',body)
        self.assertIn('= "git-on $VERSION"',body)
        self.assertNotRegex(body,r"= ['\"]git-on [0-9]+\.[0-9]+\.[0-9]+['\"]")

    def test_macos_runs_behavior_and_installer_tests(self):
        body=job('macos')
        for check in ['cargo test --locked','tests/integration.py','tests/materialized.py','tests/test_installer.py']:
            self.assertIn(check,body)


if __name__=='__main__':unittest.main(verbosity=2)
