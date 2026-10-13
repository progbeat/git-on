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

    def attempt_recording(self, dependency_probe=None):
        real_open = os.open

        def fixture_open(path, *args, **kwargs):
            # Allow the old implementation to reach its real log-open operation
            # without requiring a FUSE device or starting a real recorder.
            return real_open(os.devnull if path == "/dev/fuse" else path, *args, **kwargs)

        with patch.object(MOD.shutil, "which", return_value="fixture-tool",
                          side_effect=dependency_probe) as dependencies, \
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

    def test_destination_replaced_after_preflight_cannot_clobber_outside_log(self):
        outside = self.root / "outside-recording"
        outside.mkdir()
        outside_log = outside / "vhs.log"
        outside_log.write_bytes(b"outside recording diagnostics\n")
        replaced = False

        def replace_after_preflight(tool):
            nonlocal replaced
            if not replaced:
                self.destination.rmdir()
                self.destination.symlink_to(outside)
                replaced = True
            return "fixture-tool"

        error, dependencies, _, recorder = self.attempt_recording(replace_after_preflight)
        self.assertTrue(dependencies.called)
        self.assertTrue(replaced)
        self.assertEqual(outside_log.read_bytes(), b"outside recording diagnostics\n")
        self.assertEqual(list(outside.iterdir()), [outside_log])
        self.assertTrue(self.destination.is_symlink())
        self.assertEqual(self.destination.readlink(), outside)
        self.assertIsInstance(error, RuntimeError)
        self.assertIn("Refusing to overwrite", str(error))
        recorder.assert_not_called()

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

    def test_successful_publication_preserves_all_generated_artifact_bytes(self):
        for existing in [True, False]:
            with self.subTest(existing=existing):
                for path in self.destination.iterdir():
                    path.unlink()
                if not existing:
                    self.destination.rmdir()
                artifacts = {"demo.gif": b"animation fixture", "demo.json": b"metadata fixture",
                             "vhs.log": b"recording diagnostics"}
                with MOD.RecordingDestination(self.destination) as target:
                    target.reserve()
                    with tempfile.TemporaryDirectory(dir=f"/proc/self/fd/{target.fd}") as temporary:
                        staging = Path(temporary)
                        for name, payload in artifacts.items():
                            (staging / name).write_bytes(payload)
                        fd = os.open(staging, os.O_RDONLY | os.O_DIRECTORY)
                        try:
                            target.publish(fd, staging.name)
                        finally:
                            os.close(fd)
                self.assertEqual({path.name: path.read_bytes() for path in self.destination.iterdir()}, artifacts)

    def test_publication_cannot_replace_a_competing_artifact(self):
        real_link = os.link
        with MOD.RecordingDestination(self.destination) as target:
            target.reserve()
            with tempfile.TemporaryDirectory(dir=f"/proc/self/fd/{target.fd}") as temporary:
                staging = Path(temporary)
                (staging / "vhs.log").write_bytes(b"new recording diagnostics")
                fd = os.open(staging, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    def competing_link(*args, **kwargs):
                        (self.destination / "vhs.log").write_bytes(b"another writer's diagnostics")
                        return real_link(*args, **kwargs)

                    with patch.object(MOD.os, "link", side_effect=competing_link):
                        with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                            target.publish(fd, staging.name)
                    self.assertEqual((self.destination / "vhs.log").read_bytes(), b"another writer's diagnostics")
                finally:
                    os.close(fd)

    def test_late_destination_substitution_cannot_redirect_publication(self):
        outside = self.root / "outside-recording"
        outside.mkdir()
        outside_log = outside / "vhs.log"
        outside_log.write_bytes(b"outside diagnostics")
        original = self.root / "original-recording"
        with MOD.RecordingDestination(self.destination) as target:
            target.reserve()
            with tempfile.TemporaryDirectory(dir=f"/proc/self/fd/{target.fd}") as temporary:
                staging = Path(temporary)
                (staging / "vhs.log").write_bytes(b"new diagnostics")
                fd = os.open(staging, os.O_RDONLY | os.O_DIRECTORY)
                self.destination.rename(original)
                self.destination.symlink_to(outside)
                try:
                    with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                        target.publish(fd, staging.name)
                    self.assertEqual(outside_log.read_bytes(), b"outside diagnostics")
                finally:
                    os.close(fd)
        self.assertEqual(list(original.iterdir()), [])
        self.assertEqual(list(outside.iterdir()), [outside_log])


if __name__ == "__main__":
    unittest.main()
