#!/usr/bin/env python3
"""Protect existing recording artifacts without running VHS or mounting FUSE."""
from contextlib import contextmanager
import errno
import importlib.util
import os
from pathlib import Path
import shutil
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

    @contextmanager
    def staged_publication(self, artifacts):
        with MOD.RecordingDestination(self.destination) as target:
            target.reserve()
            staging = Path(tempfile.mkdtemp(dir=f"/proc/self/fd/{target.fd}"))
            for name, payload in artifacts.items():
                (staging / name).write_bytes(payload)
            fd = os.open(staging, os.O_RDONLY | os.O_DIRECTORY)
            try:
                yield target, staging, fd
            finally:
                os.close(fd)
                if not getattr(target, "published", False):
                    shutil.rmtree(staging)

    def publication_order(self, staging_fd, names):
        real_listdir = os.listdir
        return patch.object(MOD.os, "listdir", side_effect=lambda fd:
                            names if fd == staging_fd else real_listdir(fd))

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
                    shutil.rmtree(path) if path.is_dir() else path.unlink()
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
                self.assertEqual(list(self.destination.iterdir()), [self.destination / "capture"])
                self.assertEqual({path.name: path.read_bytes()
                                  for path in (self.destination / "capture").iterdir()}, artifacts)

    def test_publication_cannot_replace_a_competing_capture_created_after_preflight(self):
        real_rename = MOD.rename_noreplace
        with MOD.RecordingDestination(self.destination) as target:
            target.reserve()
            with tempfile.TemporaryDirectory(dir=f"/proc/self/fd/{target.fd}") as temporary:
                staging = Path(temporary)
                (staging / "vhs.log").write_bytes(b"new recording diagnostics")
                fd = os.open(staging, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    def competing_capture(*args, **kwargs):
                        capture = self.destination / "capture"
                        capture.mkdir()
                        (capture / "vhs.log").write_bytes(b"another writer's diagnostics")
                        return real_rename(*args, **kwargs)

                    with patch.object(MOD, "rename_noreplace", side_effect=competing_capture):
                        with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                            target.publish(fd, staging.name)
                    self.assertEqual((self.destination / "capture/vhs.log").read_bytes(), b"another writer's diagnostics")
                finally:
                    os.close(fd)

    def test_existing_capture_is_rejected_before_publication(self):
        artifacts = {"demo.gif": b"new animation", "demo.json": b"new metadata"}
        with self.staged_publication(artifacts) as (target, staging, fd):
            capture = self.destination / "capture"
            capture.mkdir()
            competitor = capture / "demo.json"
            competitor.write_bytes(b"another recording's metadata")
            with patch.object(MOD, "rename_noreplace", wraps=MOD.rename_noreplace) as renamed:
                with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                    target.publish(fd, staging.name)
            renamed.assert_not_called()
            self.assertFalse((self.destination / "demo.gif").exists())
            self.assertEqual(competitor.read_bytes(), b"another recording's metadata")
            self.assertEqual({name: (staging / name).read_bytes() for name in artifacts}, artifacts)
        self.assertEqual(list(self.destination.iterdir()), [capture])

    def test_publication_io_error_exposes_no_partial_artifacts(self):
        artifacts = {"demo.gif": b"new animation", "demo.json": b"new metadata"}
        with self.staged_publication(artifacts) as (target, staging, fd):
            with patch.object(MOD, "rename_noreplace", side_effect=OSError(
                    errno.ENOSPC, "Fixture publication ran out of space")):
                with self.assertRaisesRegex(OSError, "Fixture publication ran out of space"):
                    target.publish(fd, staging.name)
            self.assertFalse((self.destination / "capture").exists())
            self.assertFalse((self.destination / "demo.gif").exists())
            self.assertFalse((self.destination / "demo.json").exists())
            self.assertEqual({name: (staging / name).read_bytes() for name in artifacts}, artifacts)
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_unsupported_later_artifact_is_rejected_before_any_publication(self):
        with self.staged_publication({"demo.gif": b"new animation"}) as (target, staging, fd):
            unsupported = staging / "frames"
            unsupported.mkdir()
            with self.publication_order(fd, ["demo.gif", "frames"]), \
                    patch.object(MOD, "rename_noreplace", wraps=MOD.rename_noreplace) as renamed:
                with self.assertRaisesRegex(RuntimeError, "Unsupported recording artifact: frames"):
                    target.publish(fd, staging.name)
            renamed.assert_not_called()
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_postpublication_directory_substitution_preserves_entire_capture(self):
        artifacts = {"demo.gif": b"new animation", "demo.json": b"new metadata",
                     "vhs.log": b"new diagnostics"}
        original = self.root / "original-recording"
        outside = self.root / "outside-recording"
        outside.mkdir()
        real_rename = MOD.rename_noreplace
        with self.staged_publication(artifacts) as (target, staging, fd):
            staging_name = staging.name

            def substitute_after_publication(*args, **kwargs):
                real_rename(*args, **kwargs)
                capture = self.destination / "capture"
                (capture / "demo.gif").write_bytes(b"another recording's animation")
                (capture / "additional.txt").write_bytes(b"another recording's extra artifact")
                recreated = self.destination / staging_name
                recreated.mkdir()
                (recreated / "foreign.txt").write_bytes(b"another writer reused the staging name")
                self.destination.rename(original)
                self.destination.symlink_to(outside)

            with patch.object(MOD, "rename_noreplace", side_effect=substitute_after_publication):
                with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                    target.publish(fd, staging.name)
            self.assertTrue(target.published)
            # Cleanup is explicitly disarmed once the group becomes visible.
            target.discard_staging(staging.name)
            capture = original / "capture"
            self.assertEqual((capture / "demo.gif").read_bytes(), b"another recording's animation")
            self.assertEqual((capture / "additional.txt").read_bytes(), b"another recording's extra artifact")
            self.assertEqual((capture / "demo.json").read_bytes(), artifacts["demo.json"])
            self.assertEqual((capture / "vhs.log").read_bytes(), artifacts["vhs.log"])
            self.assertEqual((original / staging_name / "foreign.txt").read_bytes(),
                             b"another writer reused the staging name")
        self.assertCountEqual(original.iterdir(), [original / "capture", original / staging_name])
        self.assertEqual((original / staging_name / "foreign.txt").read_bytes(),
                         b"another writer reused the staging name")
        self.assertEqual(list(outside.iterdir()), [])

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

    def test_rollback_cannot_delete_a_competitor_arriving_after_ownership_check(self):
        artifacts = {"demo.gif": b"new animation", "demo.json": b"new metadata"}
        first = self.destination / "demo.gif"
        replacement = self.root / "replacement.gif"
        replacement.write_bytes(b"another recording's animation")
        real_link, real_unlink = os.link, os.unlink
        real_rename = getattr(MOD, "rename_noreplace", None)
        replaced = False
        with self.staged_publication(artifacts) as (target, staging, fd):
            def fail_second_link(source, destination, **kwargs):
                if source == "demo.json":
                    raise OSError(errno.ENOSPC, "Fixture publication ran out of space")
                return real_link(source, destination, **kwargs)

            def replace_after_ownership_check(name, **kwargs):
                nonlocal replaced
                if name == "demo.gif" and kwargs.get("dir_fd") == target.fd:
                    # The helper has already compared the inode. Replace the
                    # actual directory entry before its unlink syscall runs.
                    self.assertTrue(first.samefile(staging / "demo.gif"))
                    os.replace(replacement, first)
                    replaced = True
                return real_unlink(name, **kwargs)

            def compete_with_atomic_publication(*args, **kwargs):
                nonlocal first, replaced
                # With grouped publication there is no ownership/unlink gap.
                # Schedule the same competing writer at its sole visibility
                # boundary: an existing capture must survive RENAME_NOREPLACE.
                capture = self.destination / "capture"
                capture.mkdir()
                first = capture / "demo.gif"
                os.replace(replacement, first)
                replaced = True
                return real_rename(*args, **kwargs)

            with self.publication_order(fd, list(artifacts)), \
                    patch.object(MOD.os, "link", side_effect=fail_second_link), \
                    patch.object(MOD.os, "unlink", side_effect=replace_after_ownership_check), \
                    patch.object(MOD, "rename_noreplace", side_effect=compete_with_atomic_publication,
                                 create=True):
                with self.assertRaises((OSError, RuntimeError)):
                    target.publish(fd, staging.name)
            self.assertTrue(replaced, "controlled competitor did not reach the ownership/unlink boundary")
            self.assertTrue(first.exists(), "rollback deleted another recording's replacement")
            self.assertEqual(first.read_bytes(), b"another recording's animation")

    def test_late_capture_file_or_symlink_is_not_replaced(self):
        for kind in ("regular", "dangling symlink"):
            with self.subTest(kind=kind):
                capture = self.destination / "capture"
                capture.unlink(missing_ok=True)
                real_rename = MOD.rename_noreplace
                with self.staged_publication({"demo.gif": b"new animation"}) as (target, staging, fd):
                    def competing_entry(*args, **kwargs):
                        if kind == "regular":
                            capture.write_bytes(b"another writer's artifact")
                        else:
                            capture.symlink_to(self.root / "missing-recording")
                        return real_rename(*args, **kwargs)

                    with patch.object(MOD, "rename_noreplace", side_effect=competing_entry):
                        with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                            target.publish(fd, staging.name)
                    self.assertFalse(target.published)
                    self.assertEqual((staging / "demo.gif").read_bytes(), b"new animation")
                if kind == "regular":
                    self.assertEqual(capture.read_bytes(), b"another writer's artifact")
                else:
                    self.assertTrue(capture.is_symlink())
                    self.assertEqual(capture.readlink(), self.root / "missing-recording")

    def test_publication_moves_every_artifact_in_one_operation(self):
        artifacts = {"demo.gif": b"new animation", "demo.json": b"new metadata",
                     "vhs.log": b"new diagnostics", "demo.mp4": b"new video"}
        real_rename = MOD.rename_noreplace
        with self.staged_publication(artifacts) as (target, staging, fd):
            def observe_visibility(*args, **kwargs):
                self.assertEqual(os.listdir(target.fd), [staging.name])
                real_rename(*args, **kwargs)
                self.assertEqual(os.listdir(target.fd), ["capture"])
                self.assertEqual({path.name: path.read_bytes()
                                  for path in (self.destination / "capture").iterdir()}, artifacts)

            with patch.object(MOD, "rename_noreplace", side_effect=observe_visibility) as renamed, \
                    patch.object(MOD.os, "link", wraps=os.link) as linked, \
                    patch.object(MOD.os, "unlink", wraps=os.unlink) as unlinked:
                target.publish(fd, staging.name)
            renamed.assert_called_once()
            linked.assert_not_called()
            unlinked.assert_not_called()


if __name__ == "__main__":
    unittest.main()
