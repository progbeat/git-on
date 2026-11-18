#!/usr/bin/env python3
"""Helpers used by the real-mount write-policy tests."""
import ast
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from filesystem_quota import STORAGE_OBSERVATION


class PolicyHarnessTests(unittest.TestCase):
    def test_policy_launcher_has_no_removed_keep_reference(self):
        tree=ast.parse(Path(__file__).with_name('write_policy.py').read_text())
        method=next(fn for cls in tree.body if isinstance(cls,ast.ClassDef) for fn in cls.body if isinstance(fn,ast.FunctionDef) and fn.name=='policy')
        class Probe:
            def invoke_argv(self,args):return args
        namespace={'sys':type('Sys',(),{'executable':'python'})}
        exec(compile(ast.Module(body=[method],type_ignores=[]),'<policy regression>','exec'),namespace)
        result=namespace['policy'](Probe(),'all','print(1)')
        self.assertEqual(result,['HEAD','--write-policy=all','--','python','-c','print(1)'])

    def test_storage_accounting_restarts_when_a_listed_path_moves(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            (root/'.git').mkdir()
            (root/'payload').write_bytes(b'x'*8192)
            code=STORAGE_OBSERVATION+r'''
expected=allocated()
walk=os.walk
moved=False
def relocating_walk(*args,**kwargs):
 global moved
 for row in walk(*args,**kwargs):
  if not moved:
   (root/'payload').rename(root/'moved')
   moved=True
  yield row
os.walk=relocating_walk
assert allocated()==expected,'a moved payload must still count toward the budget'
'''
            result=subprocess.run([sys.executable,'-I','-S','-c',code],cwd=root,
                                  capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,0,result.stderr)


if __name__=='__main__':unittest.main(verbosity=2)
