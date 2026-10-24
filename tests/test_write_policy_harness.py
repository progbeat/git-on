#!/usr/bin/env python3
"""The four-policy launcher must build argv before real mounts can test policy."""
import ast
from pathlib import Path
import unittest


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


if __name__=='__main__':unittest.main(verbosity=2)
