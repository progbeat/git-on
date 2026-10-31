#!/usr/bin/env python3
"""Focused regression checks for diagnostic synchronization and CPU parsing."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

import fuse_syscall_profile as diagnostic


class DiagnosticTests(unittest.TestCase):
    def test_control_record_stays_valid_during_a_write(self):
        with tempfile.TemporaryDirectory() as directory:
            control = Path(directory) / 'control.json'
            control.write_text('{"generation": 0}\n')
            empty = threading.Event()
            observed = threading.Event()
            values = []

            def reader():
                empty.wait(2)
                try:
                    values.append(json.loads(control.read_text()))
                except BaseException as error:
                    values.append(error)
                finally:
                    observed.set()

            def paused_write(path, text):
                with path.open('w') as output:
                    empty.set()
                    if not observed.wait(2):
                        raise TimeoutError('reader did not inspect the record')
                    return output.write(text)

            thread = threading.Thread(target=reader)
            thread.start()
            try:
                with patch.object(Path, 'write_text', paused_write):
                    diagnostic.save(control, {'generation': 1})
            finally:
                thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(values, [{'generation': 0}])
            self.assertEqual(json.loads(control.read_text()), {'generation': 1})
            self.assertFalse(control.with_name('control.json.new').exists())

    @unittest.skipUnless(sys.platform == 'linux', 'Linux /proc contract')
    def test_cpu_fields_are_nonnegative_for_a_live_owned_process(self):
        value = diagnostic.proc(os.getpid())
        for key in ('user_s', 'system_s', 'reaped_child_user_s', 'reaped_child_system_s'):
            self.assertGreaterEqual(value[key], 0)
        self.assertGreaterEqual(value['threads'], 1)

    @unittest.skipUnless(sys.platform == 'linux', 'Linux /proc contract')
    def test_reaped_process_is_not_reported_as_an_attached_tracee(self):
        child = subprocess.Popen([sys.executable, '-I', '-S', '-c', 'pass'])
        child.wait(timeout=5)
        self.assertEqual(diagnostic.tracer(child.pid), -1)


if __name__ == '__main__':
    unittest.main()
