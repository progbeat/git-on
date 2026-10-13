#!/usr/bin/env python3
"""Publication state-machine tests; all GitHub mutations are replaced with a fake."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('publish_release', Path(__file__).resolve().parents[1] / 'scripts/publish_release.py')
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)
SHA = 'a' * 40


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        cwd = os.getcwd()
        self.addCleanup(os.chdir, cwd)
        os.chdir(self.temp.name)
        Path('dist').mkdir()
        Path('dist/release.json').write_text(json.dumps({'version': '0.1.0', 'commit': SHA}))
        Path('dist/archive.tar.gz').write_bytes(b'archive')
        Path('release-body.md').write_text('Release notes\n')
        self.env = patch.dict(os.environ, {'GITHUB_REPOSITORY': 'owner/repo', 'GITHUB_SHA': SHA,
             'GITHUB_EVENT_NAME': 'push', 'GITHUB_REF': 'refs/heads/master', 'RELEASE_REQUIRED': 'true'})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.refs = []
        self.releases = []
        self.assets = {}
        self.starter = set()
        self.calls = []

    def fake(self, *args, raw=False, data=None):
        self.calls.append((args, data))
        if args[0] == 'api':
            path = next(x for x in args[1:] if x.startswith('repos/'))
            if '/git/matching-refs/' in path:
                return self.refs
            if path.endswith('/git/refs'):
                self.refs = [{'ref': data['ref'], 'object': {'type': 'commit', 'sha': data['sha']}}]
                return self.refs[0]
            if path.endswith('/releases?per_page=100'):
                return [self.releases]
            if path.endswith('/releases'):
                release = dict(data, id=1)
                self.releases = [release]
                return release
            if '/assets?' in path:
                return [{'name': n, 'id': i, 'state': 'starter' if n in self.starter else 'uploaded'} for i, n in enumerate(self.assets)]
            if '/releases/assets/' in path and '--method' in args and args[args.index('--method') + 1] == 'DELETE':
                name = list(self.assets)[int(path.rsplit('/', 1)[1])]
                del self.assets[name]
                self.starter.discard(name)
                return ''
            if path.endswith('/releases/1'):
                if data is not None:
                    self.releases[0].update(data)
                return self.releases[0]
        if args[:2] == ('release', 'upload'):
            path = Path(args[3])
            self.assets[path.name] = path.read_bytes()
            return ''
        if args[:2] == ('release', 'download'):
            name = args[args.index('--pattern') + 1]
            directory = args[args.index('--dir') + 1]
            Path(directory, name).write_bytes(self.assets[name])
            return ''
        raise AssertionError((args, data))

    def run_publish(self):
        with patch.object(MOD, 'gh', self.fake):
            MOD.publish()

    def test_create_and_retry_published_without_mutation(self):
        self.run_publish()
        self.assertFalse(self.releases[0]['draft'])
        self.calls.clear()
        self.run_publish()
        self.assertTrue(all(data is None for _, data in self.calls))
        self.assertFalse(any(args[:2] == ('release', 'upload') for args, _ in self.calls))

    def test_resume_draft(self):
        self.run_publish()
        self.releases[0]['draft'] = True
        del self.assets['archive.tar.gz']
        self.run_publish()
        self.assertIn('archive.tar.gz', self.assets)
        self.assertFalse(self.releases[0]['draft'])

    def test_resume_failed_upload_starter_asset(self):
        self.run_publish()
        self.releases[0]['draft'] = True
        self.assets['archive.tar.gz'] = b''
        self.starter.add('archive.tar.gz')
        self.run_publish()
        self.assertEqual(self.assets['archive.tar.gz'], b'archive')
        self.assertFalse(self.releases[0]['draft'])

    def test_published_starter_is_never_deleted(self):
        self.run_publish()
        self.starter.add('archive.tar.gz')
        self.calls.clear()
        with self.assertRaisesRegex(RuntimeError, 'published release'):
            self.run_publish()
        self.assertFalse(any('DELETE' in args for args, _ in self.calls))

    def test_reject_tag_collision(self):
        self.refs = [{'ref': 'refs/tags/v0.1.0', 'object': {'type': 'commit', 'sha': 'b' * 40}}]
        with self.assertRaisesRegex(RuntimeError, 'another object'):
            self.run_publish()

    def test_reject_source_mismatch(self):
        with patch.dict(os.environ, {'GITHUB_SHA': 'b' * 40}), self.assertRaisesRegex(RuntimeError, 'source SHA'):
            self.run_publish()
        self.assertFalse(self.calls)

    def test_reject_wrong_event(self):
        with patch.dict(os.environ, {'GITHUB_EVENT_NAME': 'pull_request'}), self.assertRaises(RuntimeError):
            self.run_publish()
        self.assertFalse(self.calls)

    def test_nonrelease_push_skips_old_version_without_api_calls(self):
        self.run_publish()
        self.calls.clear()
        # A docs push may carry the existing version but has another source SHA.
        # The skip must happen before inspecting/recreating its old artifacts.
        with patch.dict(os.environ, {'RELEASE_REQUIRED': 'false', 'GITHUB_SHA': 'b' * 40}):
            self.run_publish()
        self.assertFalse(self.calls)
        self.assertFalse(self.releases[0]['draft'])

    def test_missing_release_decision_cannot_publish(self):
        with patch.dict(os.environ):
            del os.environ['RELEASE_REQUIRED']
            with self.assertRaisesRegex(RuntimeError, 'release decision'):
                self.run_publish()
        self.assertFalse(self.calls)

    def test_reject_changed_asset(self):
        self.run_publish()
        self.assets['archive.tar.gz'] = b'changed'
        with self.assertRaisesRegex(RuntimeError, 'asset differs'):
            self.run_publish()

    def test_reject_changed_notes(self):
        self.run_publish()
        Path('release-body.md').write_text('Different notes\n')
        with self.assertRaisesRegex(RuntimeError, 'notes differ'):
            self.run_publish()

    def test_reject_missing_published_asset(self):
        self.run_publish()
        del self.assets['archive.tar.gz']
        with self.assertRaisesRegex(RuntimeError, 'missing an asset'):
            self.run_publish()


if __name__ == '__main__':
    unittest.main()
