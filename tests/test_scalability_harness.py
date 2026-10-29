#!/usr/bin/env python3
"""Guard the real workload's root-only metadata allowance without weakening laziness."""
import unittest
import contextlib
import io
import os
from pathlib import Path
import sys
import tempfile
from unittest import mock
from scalability import CHILD, sample_resources, validate_lazy_noop


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
            (root/'0/0/0').write_bytes(b'payload\n');(root/'.gitignore').write_bytes(b'out/\n')
            previous=Path.cwd()
            try:
                os.chdir(root)
                with mock.patch.dict(os.environ,{'GIT_ON_TREE_OID':'fixture'}), mock.patch.object(sys,'argv',['child','full','1','3']), mock.patch('os.path.exists',wraps=os.path.exists) as exists, contextlib.redirect_stdout(io.StringIO()):
                    exec(CHILD,{})
                self.assertEqual(sum(call.args==('.git',) for call in exists.call_args_list),1)
            finally: os.chdir(previous)

    def test_eager_materialization_keeps_its_required_full_tree_work(self):
        validate_lazy_noop(dict(tree_reads=6,blob_loads=2),dict(backend='materialized',workload='noop'))


if __name__=='__main__': unittest.main(verbosity=2)
