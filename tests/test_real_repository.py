"""Synthetic harness regressions; these fixtures are not WebKit executions."""
import copy
import contextlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from benchmark import environment
from real_repository import (DEMAND_LIMITS, LIGHT_TESTS, LIGHT_TEST_COUNT, PACK_LIMIT,
    PIN, command, run, validate_reports, validate_test_output)


def report():
    output=f'{"." * LIGHT_TEST_COUNT}\nRan {LIGHT_TEST_COUNT} tests in 0.002s\n\nOK\n'
    observations=[dict(completed=phase, tree_oid=PIN['tree'], test_count=LIGHT_TEST_COUNT,
        test_modules=LIGHT_TESTS, test_output=output, elapsed_ns=1) for phase in ('first','repeat')]
    metrics=dict(tree_reads=10, prepared_tree_entries=419, header_reads=35, blob_loads=35,
        blob_bytes=293353, cache_hits=0, blob_cache_bytes=293353, blob_cache_entries=35,
        blob_cache_limit=16*1024*1024, tree_cache_bytes=74773, tree_cache_entries=10, tree_cache_limit=0,
        remote_raw_blob_imports=35, remote_raw_blob_bytes=293353,
        remote_native_pack_bytes=1234, remote_session_pack_bytes=103382, fuse_requests={'read':35})
    runner=dict(argv=['git-on', PIN['repository'], PIN['commit'], '--write-policy=none', '--', *command()],
        elapsed_ns=10, waited_user_cpu_s=.01, waited_system_cpu_s=.01,
        child_observations=observations, metrics=metrics)
    return dict(type='real_repository', head='c'*40, working_tree_dirty=False, pin=copy.deepcopy(PIN), runner=runner)


