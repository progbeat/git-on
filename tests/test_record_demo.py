#!/usr/bin/env python3
"""Protect existing recording artifacts without running VHS or mounting FUSE."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "record_demo", Path(__file__).resolve().parents[1] / "scripts/record_demo.py")
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)


class RecordingDestinationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "git-on"
        self.binary.write_bytes(b"fixture executable\n")
        self.binary.chmod(0o755)
        self.destination = self.root / "recording"
        self.destination.mkdir()

    def attempt_recording(self):
        real_open = os.open

        def fixture_open(path, *args, **kwargs):
            # Allow the old implementation to reach its real log-open operation
            # without requiring a FUSE device or starting a real recorder.
            return real_open(os.devnull if path == "/dev/fuse" else path, *args, **kwargs)

        with patch.object(MOD.shutil, "which", return_value="fixture-tool") as dependencies, \
                patch.object(MOD.os, "open", side_effect=fixture_open) as opened, \
                patch.object(MOD, "output", return_value="fixture version"), \
                patch.object(MOD.platform, "node", return_value="fixture-host"), \
                patch.object(MOD.platform, "platform", return_value="fixture-platform"), \
                patch.object(MOD.subprocess, "Popen", side_effect=RuntimeError(
                    "Fixture VHS recording started")) as recorder:
            error = None
            try:
                MOD.record(self.binary, self.destination)
            except Exception as caught:
                error = caught
        return error, dependencies, opened, recorder

    def assert_rejected_before_probes(self, attempt):
        error, dependencies, opened, recorder = attempt
        self.assertIsInstance(error, RuntimeError)
        self.assertIn("Refusing to overwrite", str(error))
        dependencies.assert_not_called()
        opened.assert_not_called()
        recorder.assert_not_called()

    def test_existing_secondary_log_is_not_truncated(self):
        log = self.destination / "vhs.log"
        log.write_bytes(b"previous recording diagnostics\n")
        attempt = self.attempt_recording()
        self.assertEqual(log.read_bytes(), b"previous recording diagnostics\n")
        self.assertEqual(list(self.destination.iterdir()), [log])
        self.assert_rejected_before_probes(attempt)

    def test_existing_secondary_video_is_not_replaced(self):
        video = self.destination / "demo.mp4"
        video.write_bytes(b"previous recording video\n")
        attempt = self.attempt_recording()
        self.assertEqual(video.read_bytes(), b"previous recording video\n")
        self.assertEqual(list(self.destination.iterdir()), [video])
        self.assert_rejected_before_probes(attempt)

    def test_log_symlink_cannot_truncate_a_file_outside_destination(self):
        outside = self.root / "outside.log"
        outside.write_bytes(b"unrelated diagnostics\n")
        log = self.destination / "vhs.log"
        log.symlink_to(outside)
        attempt = self.attempt_recording()
        self.assertEqual(outside.read_bytes(), b"unrelated diagnostics\n")
        self.assertTrue(log.is_symlink())
        self.assertEqual(log.readlink(), outside)
        self.assert_rejected_before_probes(attempt)

    def test_dangling_artifact_symlink_is_an_existing_entry(self):
        artifact = self.destination / "demo.mp4"
        missing = self.root / "missing-video"
        artifact.symlink_to(missing)
        attempt = self.attempt_recording()
        self.assertTrue(artifact.is_symlink())
        self.assertEqual(artifact.readlink(), missing)
        self.assertFalse(missing.exists())
        self.assertEqual(list(self.destination.iterdir()), [artifact])
        self.assert_rejected_before_probes(attempt)

    def test_existing_directory_contents_are_preserved(self):
        directory = self.destination / "previous-capture"
        directory.mkdir()
        payload = directory / "frame.png"
        payload.write_bytes(b"previous frame\n")
        attempt = self.attempt_recording()
        self.assertEqual(payload.read_bytes(), b"previous frame\n")
        self.assertEqual(list(self.destination.iterdir()), [directory])
        self.assert_rejected_before_probes(attempt)

    def test_new_or_empty_destination_reaches_normal_dependency_checks(self):
        for existing in [True, False]:
            with self.subTest(existing=existing):
                if not existing:
                    self.destination.rmdir()
                with patch.object(MOD.shutil, "which", return_value=None) as dependencies:
                    with self.assertRaisesRegex(RuntimeError, "Missing recording dependency: vhs"):
                        MOD.record(self.binary, self.destination)
                dependencies.assert_called_once_with("vhs")
                self.assertEqual(self.destination.exists(), existing)


if __name__ == "__main__":
    unittest.main()
