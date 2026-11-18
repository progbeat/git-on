"""Behavioral guards for source caching and inherited test counters."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from process_capture import capture
from source_cache import child_code,directory_child_code,directory_names,directory_reopen_code


class SourceCacheTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform=='linux','Linux dirent64 ABI')
    def test_directory_child_checks_partial_seek_reopen_and_empty_results(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'paged').mkdir();(root/'empty').mkdir()
            for name in directory_names(640):(root/'paged'/name).touch()
            commands=[[sys.executable,'-I','-S','-c',code(640)] for code in (directory_child_code,directory_reopen_code)]
            for argv in commands:
                result=subprocess.run(argv,cwd=root,capture_output=True)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual([json.loads(row) for row in result.stdout.splitlines()],
                                 [dict(completed=phase,directory_entries=640) for phase in ('first','repeat')])
            (root/'paged'/directory_names(640)[0]).unlink()
            for argv in commands:self.assertNotEqual(subprocess.run(argv,cwd=root,capture_output=True).returncode,0)

    def test_callback_metrics_use_inherited_descriptor_without_tracing(self):
        code=('import json,os; '
              'fd=int(os.environ["GIT_ON_TEST_METRICS_FD"]); '
              'os.write(fd,json.dumps({"fuse_requests":{"lookup":3,"read":2}}).encode())')
        with tempfile.TemporaryDirectory() as directory:
            result=capture([sys.executable,'-I','-S','-c',code],directory,{},Path(directory),metrics=True)
        self.assertEqual(result['fuse_requests'],{'lookup':3,'read':2})

    def test_child_checks_bytes_and_names_on_both_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'0/0').mkdir(parents=True)
            (root/'0/0/0').write_bytes(b'0000000\n');(root/'.gitignore').write_bytes(b'out/\n')
            argv=[sys.executable,'-I','-S','-c',child_code(1,[1,1,1])]
            result=subprocess.run(argv,cwd=root,capture_output=True)
            self.assertEqual(result.returncode,0,result.stderr)
            rows=[json.loads(row) for row in result.stdout.splitlines()]
            self.assertEqual([r['completed'] for r in rows if r.get('completed') in ('first','repeat')],['first','repeat'])
            self.assertEqual([r['payload_reads'] for r in rows if 'payload_reads' in r],[102,102])
            (root/'0/out').mkdir()
            self.assertNotEqual(subprocess.run(argv,cwd=root,capture_output=True).returncode,0)


if __name__=='__main__':unittest.main()