class RealRepositoryTests(unittest.TestCase):
    def test_report_can_be_repeated_with_untracked_output_but_rejects_changed_sources(self):
        with tempfile.TemporaryDirectory() as temporary, contextlib.chdir(temporary):
            def git(*args):
                subprocess.run(['git',*args],check=True,capture_output=True)
            git('init','-q')
            Path('source').write_text('committed')
            git('add','source')
            git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','fixture')
            options=SimpleNamespace(binary=Path(__file__).resolve(),output=Path('webkit-results.jsonl'),summary=None)
            with mock.patch('process_capture.capture',return_value=report()['runner']), \
                 mock.patch('real_repository.Path.is_char_device',return_value=True), \
                 mock.patch('real_repository.shutil.which',return_value='/usr/bin/fusermount3'), \
                 mock.patch('builtins.print'):
                run(options)
                self.assertTrue(options.output.exists())
                run(options)
                self.assertFalse(json.loads(options.output.read_text())['working_tree_dirty'])
                Path('source').write_text('edited')
                with self.assertRaises(ValueError):run(options)
                git('add','source')
                with self.assertRaises(ValueError):run(options)

    def test_small_command_runs_twice_without_scanning_unrelated_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            package=root/'Tools/Scripts/libraries/webkitcorepy/webkitcorepy/tests'
            package.mkdir(parents=True)
            (package.parent/'__init__.py').touch();(package/'__init__.py').touch()
            for module in LIGHT_TESTS:
                source='import unittest\nclass Example(unittest.TestCase):\n pass\n'
                if module==LIGHT_TESTS[0]:
                    source+='\n'.join(f'Example.test_{i} = lambda self: self.assertTrue(True)' for i in range(LIGHT_TEST_COUNT))
                (package/(module.rsplit('.',1)[1]+'.py')).write_text(source+'\n')
            # A recursive traversal would follow an unrelated namespace and
            # encounter data that is not a Python program or Git repository.
            (root/'unrelated').mkdir();(root/'unrelated/invalid.py').write_bytes(b'not Python {')
            (root/'unrelated/loop').symlink_to(root/'unrelated',target_is_directory=True)
            env=dict(environment(),GIT_ON_TREE_OID=PIN['tree'])
            result=subprocess.run(command(),cwd=root,env=env,capture_output=True)
            self.assertEqual(result.returncode,0,result.stderr)
            rows=[json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual([r['completed'] for r in rows],['first','repeat'])
            self.assertTrue(all(r['test_count']==LIGHT_TEST_COUNT and r['tree_oid']==PIN['tree'] for r in rows))
            env['GIT_ON_TREE_OID']='0'*40
            result=subprocess.run(command(),cwd=root,env=env,capture_output=True)
            self.assertNotEqual(result.returncode,0)
            self.assertIn(b'pinned WebKit tree',result.stderr)

    def test_run_has_one_public_invocation_and_no_clone_or_preparation(self):
        with tempfile.TemporaryDirectory() as temporary:
            for eager in (False,True):
                with self.subTest(eager=eager):
                    result=copy.deepcopy(report()['runner'])
                    if eager:result['metrics']['blob_loads']=DEMAND_LIMITS['blob_loads']+1
                    options=SimpleNamespace(binary=Path(__file__),output=Path(temporary)/f'{eager}.jsonl',
                        summary=Path(temporary)/f'{eager}.md')
                    def git_metadata(argv,**kwargs):
                        if argv==['git','rev-parse','HEAD']:return 'c'*40+'\n'
                        if argv==['git','diff','--name-only','HEAD']:return b''
                        self.fail(f'unexpected preparation command: {argv}')
                    with mock.patch('process_capture.capture',return_value=result) as captured, \
                         mock.patch('real_repository.subprocess.check_output',side_effect=git_metadata), \
                         mock.patch('real_repository.subprocess.run',side_effect=AssertionError('source preparation is forbidden')), \
                         mock.patch('real_repository.Path.is_char_device',return_value=True), \
                         mock.patch('real_repository.shutil.which',return_value='/usr/bin/fusermount3'):
                        if eager:
                            with self.assertRaises(ValueError):run(options)
                        else:run(options)
                    self.assertEqual(captured.call_count,1)
                    self.assertEqual(captured.call_args.args[0][1:5],
                        [PIN['repository'],PIN['commit'],'--write-policy=none','--'])
                    data=[json.loads(line) for line in options.output.read_text().splitlines()]
                    self.assertEqual(len(data),1,'retain the single actual observation, including on gate failure')
                    self.assertEqual(options.summary.exists(),not eager)

    def test_gate_requires_exact_identity_two_passes_and_complete_timing(self):
        valid=report()
        self.assertEqual(validate_reports([valid],'c'*40)['public_https_invocations'],1)
        for failure in ('head','dirty','pin','source','tree','missing','duplicate','phase','modules','output','time','cpu'):
            bad=copy.deepcopy(valid);data=[bad];runner=bad['runner'];row=runner['child_observations'][0]
            if failure=='head':bad['head']='d'*40
            elif failure=='dirty':bad['working_tree_dirty']=True
            elif failure=='pin':bad['pin']['commit']='d'*40
            elif failure=='source':runner['argv'][1]='file:///prepared-source.git'
            elif failure=='tree':row['tree_oid']='d'*40
            elif failure=='missing':data=[]
            elif failure=='duplicate':data.append(copy.deepcopy(bad))
            elif failure=='phase':runner['child_observations'].pop()
            elif failure=='modules':row['test_modules']=[]
            elif failure=='output':row['test_output']='OK'
            elif failure=='time':runner['elapsed_ns']=1
            elif failure=='cpu':runner['waited_user_cpu_s']=float('nan')
            with self.subTest(failure=failure),self.assertRaises(ValueError):validate_reports(data,'c'*40)

    def test_gate_rejects_eager_metadata_payload_and_transfer(self):
        for name,limit in DEMAND_LIMITS.items():
            for value in (limit+1,None,True):
                bad=report();bad['runner']['metrics'][name]=value
                with self.subTest(metric=name,value=value),self.assertRaises(ValueError):validate_reports([bad],'c'*40)
        for failure in ('transfer','missing-transfer','no-transfer','no-read','decode','bytes','cache'):
            bad=report();m=bad['runner']['metrics']
            if failure=='transfer':m['remote_session_pack_bytes']=PACK_LIMIT+1
            elif failure=='missing-transfer':del m['remote_session_pack_bytes']
            elif failure=='no-transfer':m['remote_session_pack_bytes']=m['remote_native_pack_bytes']=0
            elif failure=='no-read':m['fuse_requests']['read']=0
            elif failure=='decode':m['blob_loads']+=1
            elif failure=='bytes':m['blob_bytes']+=1
            elif failure=='cache':m['blob_cache_bytes']=m['blob_cache_limit']+1
            with self.subTest(failure=failure),self.assertRaises(ValueError):validate_reports([bad],'c'*40)

    def test_test_runner_output_requires_all_named_tests_and_no_failures_or_skips(self):
        good=f'{"." * LIGHT_TEST_COUNT}\nRan {LIGHT_TEST_COUNT} tests in 0.002s\n\nOK\n'
        validate_test_output(0,good)
        for code,output in ((1,good),(0,good.replace(f'Ran {LIGHT_TEST_COUNT}',f'Ran {LIGHT_TEST_COUNT-1}')),
                            (0,good.replace('OK','OK (skipped=1)')),(0,good.replace('OK','FAILED (failures=1)')),(0,'OK')):
            with self.subTest(code=code,output=output),self.assertRaises(ValueError):validate_test_output(code,output)


if __name__=='__main__':unittest.main(verbosity=2)
