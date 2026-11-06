import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from object_reuse import CHILD,validate_runner,native_command_audit,native_commands
from benchmark import environment
from ci_evidence import validate_object_reuse


class ObjectReuseTests(unittest.TestCase):
    def test_gate_requires_current_head_complete_two_passes_and_native_commands(self):
        head='a'*40
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for backend,count in [('fuse',160),('materialized',32)]:
                operations=3 if backend=='materialized' else count+2
                row=dict(head=head,working_tree_dirty=False,backend=backend,type='runner',files=count,blob_bytes=8192,stat_abi_bytes=144,
                    child_observations=[dict(completed=p,files=count,payload_bytes=count*8192,metadata_result_bytes=count*144) for p in ('first','repeat')],
                    metrics=dict(remote_fetches=count+2,remote_fetch_operations=operations,remote_decode_operations=operations,
                        remote_raw_blob_imports=count,remote_raw_blob_bytes=count*8192,blob_loads=count,blob_cache_bytes=1,blob_cache_limit=16*1024*1024),
                    fuse_requests={'read':1} if backend=='fuse' else {},native_commands={'cat-file':operations,'fetch':operations})
                (root/f'object-reuse-{backend}.jsonl').write_text(json.dumps(row)+'\n')
            validate_object_reuse(dict(head=head),root)
            path=root/'object-reuse-materialized.jsonl';good=json.loads(path.read_text())
            for change in ('stale','decode','native','unpack','duplicate','stat'):
                row=copy.deepcopy(good)
                if change=='stale':row['head']='b'*40
                elif change=='decode':row['metrics']['blob_loads']+=1
                elif change=='native':row['native_commands']['cat-file']+=1
                elif change=='unpack':row['native_commands']['unpack-objects']=1
                elif change=='stat':row['stat_abi_bytes']=0
                content=json.dumps(row)+'\n'
                if change=='duplicate':content+=content
                path.write_text(content)
                with self.subTest(change=change),self.assertRaises(ValueError):validate_object_reuse(dict(head=head),root)
            path.unlink()
            with self.assertRaises(ValueError):validate_object_reuse(dict(head=head),root)

    def test_native_command_audit_executes_real_git_and_preserves_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);env,log=native_command_audit(root,environment(),True)
            result=subprocess.run(['git','version'],env=env,capture_output=True,text=True,check=True)
            self.assertTrue(result.stdout.startswith('git version '))
            failed=subprocess.run(['git','cat-file','-t','not-an-oid'],env=env,capture_output=True)
            self.assertNotEqual(failed.returncode,0)
            self.assertEqual(native_commands(log),{'cat-file':1})
            self.assertEqual([json.loads(row) for row in log.read_text().splitlines()],[['version'],['cat-file','-t','not-an-oid']])

    def test_same_child_checks_distinct_bytes_stat_results_and_two_closed_passes(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for index in range(3):(root/f'f{index:05d}').write_bytes(hashlib.shake_256(f'object-reuse:{index}'.encode()).digest(32))
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
            fuse_requests={'read':1},native_commands={'fetch':1,'cat-file':1})
        validate_runner(good,160,8192,144,'fuse')
        for key in ('remote_fetch_operations','remote_decode_operations','remote_session_fetch_operations','remote_session_decode_operations','remote_session_starts'):
            broken=copy.deepcopy(good);broken['metrics'][key]+=1
            with self.assertRaises(AssertionError):validate_runner(broken,160,8192,144,'fuse')


if __name__=='__main__':unittest.main()
