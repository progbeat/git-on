import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from import_profile import CHILD,validate_runner
from ci_evidence import validate_import_profiles
from startup_profile import PHASES,validate_timing


class ImportProfileTests(unittest.TestCase):
    def test_full_timeline_rejects_missing_or_reordered_phases_and_excluded_time(self):
        row=dict(runner_timeline=[dict(phase=phase,monotonic_ns=i+1) for i,phase in enumerate(PHASES)],
            observed_start_monotonic_ns=0,observed_end_monotonic_ns=len(PHASES)+1,elapsed_ns=len(PHASES)+1)
        intervals=validate_timing(row)
        self.assertEqual(sum(intervals.values()),len(PHASES)-1)
        for defect in ('missing','order','clock','total','unproven_backend'):
            broken=copy.deepcopy(row)
            if defect=='missing':broken['runner_timeline'].pop()
            elif defect=='order':broken['runner_timeline'].reverse()
            elif defect=='clock':broken['runner_timeline'][0]['monotonic_ns']=-1
            elif defect=='unproven_backend':broken['runner_timeline'][4]['phase']='backend_ready'
            else:broken['elapsed_ns']-=1
            with self.assertRaises(AssertionError):validate_timing(broken)

    def test_gate_requires_current_head_complete_baseline_and_native_io(self):
        head='a'*40
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for backend,count,traces in [('fuse',160,[False]),('materialized',32,[False,True])]:
                observations=[dict(completed=p,files=count,payload_bytes=count*8192,metadata_result_bytes=count*144) for p in ('first','repeat')]
                identity=dict(head=head,working_tree_dirty=False,backend=backend,files=count,blob_bytes=8192)
                baseline=dict(identity,type='baseline',stat_abi_bytes=144,materialize=dict(elapsed_ns=1),command=dict(elapsed_ns=2,child_observations=observations),cleanup=dict(elapsed_ns=3),total_elapsed_ns=6)
                rows=[baseline]
                for traced in traces:
                    rows.append(dict(identity,type='runner',variant='current',traced=traced,child_observations=observations,
                        metrics=dict(remote_fetches=count+2,remote_fetch_operations=3 if backend=='materialized' else count+2,remote_decode_operations=3 if backend=='materialized' else count+2,remote_raw_blob_imports=count,remote_raw_blob_bytes=count*8192,blob_loads=count,blob_cache_bytes=1,blob_cache_limit=8388608),
                        fuse_requests={'read':1} if backend=='fuse' else {},waited_user_cpu_s=1,waited_system_cpu_s=1,runner_and_waited_children_peak_resident_kib=1000,
                        processes={'cat-file':3 if backend=='materialized' else count+2,'fetch':3 if backend=='materialized' else count+2},file_io_by_command={'other':{'raw_parts_write_bytes':count*8192}}))
                (root/f'import-profile-{backend}.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in rows))
            validate_import_profiles(dict(head=head),root)
            path=root/'import-profile-materialized.jsonl';good=path.read_text()
            for change in ('stale','baseline','trace'):
                data=[json.loads(row) for row in good.splitlines()]
                if change=='stale':data[1]['head']='b'*40
                if change=='baseline':data[0]['total_elapsed_ns']=2
                if change=='trace':data[-1]['file_io_by_command']={}
                path.write_text(''.join(json.dumps(r)+'\n' for r in data))
                with self.assertRaises(ValueError):validate_import_profiles(dict(head=head),root)
            path.unlink()
            with self.assertRaises(ValueError):validate_import_profiles(dict(head=head),root)

    def test_same_child_checks_distinct_bytes_stat_results_and_two_closed_passes(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for index in range(3):(root/f'f{index:05d}').write_bytes(hashlib.shake_256(f'import-profile:{index}'.encode()).digest(32))
            result=subprocess.run([sys.executable,'-I','-S','-c',CHILD,'3','32','144'],cwd=root,capture_output=True,check=True)
            rows=[json.loads(line) for line in result.stdout.splitlines()]
            phases=[r for r in rows if 'completed' in r]
            self.assertEqual([p['completed'] for p in phases],['first','repeat'])
            self.assertTrue(all(p['payload_bytes']==96 and p['metadata_result_bytes']==432 for p in phases))
            (root/'f00001').write_bytes(b'wrong')
            self.assertNotEqual(subprocess.run([sys.executable,'-I','-S','-c',CHILD,'3','32','144'],cwd=root,capture_output=True).returncode,0)

    def test_validator_rejects_repeated_decode_missing_phase_and_wrong_import_bytes(self):
        good=dict(child_observations=[dict(completed=p,files=160,payload_bytes=1310720,metadata_result_bytes=23040) for p in ('first','repeat')],
            metrics=dict(remote_fetches=162,remote_raw_blob_imports=160,remote_raw_blob_bytes=1310720,blob_loads=160,blob_cache_bytes=100,blob_cache_limit=8388608))
        validate_runner(good,160,8192,144)
        for key in ('remote_fetches','remote_raw_blob_imports','remote_raw_blob_bytes','blob_loads'):
            broken=copy.deepcopy(good);broken['metrics'][key]+=1
            with self.assertRaises(AssertionError):validate_runner(broken,160,8192,144)
        broken=copy.deepcopy(good);broken['child_observations'].pop()
        with self.assertRaises(AssertionError):validate_runner(broken,160,8192,144)

    def test_session_counts_replace_native_demands_without_hiding_repeat_work(self):
        good=dict(child_observations=[dict(completed=p,files=160,payload_bytes=1310720,metadata_result_bytes=23040) for p in ('first','repeat')],
            metrics=dict(remote_fetches=162,remote_raw_blob_imports=160,remote_raw_blob_bytes=1310720,blob_loads=160,blob_cache_bytes=100,blob_cache_limit=8388608,
                remote_fetch_operations=1,remote_decode_operations=1,remote_session_fetch_operations=161,remote_session_decode_operations=161,remote_session_starts=1),
            fuse_requests={'read':1})
        validate_runner(good,160,8192,144,'fuse')
        for key in ('remote_fetch_operations','remote_decode_operations','remote_session_fetch_operations','remote_session_decode_operations','remote_session_starts'):
            broken=copy.deepcopy(good);broken['metrics'][key]+=1
            with self.assertRaises(AssertionError):validate_runner(broken,160,8192,144,'fuse')


if __name__=='__main__':unittest.main()
