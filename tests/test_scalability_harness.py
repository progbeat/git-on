#!/usr/bin/env python3
"""Guard the real workload's root-only metadata allowance without weakening laziness."""
import unittest
from scalability import validate_lazy_noop


class ScalabilityHarnessTests(unittest.TestCase):
    def test_python_noop_allows_root_lookups_and_rejects_recursive_or_payload_work(self):
        for backend in ('fuse','passthrough'):
            description=dict(backend=backend,workload='noop')
            for trees in (0,1):
                validate_lazy_noop(dict(tree_reads=trees,blob_loads=0),description)
            for metrics in (dict(tree_reads=2,blob_loads=0),dict(tree_reads=1,blob_loads=1)):
                with self.assertRaises(AssertionError): validate_lazy_noop(metrics,description)

    def test_eager_materialization_keeps_its_required_full_tree_work(self):
        validate_lazy_noop(dict(tree_reads=6,blob_loads=2),dict(backend='materialized',workload='noop'))


if __name__=='__main__': unittest.main(verbosity=2)
