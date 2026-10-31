"""Negative tests for real-fixture evidence; generated data is not WebKit evidence."""
import copy
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from benchmark import environment
from real_repository import LIGHT_TESTS, MODULES, command, entries, git, manifest, observe, prepare, process_snapshot, cached_pages, validate_reports,validate_test_output
from import_profile import stat_abi_bytes
from startup_profile import PHASES


class RealRepositoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory(); self.addCleanup(self.temporary.cleanup)
        self.root=Path(self.temporary.name); self.repo=self.root/'source'; self.repo.mkdir()
        subprocess.run(['git','init','--quiet',str(self.repo)],check=True,env=environment())
        git(self.repo,'config','user.email','fixture@example.invalid');git(self.repo,'config','user.name','Fixture')
        package=self.repo/MODULES;package.mkdir(parents=True)
        (package/'checker.py').write_bytes(b'answer = 6 * 7\n')
        (package/'__init__.py').write_bytes(b'')
        (package/'checker_unittest_input.py').write_bytes(b'intentionally not valid Python {\n')
        (self.repo/'entry').write_bytes(b'hello\n');(self.repo/'run').write_bytes(b'#!/bin/sh\nexit 0\n');(self.repo/'run').chmod(0o755)
        (self.repo/'empty').write_bytes(b'')
        (self.repo/'link').symlink_to('entry')
        (self.repo/'nested').mkdir();(self.repo/'nested/file').write_bytes(b'x')
        with open(os.fsencode(self.repo)+b'/byte-\xff\nname','wb') as out:out.write(b'bytes')
        git(self.repo,'add','--all');git(self.repo,'commit','--quiet','-m','fixture')
        self.path=self.root/'manifest';self.counts=manifest(self.repo,'HEAD',self.path)

    def execute(self,workload='full',root=None):
        return subprocess.run(command(self.path,workload,144),cwd=root or self.repo,
            env=environment(),capture_output=True)

    def test_complete_walk_and_syntax_workload_check_two_passes_without_executing_source(self):
        result=self.execute();self.assertEqual(result.returncode,0,result.stderr)
        rows=[json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([r['completed'] for r in rows],['first','repeat'])
        self.assertTrue(all(r['files']==self.counts['blob_paths'] and r['payload_bytes']==self.counts['logical_blob_bytes'] for r in rows))
        syntax=self.execute('syntax');self.assertEqual(syntax.returncode,0,syntax.stderr)
        self.assertEqual([json.loads(l)['syntax_modules'] for l in syntax.stdout.splitlines()],[2,2])
        self.assertEqual(self.counts['python_modules'],2)
        self.assertEqual(self.counts['syntax_unique_blobs'],2)
        self.assertEqual(self.counts['syntax_nonempty_unique_blobs'],1)
        self.assertEqual(self.counts['nonempty_unique_blob_objects'],self.counts['unique_blob_objects']-1)
        self.assertFalse(list(self.repo.rglob('__pycache__')))

    def test_wrong_bytes_mode_symlink_or_missing_name_fail(self):
        for failure in ('bytes','mode','link','missing'):
            with self.subTest(failure=failure):
                git(self.repo,'reset','--hard','--quiet','HEAD')
                if failure=='bytes':(self.repo/'entry').write_bytes(b'wrong\n')
                elif failure=='mode':(self.repo/'run').chmod(0o644)
                elif failure=='link':(self.repo/'link').unlink();(self.repo/'link').symlink_to('other')
                else:(self.repo/'entry').unlink()
                self.assertNotEqual(self.execute().returncode,0)

    def test_unexpected_names_are_not_silently_filtered(self):
        # Only actual root clone metadata is omitted. In particular, retain the
        # previous Copilot regression's nested .git/out and root out checks.
        for name in ('nested/.git','nested/out','out','zzz-extra'):
            with self.subTest(name=name):
                extra=self.repo/name;extra.mkdir()
                try:self.assertNotEqual(self.execute().returncode,0)
                finally:extra.rmdir()
        env=dict(environment(),GIT_ON_TREE_OID='test-snapshot')
        result=subprocess.run(command(self.path,'full',144),cwd=self.repo,env=env,capture_output=True)
        self.assertNotEqual(result.returncode,0,'snapshot exposed root clone metadata')

    def test_manifest_preserves_byte_names_and_rejects_truncation_or_traversal(self):
        data=list(entries(self.path));self.assertTrue(any(row[-1]==b'byte-\xff\nname' for row in data))
        original=self.path.read_bytes()
        for malformed in (original[:-1], b'100644 blob '+b'a'*40+b' 1\t../outside\0'):
            self.path.write_bytes(malformed)
            with self.assertRaises(ValueError):list(entries(self.path))

    def test_preparation_rejects_wrong_pin_and_is_idempotent(self):
        pin=dict(repository=str(self.repo),commit=git(self.repo,'rev-parse','HEAD').strip().decode(),
            tree=git(self.repo,'rev-parse','HEAD^{tree}').strip().decode(),
            **{k:self.counts[k] for k in ('blob_paths','logical_blob_bytes','gitlinks')})
        # A local repository advertises the exact fixture commit, without any
        # network or a large public clone in these harness tests.
        destination=self.root/'prepared'
        first=prepare(destination,pin);second=prepare(destination,pin)
        self.assertEqual(first,second)
        self.assertEqual(list(entries(destination/'tree.manifest')),list(entries(self.path)))
        broken=dict(pin,tree='0'*40)
        with self.assertRaises(ValueError):prepare(destination,broken)
        broken=dict(pin,blob_paths=pin['blob_paths']+1)
        with self.assertRaises(ValueError):prepare(destination,broken)

    def test_observer_accounts_descendant_cpu_and_reports_sampling_limits(self):
        code='import json,subprocess,sys,time;subprocess.run([sys.executable,"-c","sum(range(5000000))"],check=True);time.sleep(.06);print(json.dumps({"completed":"test"}))'
        observed=observe([sys.executable,'-I','-S','-c',code],self.root,environment(),5)
        self.assertGreater(observed['waited_user_cpu_s'],0)
        self.assertGreater(observed['memory']['count'],0)
        self.assertGreater(observed['memory']['peak_rss_sum_kib'],0)
        self.assertIn('shared pages',observed['memory_limits'])
        self.assertEqual(observed['command_observations'],[{'completed':'test'}])
        stat_abi_bytes(self.root)
        waited=observe([sys.executable,'-I','-S','-c',code],self.root,environment(),5,resource_observer=self.root/'resource-observer')
        self.assertGreater(waited['waited_process_peak_rss_kib'],0)

    def test_descriptor_permission_does_not_discard_observable_rss(self):
        proc=self.root/'proc';base=proc/'123';(base/'task/123').mkdir(parents=True);(base/'fd').mkdir()
        (base/'stat').write_text('123 (observer) '+ ' '.join(['0']*19+['6789']))
        (base/'status').write_text('VmRSS: 4321 kB\n')
        (base/'task/123/children').write_text('')
        original=Path.iterdir
        def directories(path):
            if path==base/'fd':raise PermissionError('protected descriptors')
            return original(path)
        with mock.patch('real_repository.Path',return_value=proc),mock.patch.object(Path,'iterdir',directories):
            self.assertEqual(process_snapshot(123),(4321,0,True,False))

    def test_metrics_and_complete_timeline_survive_the_waited_peak_wrapper(self):
        stat_abi_bytes(self.root)
        metrics=dict(blob_cache_bytes=0,blob_cache_limit=16*1024*1024,tree_cache_bytes=0,tree_cache_limit=0)
        code=f"""import json,os,time
phases={PHASES!r}
events=[dict(phase=p,monotonic_ns=time.monotonic_ns()) for p in phases]
for key,value in [('GIT_ON_TEST_METRICS_FD',{metrics!r}),('GIT_ON_TEST_TIMING_FD',events)]:
 with os.fdopen(int(os.environ[key]),'w') as output:json.dump(value,output)
print(json.dumps(dict(completed='channels')))
"""
        observed=observe([sys.executable,'-I','-S','-c',code],self.root,environment(),5,metrics=True,resource_observer=self.root/'resource-observer')
        self.assertEqual(observed['metrics'],metrics)
        self.assertEqual(observed['argv'][0],sys.executable)
        self.assertNotIn(str(self.root/'resource-observer'),observed['argv'])
        self.assertEqual([event['phase'] for event in observed['runner_timeline']],PHASES)
        self.assertGreater(observed['waited_process_peak_rss_kib'],0)
        self.assertLessEqual(sum(observed['intervals'].values()),observed['elapsed_ns'])

    @unittest.skipUnless(sys.platform=='linux','Linux mincore performance diagnostic')
    def test_cache_probe_reports_owned_file_residency_without_changing_bytes(self):
        path=self.root/'cache-observation';data=b'unchanged'*1024;path.write_bytes(data)
        observed=cached_pages(path,len(data))
        self.assertGreater(observed['pages'],0)
        self.assertIsInstance(observed['resident'],int)
        self.assertLessEqual(observed['resident'],observed['pages'])
        self.assertEqual(path.read_bytes(),data)
        self.assertIn('unavailable_errno',cached_pages(self.root/'absent',len(data)))
        completed=subprocess.run([*command(self.path,'full',144),'--profile-cache'],cwd=self.repo,env=environment(),capture_output=True,check=True)
        rows=[json.loads(line) for line in completed.stdout.splitlines()]
        self.assertTrue(all(row['diagnostic_cache']['samples'] for row in rows))
        self.assertTrue(all(row['files']==self.counts['blob_paths'] for row in rows))

    def test_failed_command_is_not_a_successful_measurement(self):
        with self.assertRaises(RuntimeError):observe([sys.executable,'-c','raise SystemExit(3)'],self.root,environment(),5)

    def test_timeout_does_not_leave_owned_descendants_running(self):
        marker=self.root/'escaped-child'
        nested=f'import time,signal;from pathlib import Path;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(.5);Path({str(marker)!r}).write_text("leak")'
        code=f'import subprocess,sys,time;subprocess.Popen([sys.executable,"-c",{nested!r}]);time.sleep(10)'
        with self.assertRaises(subprocess.TimeoutExpired):observe([sys.executable,'-c',code],self.root,environment(),.1)
        time.sleep(.7)
        self.assertFalse(marker.exists(),'timed-out measurement left its child running')

    def test_waited_peak_wrapper_preserves_owned_child_graceful_timeout_cleanup(self):
        stat_abi_bytes(self.root)
        marker=self.root/'orphan-survived';pidfile=self.root/'owned-child';ready=self.root/'handler-ready'
        descendant=f'import time;from pathlib import Path;time.sleep(1);Path({str(marker)!r}).write_text("leak")'
        code=f"""import os,signal,subprocess,sys,time
from pathlib import Path
child=subprocess.Popen([sys.executable,'-I','-S','-c',{descendant!r}],start_new_session=True)
Path({str(pidfile)!r}).write_text(str(child.pid))
def stop(sig,frame):
 time.sleep(.2)
 os.killpg(child.pid,signal.SIGKILL)
 child.wait()
 raise SystemExit(0)
signal.signal(signal.SIGTERM,stop)
Path({str(ready)!r}).touch()
time.sleep(30)
"""
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                observe([sys.executable,'-I','-S','-c',code],self.root,environment(),.5,resource_observer=self.root/'resource-observer')
            self.assertTrue(ready.exists(),'measured command never installed its cleanup handler')
            time.sleep(1.1)
            self.assertFalse(marker.exists(),'measurement wrapper killed the owner before descendant cleanup')
        finally:
            if pidfile.exists():
                try:os.killpg(int(pidfile.read_text()),signal.SIGKILL)
                except ProcessLookupError:pass

    def test_required_gate_rejects_stale_incomplete_or_unmeasured_reports(self):
        # Synthetic parser records: no real mount or WebKit execution is claimed.
        pin=dict(repository='generated',commit='a'*40,tree='b'*40,
            **{k:self.counts[k] for k in ('blob_paths','logical_blob_bytes','gitlinks')})
        identity=dict(head='c'*40,working_tree_dirty=False,fixture=dict(pin=pin,counts=self.counts))
        resource=dict(elapsed_ns=10,waited_user_cpu_s=.01,waited_system_cpu_s=.01,waited_process_peak_rss_kib=1234,
            waited_input_block_bytes=0,waited_output_block_bytes=0,
            memory=dict(count=1,peak_rss_sum_kib=1234,unavailable=False),command_observations=[])
        common=dict(materialize=dict(elapsed_ns=20,commands=[resource,resource]),cleanup_ns=5,
            shared_materialization_samples=1,stored_received_pack_bytes=123)
        data=[dict(type='real_repository_baseline',source=source,**identity,**common) for source in ('local','remote')]
        for workload,source,trial in [('full','local',0)]+[('tests',s,t) for s in ('local','remote') for t in range(3)]:
            full=workload=='full';files=self.counts['blob_paths' if full else 'python_modules']
            observations=[dict(completed=p,workload=workload,files=files,syntax_modules=0 if full else files,
                payload_bytes=self.counts['logical_blob_bytes' if full else 'syntax_path_bytes'],metadata_result_bytes=144*files,elapsed_ns=4) for p in ('first','repeat')]
            if not full:
                observations=[dict(completed=p,workload=workload,test_count=32,test_modules=LIGHT_TESTS,
                    test_output='................................\nRan 32 tests in 0.002s\n\nOK\n',elapsed_ns=4) for p in ('first','repeat')]
            executed=dict(resource,command_observations=observations)
            metrics=dict(blob_loads=self.counts['nonempty_unique_blob_objects' if full else 'syntax_nonempty_unique_blobs'],fuse_requests={'read':1},
                blob_cache_bytes=1,blob_cache_limit=16*1024*1024,tree_cache_bytes=4096,tree_cache_limit=0,
                remote_native_pack_bytes=64 if source=='remote' else 0,remote_session_pack_bytes=80 if source=='remote' else 0,
                remote_raw_blob_bytes=self.counts['syntax_blob_bytes'] if source=='remote' else 0,
                remote_raw_blob_imports=self.counts['syntax_unique_blobs'] if source=='remote' else 0)
            transfer=dict(remote_verified_blob_bytes=metrics['remote_raw_blob_bytes'],remote_verified_blobs=metrics['remote_raw_blob_imports'],
                session_pack_payload_bytes=metrics['remote_session_pack_bytes'],native_stored_received_pack_bytes=metrics['remote_native_pack_bytes'])
            data.append(dict(type='real_repository',workload=workload,source=source,trial=trial,**identity,
                baseline=dict(common,direct_command=executed,total_elapsed_ns=35),
                runner=dict(executed,metrics=metrics,elapsed_ns=20,observed_start_monotonic_ns=0,observed_end_monotonic_ns=20,
                    runner_timeline=[dict(phase=p,monotonic_ns=i+1) for i,p in enumerate(PHASES)]),
                profile_spans_enabled=False,transfer_evidence=transfer))
        validate_reports(data,'c'*40,pin)
        partial=copy.deepcopy(data)
        partial[2]['runner']=copy.deepcopy(partial[2]['runner'])
        partial[2]['runner']['memory']['unavailable']=True
        self.assertEqual(validate_reports(partial,'c'*40,pin)['partial_process_tree_rss_samples'],1)
        # Partial estimates remain explicit and never replace the primary
        # waited peak or an entirely absent runner observation.
        partial[2]['runner']['waited_process_peak_rss_kib']=None
        with self.assertRaises(ValueError):validate_reports(partial,'c'*40,pin)
        partial[2]['runner']['waited_process_peak_rss_kib']=1234
        partial[2]['runner']['memory']['count']=0
        with self.assertRaises(ValueError):validate_reports(partial,'c'*40,pin)
        for failure in ('head','missing','duplicate','phase','bytes','decode','setup','memory','cpu','transfer','timeline','profiling'):
            bad=copy.deepcopy(data);row=bad[2]
            if failure=='head':row['head']='d'*40
            elif failure=='missing':bad.pop()
            elif failure=='duplicate':bad[-1]=copy.deepcopy(bad[-2])
            elif failure=='phase':row['runner']['command_observations'].pop()
            elif failure=='bytes':row['runner']['command_observations'][0]['payload_bytes']-=1
            elif failure=='decode':row['runner']['metrics']['blob_loads']+=1
            elif failure=='setup':row['baseline']['total_elapsed_ns']-=20
            elif failure=='memory':row['runner']['waited_process_peak_rss_kib']=None
            elif failure=='cpu':del row['runner']['waited_user_cpu_s']
            elif failure=='transfer':row['transfer_evidence']['session_pack_payload_bytes']+=1
            elif failure=='timeline':row['runner']['runner_timeline'].pop()
            else:row['profile_spans_enabled']=True
            with self.subTest(failure=failure),self.assertRaises(ValueError):validate_reports(bad,'c'*40,pin)

    def test_test_runner_output_requires_all_named_tests_and_no_failures_or_skips(self):
        good='................................\nRan 32 tests in 0.002s\n\nOK\n'
        validate_test_output(0,good)
        for code,output in ((1,good),(0,good.replace('32','31')),(0,good.replace('OK','OK (skipped=1)')),
                            (0,good.replace('OK','FAILED (failures=1)')),(0,'OK')):
            with self.subTest(code=code,output=output),self.assertRaises(ValueError):validate_test_output(code,output)


if __name__=='__main__':unittest.main(verbosity=2)
