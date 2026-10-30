"""The diagnostic must count kernel requests and descendant file I/O, not pipes."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from io_profile import capture,child_code,trace_summary


class IoProfileTests(unittest.TestCase):
    def test_callback_metrics_use_inherited_descriptor_without_tracing(self):
        code=('import json,os; '
              'fd=int(os.environ["GIT_ON_TEST_METRICS_FD"]); '
              'os.write(fd,json.dumps({"fuse_requests":{"lookup":3,"read":2}}).encode())')
        with tempfile.TemporaryDirectory() as directory:
            result=capture([sys.executable,'-I','-S','-c',code],directory,{},Path(directory),metrics=True)
        self.assertEqual(result['fuse_requests'],{'lookup':3,'read':2})
        self.assertFalse(result['traced'])

    def test_trace_includes_native_git_file_writes_and_fuse_request_headers(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);trace=root/'trace.1'
            trace.write_text('execve("/usr/bin/git", ["git", "unpack-objects", "-q"], 0) = 0\n'
                f'write(4<{root}/run/remote-repository/objects/aa/tmp>, "data", 4) = 4 <0.001>\n'
                f'fsync(4<{root}/run/remote-repository/objects/aa/tmp>) = 0 <0.001>\n'
                'write(1<pipe:[12]>, "observer", 8) = 8 <0.001>\n')
            with trace.open('a') as output:
                output.write(f'write(1<pipe:[12]>, "{root}/not-a-file-write", 100) = 100 <0.001>\n')
            fuse=root/'trace.2';fuse.write_text(
                'read(7</dev/fuse<char 10:229>>, "\\x40\\0\\0\\0\\x01\\0\\0\\0", 131072) = 64 <0.001>\n'
                'read(7</dev/fuse<char 10:229>>,  <unfinished ...>\n'
                '<... read resumed>"\\x40\\0\\0\\0\\x0f\\0\\0\\0", 131072) = 64 <0.001>\n')
            result=trace_summary([trace,fuse],root)
            self.assertEqual(result['processes']['unpack-objects'],1)
            self.assertEqual(result['file_io_by_command']['unpack-objects']['objects_write_bytes'],4)
            self.assertEqual(result['file_io_by_command']['unpack-objects']['objects_sync_calls'],1)
            self.assertEqual(result['fuse_requests'],{'lookup':1,'read':1})

    def test_child_checks_bytes_and_names_on_both_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'0/0').mkdir(parents=True)
            (root/'0/0/0').write_bytes(b'0000000\n');(root/'.gitignore').write_bytes(b'out/\n')
            argv=[sys.executable,'-I','-S','-c',child_code(1,[1,1,1])]
            result=subprocess.run(argv,cwd=root,capture_output=True)
            self.assertEqual(result.returncode,0,result.stderr)
            rows=[json.loads(row) for row in result.stdout.splitlines()]
            self.assertEqual([r['profile_phase'] for r in rows if 'profile_phase' in r],['first','repeat'])
            self.assertEqual([r['payload_reads'] for r in rows if 'payload_reads' in r],[102,102])
            (root/'0/out').mkdir()
            self.assertNotEqual(subprocess.run(argv,cwd=root,capture_output=True).returncode,0)


if __name__=='__main__':unittest.main()
