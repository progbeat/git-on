#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest
import json
import os
import re
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
            (root / 'src').mkdir()
            (root / 'src/main.rs').write_text('fn main() {}\n')
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
            (root / 'src/main.rs').write_text('fn main() { println!("fixed"); }\n')
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
            (root / 'src').mkdir()
            (root / 'src/main.rs').write_text('fn main() {}\n')
            package('0.1.0')
            git('add', '.')
            git('commit', '-qm', 'old master')
            before = git('rev-parse', 'HEAD')
            if shape == 'merge':
                git('checkout', '-qb', 'feature')
            package('0.1.1')
            (root / 'src/main.rs').write_text('fn main() { println!("changed"); }\n')
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


class ChangeClassificationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.script = str(Path(SPEC.origin).resolve())
        self.git('init', '-q', '-b', 'master')
        self.git('config', 'user.name', 'Test')
        self.git('config', 'user.email', 'test@example.invalid')
        self.write('Cargo.toml', '[package]\nname = "git-on"\nversion = "1.1.0"\n'
                   '[dependencies]\nruntime = "1"\n[dev-dependencies]\ntest-only = "1"\n')
        self.write('Cargo.lock', '[[package]]\nname = "git-on"\nversion = "1.1.0"\n'
                   'dependencies = ["runtime", "test-only"]\n'
                   '[[package]]\nname = "runtime"\nversion = "1.0.0"\n'
                   'dependencies = ["transitive"]\n'
                   '[[package]]\nname = "transitive"\nversion = "1.0.0"\n'
                   '[[package]]\nname = "test-only"\nversion = "1.0.0"\n')
        self.write('src/main.rs', 'fn main() {}\n')
        self.write('.github/release-notes.md', notes('1.1.0', 'minor'))
        self.write('.github/workflows/ci.yml', 'jobs:\n  rust:\n    steps:\n      - run: cargo test\n'
                   '  package:\n    runs-on: ubuntu-22.04\n    steps:\n'
                   '      - run: cargo +1.90.0 build --release --locked\n'
                   '  release:\n    needs: [package]\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'released master')
        self.base = self.git('rev-parse', 'HEAD')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.root), *args], text=True).strip()

    def write(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def replace(self, name, old, new):
        self.write(name, (self.root / name).read_text().replace(old, new))

    def run_policy(self, release, success=True):
        self.git('add', '.')
        self.git('commit', '-qm', 'proposed change')
        output = self.root / 'policy-output'
        result = subprocess.run([sys.executable, self.script, '--base', self.base], cwd=self.root,
                                capture_output=True, text=True, env={**os.environ, 'GITHUB_OUTPUT': str(output)})
        if success:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f'release_required={str(release).lower()}\n', output.read_text())
            self.assertEqual((self.root / 'release-body.md').exists(), release)
        else:
            self.assertNotEqual(result.returncode, 0)
        return result

    def bump(self):
        self.replace('Cargo.toml', 'version = "1.1.0"', 'version = "1.2.0"')
        self.replace('Cargo.lock', 'version = "1.1.0"', 'version = "1.2.0"')
        self.write('.github/release-notes.md', notes('1.2.0', 'minor'))

    def test_docs_tests_examples_installer_and_gif_need_no_release(self):
        for name in ['README.md', 'docs/usage.md', 'tests/check.py', 'examples/main.rs', 'install.sh', 'assets/demo.gif']:
            self.write(name, 'non-binary change\n')
        # Stale body files from a previous invocation cannot authorize publication.
        self.write('release-body.md', 'old release body\n')
        self.run_policy(False)

    def test_source_requires_bump(self):
        self.write('src/main.rs', 'fn main() { println!("changed"); }\n')
        self.run_policy(True, success=False)

    def test_vendored_runtime_source_requires_a_release(self):
        self.write('vendor/fuser/src/reply.rs', 'runtime patch\n')
        self.run_policy(True, success=False)

    def test_vendored_docs_tests_and_standalone_examples_need_no_release(self):
        for name in ['README.git-on.md', 'LICENSE.md', 'tests/integration_tests.rs',
                     'examples/hello.rs']:
            self.write('vendor/fuser/' + name, 'non-binary change\n')
        self.run_policy(False)

    def test_vendored_manifests_and_build_inputs_require_a_release(self):
        for name in ['Cargo.toml', 'build.rs', '.cargo/config.toml']:
            with self.subTest(name=name):
                self.write('vendor/fuser/' + name, 'binary build change\n')
                self.run_policy(True, success=False)
                (self.root / 'vendor/fuser' / name).unlink()

    def test_native_distribution_matrix_changes_require_a_release(self):
        self.write('.github/workflows/ci.yml', 'jobs:\n  package-build:\n    runs-on: macos-14\n')
        self.run_policy(True, success=False)

    def test_mixed_source_and_docs_has_one_release(self):
        self.write('README.md', 'updated documentation\n')
        self.write('src/main.rs', 'fn main() { println!("changed"); }\n')
        self.bump()
        self.run_policy(True)

    def test_docs_cannot_republish_with_unnecessary_bump(self):
        self.write('README.md', 'documentation only\n')
        self.bump()
        self.run_policy(False, success=False)

    def test_nonrelease_changes_must_retain_release_notes_bytes(self):
        path = self.root / '.github/release-notes.md'
        original = path.read_bytes()
        self.git('config', 'core.autocrlf', 'false')
        for change in ['content', 'deletion', 'line-endings']:
            with self.subTest(change=change):
                if change == 'deletion':
                    path.unlink()
                else:
                    path.write_bytes(original + b'Additional unpublished metadata.\n'
                                     if change == 'content' else original.replace(b'\n', b'\r\n'))
                result = self.run_policy(False, success=False)
                self.assertIn('keep the current master release notes', result.stderr)

    def test_nonrelease_cannot_add_previously_absent_release_notes(self):
        self.git('rm', '.github/release-notes.md')
        self.git('commit', '-qm', 'baseline without release notes')
        self.base = self.git('rev-parse', 'HEAD')
        self.write('.github/release-notes.md', notes('1.1.0', 'minor'))
        result = self.run_policy(False, success=False)
        self.assertIn('keep the current master release notes', result.stderr)

    def test_nonrelease_can_retain_absent_release_notes(self):
        self.git('rm', '.github/release-notes.md')
        self.git('commit', '-qm', 'baseline without release notes')
        self.base = self.git('rev-parse', 'HEAD')
        self.write('README.md', 'documentation only\n')
        self.run_policy(False)

    def test_runtime_manifest_change_requires_bump(self):
        self.replace('Cargo.toml', 'runtime = "1"', 'runtime = "2"')
        self.run_policy(True, success=False)

    def test_transitive_runtime_lock_update_requires_bump(self):
        self.replace('Cargo.lock', 'name = "transitive"\nversion = "1.0.0"',
                     'name = "transitive"\nversion = "1.0.1"')
        self.run_policy(True, success=False)

    def test_dev_dependency_update_needs_no_release(self):
        self.replace('Cargo.toml', 'test-only = "1"', 'test-only = "2"')
        self.replace('Cargo.lock', 'name = "test-only"\nversion = "1.0.0"',
                     'name = "test-only"\nversion = "2.0.0"')
        self.run_policy(False)

    def mixed_version_baseline(self, declaration='[dependencies]\nshared = "1"\n'):
        self.write('Cargo.toml', '[package]\nname = "git-on"\nversion = "1.1.0"\n'
                   + declaration + '[dev-dependencies]\nshared-tests = { package = "shared", version = "2" }\n')
        self.write('Cargo.lock', '[[package]]\nname = "git-on"\nversion = "1.1.0"\n'
                   'dependencies = ["shared 1.0.0", "shared 2.0.0"]\n'
                   '[[package]]\nname = "shared"\nversion = "1.0.0"\n'
                   'dependencies = ["runtime-leaf"]\n'
                   '[[package]]\nname = "shared"\nversion = "2.0.0"\n'
                   'dependencies = ["test-leaf"]\n'
                   '[[package]]\nname = "runtime-leaf"\nversion = "1.0.0"\n'
                   '[[package]]\nname = "test-leaf"\nversion = "1.0.0"\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'runtime and dev copies of one crate')
        self.base = self.git('rev-parse', 'HEAD')

    def test_dev_only_copy_of_runtime_crate_needs_no_release(self):
        self.mixed_version_baseline()
        self.replace('Cargo.lock', 'shared 2.0.0', 'shared 2.0.1')
        self.replace('Cargo.lock', 'name = "shared"\nversion = "2.0.0"',
                     'name = "shared"\nversion = "2.0.1"')
        self.replace('Cargo.lock', 'name = "test-leaf"\nversion = "1.0.0"',
                     'name = "test-leaf"\nversion = "1.0.1"')
        self.run_policy(False)

    def test_renamed_build_and_target_dependencies_exclude_dev_copy(self):
        for declaration in [
            '[dependencies]\nrenamed = { package = "shared", version = "1" }\n',
            '[build-dependencies]\nshared = "1"\n',
            '[target.\'cfg(unix)\'.dependencies]\nshared = "1"\n',
            '[target.\'cfg(unix)\'.build-dependencies]\nshared = "1"\n',
        ]:
            with self.subTest(declaration=declaration):
                self.mixed_version_baseline(declaration)
                self.replace('Cargo.lock', 'shared 2.0.0', 'shared 2.0.1')
                self.replace('Cargo.lock', 'name = "shared"\nversion = "2.0.0"',
                             'name = "shared"\nversion = "2.0.1"')
                self.run_policy(False)

    def test_runtime_copy_and_transitive_changes_still_require_release(self):
        self.mixed_version_baseline()
        self.replace('Cargo.lock', 'shared 1.0.0', 'shared 1.0.1')
        self.replace('Cargo.lock', 'name = "shared"\nversion = "1.0.0"',
                     'name = "shared"\nversion = "1.0.1"')
        self.replace('Cargo.lock', 'name = "runtime-leaf"\nversion = "1.0.0"',
                     'name = "runtime-leaf"\nversion = "1.0.1"')
        self.run_policy(True, success=False)
        self.bump()
        self.run_policy(True)

    def test_overlapping_runtime_and_dev_version_edges_require_clarification(self):
        self.mixed_version_baseline('[dependencies]\nshared = ">=1, <3"\n')
        self.replace('Cargo.lock', 'shared 2.0.0', 'shared 2.0.1')
        self.replace('Cargo.lock', 'name = "shared"\nversion = "2.0.0"',
                     'name = "shared"\nversion = "2.0.1"')
        result = self.run_policy(False, success=False)
        self.assertIn('cannot unambiguously classify runtime/build dependency', result.stderr)
        self.assertNotIn('expected exactly one', result.stderr)

    def test_cargo_requirement_boundaries_select_locked_runtime_version(self):
        for requirement, selected, rejected in [
            ('1', '1.9.0', '2.0.0'), ('0.2', '0.2.9', '0.3.0'),
            ('0.0.3', '0.0.3', '0.0.4'), ('~1.2', '1.2.9', '1.3.0'),
            ('1.*', '1.9.0', '2.0.0'), ('1.2.*', '1.2.9', '1.3.0'),
            ('>=1.2, <1.5', '1.4.9', '1.5.0'), ('>1.2', '1.3.0', '1.2.9'),
            ('=1.2', '1.2.9', '1.3.0'), ('1', '1.1.0', '1.1.0-alpha'),
            ('1.0.0-alpha.2', '1.0.0-alpha.10', '1.0.1-alpha'),
            ('1.0.0-alpha', '1.2.0', '1.0.0-0'),
            ('=1.2.3+build', '1.2.3+different', '1.2.4'),
            ('=1.2, >=1.2.3-alpha', '1.2.3', '1.2.3-alpha'),
            ('^1.2, >=1.2.3-alpha', '1.2.3-alpha', '1.2.3-0'),
        ]:
            with self.subTest(requirement=requirement):
                self.assertTrue(MOD.cargo_version_matches(requirement, selected))
                self.assertFalse(MOD.cargo_version_matches(requirement, rejected))
        self.assertFalse(MOD.cargo_version_matches('>=1.2, =1.2.3-alpha', '1.2.3-alpha'))

    def test_release_profile_change_requires_bump(self):
        with (self.root / 'Cargo.toml').open('a') as stream:
            stream.write('\n[profile.release]\nlto = "fat"\n')
        self.run_policy(True, success=False)

    def test_distribution_build_change_requires_bump(self):
        self.replace('.github/workflows/ci.yml', 'cargo +1.90.0 build', 'cargo +1.91.0 build')
        self.run_policy(True, success=False)

    def test_inherited_workflow_rustflags_requires_bump(self):
        workflow = '.github/workflows/ci.yml'
        self.write(workflow, 'env:\n  RUSTFLAGS: -C target-cpu=native\n' + (self.root / workflow).read_text())
        self.run_policy(True, success=False)

    def test_inherited_workflow_run_defaults_requires_bump(self):
        workflow = '.github/workflows/ci.yml'
        self.write(workflow, 'defaults:\n  run:\n    shell: bash -e {0}\n' + (self.root / workflow).read_text())
        self.run_policy(True, success=False)

    def test_test_job_change_needs_no_release(self):
        self.replace('.github/workflows/ci.yml', 'cargo test', 'cargo test --locked')
        self.run_policy(False)

    def test_custom_build_script_change_requires_bump(self):
        self.write('build.rs', 'fn main() {}\n')
        self.run_policy(True, success=False)

    def test_source_rename_outside_src_requires_bump(self):
        (self.root / 'examples').mkdir()
        self.git('mv', 'src/main.rs', 'examples/main.rs')
        self.run_policy(True, success=False)

    def test_test_profile_change_needs_no_release(self):
        with (self.root / 'Cargo.toml').open('a') as stream:
            stream.write('\n[profile.test]\nopt-level = 1\n')
        self.run_policy(False)

    def test_target_runtime_dependency_requires_bump(self):
        with (self.root / 'Cargo.toml').open('a') as stream:
            stream.write('\n[target.\'cfg(unix)\'.dependencies]\nruntime = "1"\n')
        self.run_policy(True, success=False)

    def test_target_test_dependency_needs_no_release(self):
        with (self.root / 'Cargo.toml').open('a') as stream:
            stream.write('\n[target.\'cfg(unix)\'.dev-dependencies]\ntest-only = "1"\n')
        self.run_policy(False)

    def test_docs_only_master_push_emits_skip(self):
        self.write('README.md', 'documentation only\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'docs-only merge')
        event = self.root / 'event.json'
        event.write_text(json.dumps({'before': self.base, 'after': self.git('rev-parse', 'HEAD')}))
        output = self.root / 'policy-output'
        result = subprocess.run([sys.executable, self.script, '--push-event', str(event)], cwd=self.root,
                                capture_output=True, text=True, env={**os.environ, 'GITHUB_OUTPUT': str(output)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('release_required=false\n', output.read_text())
        self.assertFalse((self.root / 'release-body.md').exists())

    def test_required_checks_have_no_path_filters_or_release_condition(self):
        workflow = (Path(SPEC.origin).parents[1] / '.github/workflows/ci.yml').read_text()
        self.assertNotIn('paths:', workflow)
        self.assertNotIn('paths-ignore:', workflow)
        self.assertIn("needs.release-policy.outputs.release_required == 'true'", workflow)
        self.assertIn('release_required: ${{ steps.policy.outputs.release_required }}', workflow)
        # Rust, real FUSE, performance, policy and packaging stay unconditional.
        jobs = workflow.split('\njobs:\n', 1)[1]
        for name in ['rust', 'real-fuse', 'lazy-performance', 'release-policy', 'package']:
            job = jobs.split(f'  {name}:\n', 1)[1]
            job = re.split(r'\n  [\w-]+:\n', job, maxsplit=1)[0]
            self.assertNotIn("release_required == 'true'", job)


if __name__ == "__main__":
    unittest.main()
