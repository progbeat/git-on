"""Behavioral guards for source caching and inherited test counters."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from process_capture import capture
from source_cache import child_code


class SourceCacheTests(unittest.TestCase):
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
