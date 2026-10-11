#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest
import json
import os
import subprocess
import sys
import tempfile

SPEC = importlib.util.spec_from_file_location("release_policy", Path(__file__).resolve().parents[1] / "scripts/release_policy.py")
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)


def notes(v, kind):
    return f"# {v}\nBump: {kind}\n\nFix immutable file protection during concurrent writes.\n"


class ReleasePolicyTests(unittest.TestCase):
    def test_bootstrap(self):
        MOD.validate("0.1.0", None, "0.1.0", notes("0.1.0", "initial"))

    def test_all_increment_types(self):
        for new, kind in [("1.2.4", "patch"), ("1.3.0", "minor"), ("2.0.0", "major")]:
            MOD.validate(new, "1.2.3", new, notes(new, kind))

    def test_reject_unchanged_lower_skipped_and_stale_versions(self):
        for old, new, kind in [("1.2.3", "1.2.3", "patch"), ("1.2.3", "1.2.2", "patch"), ("1.2.3", "1.2.5", "patch"), ("1.2.4", "1.2.4", "patch"), ("1.2.3", "2.0.0", "minor")]:
            with self.subTest(old=old, new=new, kind=kind), self.assertRaises(ValueError):
                MOD.validate(new, old, new, notes(new, kind))

    def test_reject_invalid_semver(self):
        for text in ["v1.2.3", "01.2.3", "1.2", "1.2.3-rc1", "1.2.3+build", "-1.2.3"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                MOD.version(text)

    def test_reject_lock_and_notes_mismatch(self):
        for lock, note in [("0.1.0", notes("0.1.1", "patch")), ("0.1.1", notes("0.2.0", "minor")), ("0.1.1", "# 0.1.1\nBump: patch\n\nTODO write the release notes here.")]:
            with self.subTest(lock=lock, note=note), self.assertRaises(ValueError):
                MOD.validate("0.1.1", "0.1.0", lock, note)

    def test_initial_is_not_reusable(self):
        with self.assertRaises(ValueError):
            MOD.validate("0.1.0", "0.1.0", "0.1.0", notes("0.1.0", "initial"))


class PushBaselineTests(unittest.TestCase):
    def test_rebased_multicommit_push_uses_before_not_last_parent(self):
        # The bump is in commit 1; commit 2 merely adds a fix. Rebase merges keep both.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(['git', '-C', directory, *args], text=True).strip()
            git('init', '-q')
            git('config', 'user.name', 'Test')
            git('config', 'user.email', 'test@example.invalid')
            def package(v):
                (root / 'Cargo.toml').write_text(f'[package]\nname = "git-on"\nversion = "{v}"\n')
                (root / 'Cargo.lock').write_text(f'[[package]]\nname = "git-on"\nversion = "{v}"\n')
            package('0.1.0')
            git('add', '.')
            git('commit', '-qm', 'old master')
            before = git('rev-parse', 'HEAD')
            package('0.1.1')
            (root / '.github').mkdir()
            (root / '.github/release-notes.md').write_text(notes('0.1.1', 'patch'))
            git('add', '.')
            git('commit', '-qm', 'version bump')
            (root / 'fix').write_text('bug fix')
            git('add', '.')
            git('commit', '-qm', 'fix after bump')
            event = root / 'event.json'
            event.write_text(json.dumps({'before': before, 'after': git('rev-parse', 'HEAD')}))
            script = str(Path(SPEC.origin).resolve())
            bad = subprocess.run([sys.executable, script, '--base', 'HEAD^1'], cwd=root, capture_output=True, text=True)
            self.assertNotEqual(bad.returncode, 0)
            good = subprocess.run([sys.executable, script, '--push-event', str(event)], cwd=root, capture_output=True, text=True)
            self.assertEqual(good.returncode, 0, good.stderr)

    def check_push_shape(self, shape):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(['git', '-C', directory, *args], text=True).strip()
            git('init', '-q', '-b', 'master')
            git('config', 'user.name', 'Test')
            git('config', 'user.email', 'test@example.invalid')
            def package(v):
                (root / 'Cargo.toml').write_text(f'[package]\nname = "git-on"\nversion = "{v}"\n')
                (root / 'Cargo.lock').write_text(f'[[package]]\nname = "git-on"\nversion = "{v}"\n')
            package('0.1.0')
            git('add', '.')
            git('commit', '-qm', 'old master')
            before = git('rev-parse', 'HEAD')
            if shape == 'merge':
                git('checkout', '-qb', 'feature')
            package('0.1.1')
            (root / '.github').mkdir()
            (root / '.github/release-notes.md').write_text(notes('0.1.1', 'patch'))
            git('add', '.')
            git('commit', '-qm', 'one release change')
            if shape == 'merge':
                git('checkout', '-q', 'master')
                git('merge', '--no-ff', '-qm', 'merge feature', 'feature')
                self.assertEqual(len(git('rev-list', '--parents', '-n', '1', 'HEAD').split()), 3)
            after = git('rev-parse', 'HEAD')
            event = root / 'event.json'
            script = str(Path(SPEC.origin).resolve())
            def run(old, new):
                event.write_text(json.dumps({'before': old, 'after': new}))
                return subprocess.run([sys.executable, script, '--push-event', str(event)], cwd=root, capture_output=True, text=True)
            result = run(before, after)
            self.assertEqual(result.returncode, 0, result.stderr)
            if shape == 'squash':
                for old, new in [('0' * 40, after), ('bad', after), (before, '0' * 40), (before, before), (before, 'f' * 40)]:
                    with self.subTest(before=old, after=new):
                        self.assertNotEqual(run(old, new).returncode, 0)
                # A real commit from unrelated history is not a valid pre-push master.
                git('checkout', '--orphan', 'unrelated')
                git('commit', '-qm', 'unrelated root')
                unrelated = git('rev-parse', 'HEAD')
                git('checkout', '-q', 'master')
                self.assertNotEqual(run(unrelated, after).returncode, 0)

    def test_squash_push_and_invalid_event_rejection(self):
        self.check_push_shape('squash')

    def test_two_parent_merge_push(self):
        self.check_push_shape('merge')

    def test_workflow_uses_push_event_for_both_validation_stages(self):
        workflow = (Path(SPEC.origin).parents[1] / '.github/workflows/ci.yml').read_text()
        self.assertNotIn('HEAD^1', workflow)
        self.assertEqual(workflow.count('--push-event "$GITHUB_EVENT_PATH"'), 2)


if __name__ == "__main__":
    unittest.main()
