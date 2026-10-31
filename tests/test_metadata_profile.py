import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from metadata_profile import CHILD,validate_runner


class MetadataProfileTests(unittest.TestCase):
    def test_command_checks_bytes_and_metadata_on_two_closed_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for index in range(3):(root/f'f{index:06d}').write_bytes(b'x')
            result=subprocess.run([sys.executable,'-I','-S','-c',CHILD,'3','7','144'],cwd=root,capture_output=True,check=True)
            phases=[r for line in result.stdout.splitlines() if 'completed' in (r:=json.loads(line))]
            self.assertEqual([p['completed'] for p in phases],['first','repeat'])
            self.assertTrue(all(p['payload_bytes']==7 and p['metadata_result_bytes']==1008 for p in phases))
            (root/'f000001').write_bytes(b'y')
            self.assertNotEqual(subprocess.run([sys.executable,'-I','-S','-c',CHILD,'3','7','144'],cwd=root,capture_output=True).returncode,0)

    def test_gate_rejects_repeated_decode_incomplete_data_and_wrong_backend(self):
        good=dict(child_observations=[dict(completed=phase,operations=7,payload_bytes=7,metadata_result_bytes=1008) for phase in ('first','repeat')],
            metrics=dict(tree_reads=1,prepared_tree_entries=100000,blob_loads=1,tree_cache_bytes=216,tree_cache_limit=4194304,blob_cache_bytes=1,blob_cache_limit=8388608),fuse_requests={'read':1})
        validate_runner(good,100000,7,144,'fuse')
        retained=copy.deepcopy(good);retained['metrics']['tree_cache_limit']=0
        validate_runner(retained,100000,7,144,'fuse')
        for key in ('tree_reads','prepared_tree_entries','blob_loads','tree_cache_bytes','blob_cache_bytes'):
            bad=copy.deepcopy(good)
            bad['metrics'][key]=100000000
            with self.assertRaises(AssertionError):validate_runner(bad,100000,7,144,'fuse')
        for key in ('operations','payload_bytes','metadata_result_bytes'):
            bad=copy.deepcopy(good);bad['child_observations'][1][key]+=1
            with self.assertRaises(AssertionError):validate_runner(bad,100000,7,144,'fuse')
        bad=copy.deepcopy(good);bad['child_observations'].pop()
        with self.assertRaises(AssertionError):validate_runner(bad,100000,7,144,'fuse')
        with self.assertRaises(AssertionError):validate_runner(good,100000,7,144,'materialized')
        bad=copy.deepcopy(good);bad['fuse_requests']={}
        with self.assertRaises(AssertionError):validate_runner(bad,100000,7,144,'fuse')


if __name__=='__main__':unittest.main()
