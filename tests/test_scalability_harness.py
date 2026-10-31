#!/usr/bin/env python3
"""Guard the real workload's root-only metadata allowance without weakening laziness."""
import unittest
import contextlib
import io
import json
import subprocess
import os
from pathlib import Path
import sys
import tempfile
from unittest import mock
from scalability import CHILD, expected_demands, fixture, fixture_shape, scenario_plan, sample_resources, validate_lazy_noop


@contextlib.contextmanager
def minimal_worktree():
    with tempfile.TemporaryDirectory() as directory:
        root=Path(directory);(root/'0/0').mkdir(parents=True)
        (root/'0/0/0').write_bytes(b'0000000\n');(root/'.gitignore').write_bytes(b'out/\n')
        yield root


def child_in(root,mode,*,snapshot=False,write=False):
    env={k:v for k,v in os.environ.items() if k!='GIT_ON_TREE_OID'}
    if snapshot:env['GIT_ON_TREE_OID']='fixture'
    return subprocess.run([sys.executable,'-c',CHILD,mode,'1','[1,1,1]',*(['write'] if write else [])],cwd=root,env=env,capture_output=True)


class ScalabilityHarnessTests(unittest.TestCase):
    def test_denied_proc_observation_does_not_skip_available_filesystem_sample(self):
        samples=dict(runner_peak_descriptors=None,runner_peak_resident_kib=None,
            filesystem_allocated_delta_bytes=0,runner_observation_denied=False)
        with mock.patch('sys.platform','linux'), mock.patch.object(Path,'iterdir',side_effect=PermissionError), mock.patch('os.statvfs',return_value=mock.Mock(f_bavail=9,f_frsize=1024)):
            sample_resources(42,Path('/fixture'),16384,samples)
        self.assertTrue(samples['runner_observation_denied'])
        self.assertIsNone(samples['runner_peak_descriptors'])
        self.assertEqual(samples['filesystem_allocated_delta_bytes'],7168)

    def test_python_noop_allows_root_lookups_and_rejects_recursive_or_payload_work(self):
        for backend in ('fuse','passthrough'):
            description=dict(backend=backend,workload='noop')
            for trees in (0,1):
                validate_lazy_noop(dict(tree_reads=trees,blob_loads=0),description)
            for metrics in (dict(tree_reads=2,blob_loads=0),dict(tree_reads=1,blob_loads=1)):
                with self.assertRaises(AssertionError): validate_lazy_noop(metrics,description)

    def test_full_walk_checks_root_git_invariant_once(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'0/0').mkdir(parents=True)
            (root/'0/0/0').write_bytes(b'0000000\n');(root/'.gitignore').write_bytes(b'out/\n')
            previous=Path.cwd()
            try:
                os.chdir(root)
                with mock.patch.dict(os.environ,{'GIT_ON_TREE_OID':'fixture'}), mock.patch.object(sys,'argv',['child','full','1','[1,1,1]']), mock.patch('os.path.exists',wraps=os.path.exists) as exists, contextlib.redirect_stdout(io.StringIO()):
                    exec(CHILD,{})
                self.assertEqual(sum(call.args==('.git',) for call in exists.call_args_list),1)
            finally: os.chdir(previous)

    def test_eager_materialization_keeps_its_required_full_tree_work(self):
        validate_lazy_noop(dict(tree_reads=6,blob_loads=2),dict(backend='materialized',workload='noop'))

    def test_walk_rejects_nested_git_and_out_subtrees(self):
        for mode in ('full','enumerate'):
            for name in ('.git','out'):
                for snapshot in (False,True):
                    with self.subTest(mode=mode,name=name,snapshot=snapshot),minimal_worktree() as root:
                        unexpected=root/'0'/name;unexpected.mkdir();(unexpected/'extra').write_bytes(b'unexpected')
                        result=child_in(root,mode,snapshot=snapshot)
                        self.assertNotEqual(result.returncode,0,'unexpected nested subtree was hidden')
                        self.assertIn(b'AssertionError',result.stderr)

    def test_walk_rejects_stale_root_out_before_generated_writes(self):
        for mode in ('full','enumerate'):
            for kind in ('directory','file','dangling-symlink'):
                with self.subTest(mode=mode,kind=kind),minimal_worktree() as root:
                    out=root/'out'
                    if kind=='directory':out.mkdir()
                    elif kind=='file':out.write_bytes(b'stale')
                    else:out.symlink_to('missing')
                    result=child_in(root,mode,write=mode=='full')
                    self.assertNotEqual(result.returncode,0,'stale root output was accepted')
                    self.assertIn(b'previous case leaked generated files',result.stderr)

    def test_walk_never_exempts_snapshot_root_git(self):
        for mode in ('full','enumerate'):
            with self.subTest(mode=mode),minimal_worktree() as root:
                (root/'.git').mkdir()
                result=child_in(root,mode,snapshot=True)
                self.assertNotEqual(result.returncode,0,'snapshot exposed root Git metadata')
                self.assertIn(b'AssertionError',result.stderr)

    def test_full_walk_can_generate_root_out_after_namespace_validation(self):
        with minimal_worktree() as root:
            result=child_in(root,'full',snapshot=True,write=True)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertTrue(json.loads(result.stdout.splitlines()[-1])['generated_write'])
            self.assertEqual((root/'out/generated').read_bytes(),b'generated')

    def test_plan_preserves_interactions_and_selects_larger_scale_without_cartesian_repetition(self):
        for source in ('local','remote'):
            cases=scenario_plan('fuse',source,'matrix',['none','ignored','untracked','all'],'both')
            self.assertEqual(len(cases),24)
            for count in (1000,):
                full={(c['policy'],c['verify']) for c in cases if c['files']==count and c['workload']=='full'}
                self.assertEqual(full,{(p,v) for p in ('none','ignored','untracked','all') for v in (False,True)})
            self.assertFalse(any(c['files']>1000 for c in cases))
            for backend,policy in (('materialized','all'),('passthrough','ignored')):
                compact=scenario_plan(backend,source,'matrix',[policy],'both')
                self.assertEqual(len(compact),2)
                self.assertTrue(all(c['workload']=='full' for c in compact))
                self.assertEqual({c['verify'] for c in compact},{False,True})
        namespace=scenario_plan('fuse','remote','namespace',['ignored'],'both')
        self.assertEqual(namespace,[dict(files=1000000,policy='ignored',verify=True,workload='enumerate',invocation='first')])

    def test_fixture_and_real_child_validate_diverse_bytes_complete_names_and_separate_namespace_reads(self):
        from remote import Fixture
        f=Fixture()
        try:
            key=f.root/'key'
            subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
            info=fixture(f,1000,key)
            self.assertEqual(info['fanouts'],[10,100])
            self.assertEqual(info['distinct_objects'],21)
            self.assertEqual(len(info['payload_objects']),16)
            self.assertEqual(len(expected_demands(info,'fuse','noop')),2)
            self.assertEqual(len(expected_demands(info,'fuse','sparse')),4)
            clone=f.root/'clone'
            subprocess.run(['git','clone','--quiet',str(f.repo),str(clone)],env=f.env,check=True)
            def child(mode):return subprocess.run([sys.executable,'-c',CHILD,mode,'1000',json.dumps(info['fanouts'])],cwd=clone,env=f.env,capture_output=True)
            full=child('full');self.assertEqual(full.returncode,0,full.stderr)
            observation=json.loads(full.stdout.splitlines()[-1])
            self.assertEqual(observation['visited_leaves'],1000)
            self.assertEqual(observation['payload_reads'],1101)
            namespace=child('enumerate');self.assertEqual(namespace.returncode,0,namespace.stderr)
            observation=json.loads(namespace.stdout.splitlines()[-1])
            self.assertEqual(observation['visited_leaves'],1000)
            self.assertEqual(observation['payload_reads'],info['namespace_payload_reads'])
            self.assertLess(observation['payload_reads'],1000,'namespace is never labeled as reading every payload')
            target=clone/info['first_path'];target.write_bytes(b'corrupt!')
            self.assertNotEqual(child('full').returncode,0,'wrong bytes accepted')
            target.unlink()
            self.assertNotEqual(child('enumerate').returncode,0,'missing namespace entry accepted')
        finally:f.close()

    def test_wide_shapes_retain_logical_count_and_do_not_set_a_product_ceiling(self):
        import math
        for count in (1,10,1000,10000,100000,1000000,100000000):
            shape=fixture_shape(count)
            self.assertEqual(math.prod(shape),count)
            self.assertLessEqual(max(shape),1000)
        self.assertEqual(fixture_shape(1000000),[100,100,100])


if __name__=='__main__': unittest.main(verbosity=2)
