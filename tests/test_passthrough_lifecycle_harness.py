#!/usr/bin/env python3
"""A live readiness producer may pause between creating and completing JSON."""
import importlib.util
from pathlib import Path
import tempfile
import threading
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('lifecycle', Path(__file__).with_name('passthrough_lifecycle.py'))
lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lifecycle)


class ReadinessTests(unittest.TestCase):
    def test_created_but_incomplete_json_waits_for_live_producer(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'ready'
            path.write_text('{"pid":')
            resume, done = threading.Event(), threading.Event()
            def publish():
                resume.wait(5)
                path.write_text('{"pid":123}')
                done.set()
            producer = threading.Thread(target=publish)
            producer.start()
            def poll_delay(_seconds):
                resume.set()
                self.assertTrue(done.wait(2))
            try:
                with mock.patch.object(lifecycle.time, 'sleep', side_effect=poll_delay):
                    self.assertEqual(lifecycle.wait_file(path, mock.Mock(poll=lambda: None), seconds=2), {'pid':123})
            finally:
                resume.set()
                producer.join(5)

    def test_incomplete_json_does_not_hide_a_dead_producer(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'ready'
            path.write_text('{')
            process = mock.Mock(poll=lambda: 9, returncode=9)
            with self.assertRaisesRegex(AssertionError, 'process exited 9'):
                lifecycle.wait_file(path, process)


if __name__ == '__main__':
    unittest.main(verbosity=2)
