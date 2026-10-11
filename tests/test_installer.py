#!/usr/bin/env python3
"""Offline installer regressions and real packaged-binary installer acceptance.

No GitHub request is made. Fixture transport is supplied through a fake gh on
PATH, not through a production insecure URL or checksum-bypass option.
"""
import argparse
import hashlib
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import signal
import time
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / 'install.sh'
SOURCE = SCRIPT.read_text().split("<<'GIT_ON_INSTALLER_PYTHON'\n", 1)[1].rsplit('\nGIT_ON_INSTALLER_PYTHON', 1)[0]
MOD = importlib.util.module_from_spec(importlib.util.spec_from_loader('installer', loader=None))
exec(compile(SOURCE, str(SCRIPT), 'exec'), MOD.__dict__)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.releases = self.root / 'releases'
        self.releases.mkdir()
        self.bin = self.root / 'home with spaces/.local/bin'
        self.bin.mkdir(parents=True)
        self.old = self.bin / 'git-on'
        self.old.write_bytes(b'old executable\n')
        self.fakebin = self.root / 'tools'
        self.fakebin.mkdir()
        # Exercise the actual shell entrypoint, parser, embedded Python and gh arguments.
        helper = self.fakebin / 'gh'
        helper.write_text('''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
args = sys.argv[1:]
root = pathlib.Path(os.environ['INSTALLER_FIXTURES'])
with (root / 'requests.jsonl').open('a') as output:
    output.write(json.dumps(args) + '\\n')
if args[:3] == ['api', '--hostname', 'github.com']:
    endpoint = args[3]
    tag = (root / 'latest').read_text() if endpoint.endswith('/latest') else endpoint.rsplit('/', 1)[1]
    sys.stdout.buffer.write((root / tag / 'api.json').read_bytes())
elif args[:2] == ['release', 'download']:
    assert args[3:5] == ['--repo', 'https://github.com/progbeat/git-on']
    name = args[args.index('--pattern') + 1]
    target = pathlib.Path(args[args.index('--dir') + 1]) / name
    if (root / 'fail-download').exists():
        target.write_bytes(b'partial')
        sys.exit(1)
    shutil.copyfile(root / args[2] / name, target)
else:
    sys.exit(2)
''')
        helper.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.root / 'home with spaces'),
                        PATH=str(self.fakebin) + os.pathsep + os.environ['PATH'],
                        INSTALLER_FIXTURES=str(self.releases))
        self.release('0.1.0')

    def release(self, version, members=None, distribution=None):
        tag = 'v' + version
        directory = self.releases / tag
        directory.mkdir(exist_ok=True)
        archive = 'git-on-' + tag + '-' + MOD.TARGET + '.tar.gz'
        root = archive[:-7]
        if distribution:
            for name in (archive, 'release.json', 'SHA256SUMS'):
                shutil.copyfile(distribution / name, directory / name)
        else:
            if members is None:
                members = [(root, 'dir', b''),
                           (root + '/git-on', 'file', ('#!/bin/sh\nprintf "git-on ' + version + '\\n"\n').encode()),
                           (root + '/README.md', 'file', b'fixture documentation')]
            with tarfile.open(directory / archive, 'w:gz') as package:
                for name, kind, content in members:
                    member = tarfile.TarInfo(name)
                    member.mode = 0o755 if name.endswith('/git-on') else 0o644
                    if kind == 'dir':
                        member.type = tarfile.DIRTYPE
                    elif kind in ('symlink', 'hardlink'):
                        member.type = tarfile.SYMTYPE if kind == 'symlink' else tarfile.LNKTYPE
                        member.linkname = str(content)
                    else:
                        member.size = len(content)
                    package.addfile(member, io.BytesIO(content) if kind == 'file' else None)
            (directory / 'release.json').write_text(json.dumps({'version': version, 'commit': 'a' * 40,
                'target': MOD.TARGET, 'baseline': 'Ubuntu 22.04 / glibc 2.35', 'rust': '1.90.0'}))
            self.checksums(directory, archive)
        self.api(directory, tag, archive)
        (self.releases / 'latest').write_text(tag)
        return directory, archive

    def checksums(self, directory, archive):
        (directory / 'SHA256SUMS').write_text(''.join(hashlib.sha256((directory / name).read_bytes()).hexdigest() + '  ' + name + '\n'
                                                  for name in (archive, 'release.json')))

    def api(self, directory, tag, archive):
        assets = [{'name': name, 'state': 'uploaded', 'size': (directory / name).stat().st_size,
                   'browser_download_url': 'https://github.com/progbeat/git-on/releases/download/' + tag + '/' + name}
                  for name in (archive, 'SHA256SUMS', 'release.json')]
        data = {'tag_name': tag, 'draft': False, 'prerelease': False, 'assets': assets}
        (directory / 'api.json').write_text(json.dumps(data))
        return data

    def run_script(self, *args, success=True, pipe=False):
        command = ['sh', '-s', '--'] if pipe else ['sh', str(SCRIPT)]
        result = subprocess.run(command + ['--github-cli', *args],
                                input=SCRIPT.read_text() if pipe else None,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env=self.env, timeout=30)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(self.old.read_bytes(), b'old executable\n')
        self.assertEqual(list(self.bin.glob('.git-on-install-*')), [])
        return result

    def test_truncated_pipe_payload_never_starts_installation(self):
        script = SCRIPT.read_text()
        suffix = script.index('if __name__ == "__main__":')
        # Each line boundary after main() may still be a syntactically valid
        # Python file. The shell must wait for the whole enclosing function.
        boundaries = [index + 1 for index in range(suffix, len(script))
                      if script[index] == '\n' and index + 1 < script.rfind('GIT_ON_INSTALLER_PYTHON') + len('GIT_ON_INSTALLER_PYTHON')]
        for boundary in boundaries:
            with self.subTest(boundary=boundary):
                self.old.write_bytes(b'old executable\n')
                (self.releases / 'requests.jsonl').unlink(missing_ok=True)
                result = subprocess.run(['sh', '-s', '--', '--github-cli'], input=script[:boundary],
                                        env=self.env, capture_output=True, text=True, timeout=30)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual(self.old.read_bytes(), b'old executable\n')
                self.assertFalse((self.releases / 'requests.jsonl').exists())

    def test_final_invocation_truncation_cannot_drop_arguments(self):
        script = SCRIPT.read_text()
        marker = self.root / 'python-argv'
        python = self.fakebin / 'python3'
        python.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$INSTALLER_PROBE\"\n")
        python.chmod(0o755)
        env = dict(self.env, INSTALLER_PROBE=str(marker))
        start = script.rfind('git_on_install "$@"')
        # All prefixes shorter than the complete guarded invocation are inert.
        end = script.rstrip().rfind('}') if script.rstrip().endswith('}') else len(script.rstrip())
        for boundary in range(start, end + 1):
            with self.subTest(boundary=boundary):
                marker.unlink(missing_ok=True)
                subprocess.run(['sh', '-s', '--', '--github-cli', '--version', '0.1.0', '--bin-dir', '/intended/path'],
                               input=script[:boundary], env=env, capture_output=True, text=True, timeout=10)
                self.assertFalse(marker.exists(), marker.read_text() if marker.exists() else '')
        result = subprocess.run(['sh', '-s', '--', '--github-cli', '--version', '0.1.0', '--bin-dir', '/intended/path'],
                                input=script.rstrip(), env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(marker.read_text().splitlines(), ['-', '--github-cli', '--version', '0.1.0', '--bin-dir', '/intended/path'])

    def test_latest_install_repeat_and_update_through_pipe(self):
        result = self.run_script(pipe=True)
        self.assertIn('v0.1.0', result.stdout)
        first = self.old.read_bytes()
        self.assertEqual(stat.S_IMODE(self.old.stat().st_mode), 0o755)
        self.run_script()
        self.assertEqual(self.old.read_bytes(), first)
        self.release('0.2.0')
        self.run_script()
        self.assertIn(b'git-on 0.2.0', self.old.read_bytes())
        requests = [json.loads(line) for line in (self.releases / 'requests.jsonl').read_text().splitlines()]
        self.assertEqual(sum(args[0] == 'api' for args in requests), 3)
        self.assertTrue(all(args[-1].endswith('/latest') for args in requests if args[0] == 'api'))

    def test_specific_version_and_custom_destination(self):
        self.release('0.2.0')
        destination = self.root / 'custom bin'
        self.run_script('--version', 'v0.1.0', '--bin-dir', str(destination))
        self.assertIn(b'git-on 0.1.0', (destination / 'git-on').read_bytes())
        self.assertEqual(self.old.read_bytes(), b'old executable\n')
        requests = [json.loads(line) for line in (self.releases / 'requests.jsonl').read_text().splitlines()]
        self.assertEqual(requests[0][-1], 'repos/progbeat/git-on/releases/tags/v0.1.0')

    def test_partial_download_preserves_old_binary(self):
        (self.releases / 'fail-download').touch()
        self.assertIn('GitHub CLI request failed', self.run_script(success=False).stderr)

    def test_checksum_failure_preserves_old_binary(self):
        directory = self.releases / 'v0.1.0'
        path = next(directory.glob('*.tar.gz'))
        data = bytearray(path.read_bytes())
        data[-1] ^= 1
        path.write_bytes(data)
        self.assertIn('Checksum verification failed', self.run_script(success=False).stderr)

    def test_wrong_binary_version_preserves_old_binary(self):
        root = 'git-on-v0.1.0-' + MOD.TARGET
        self.release('0.1.0', [(root, 'dir', b''), (root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.9.0\n'),
                               (root + '/README.md', 'file', b'readme')])
        self.assertIn('version check', self.run_script(success=False).stderr)

    def test_missing_runtime_library_probe_preserves_old_binary(self):
        root = 'git-on-v0.1.0-' + MOD.TARGET
        self.release('0.1.0', [(root, 'dir', b''), (root + '/git-on', 'file',
                              b'#!/bin/sh\necho "libz.so.1: cannot open shared object file" >&2\nexit 127\n'),
                               (root + '/README.md', 'file', b'readme')])
        result = self.run_script(success=False)
        self.assertIn('libz.so.1', result.stderr)
        self.assertIn('libgcc_s.so.1', result.stderr)

    def test_unsafe_archive_members_preserve_old_binary(self):
        root = 'git-on-v0.1.0-' + MOD.TARGET
        normal = [(root, 'dir', b''), (root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.1.0\n'),
                  (root + '/README.md', 'file', b'readme')]
        cases = [normal + [('../escape', 'file', b'no')],
                 normal[:1] + [(root + '/git-on', 'symlink', '/tmp/escape')] + normal[2:],
                 normal[:1] + [(root + '/git-on', 'hardlink', '/tmp/escape')] + normal[2:],
                 normal + [normal[1]],
                 normal[:2] + [('/absolute', 'file', b'no')]]
        for members in cases:
            with self.subTest(members=members):
                self.release('0.1.0', members)
                self.run_script(success=False)
                self.assertFalse((self.root / 'escape').exists())

    def test_license_member_and_original_release_layout(self):
        root = 'git-on-v0.1.0-' + MOD.TARGET
        original = [(root, 'dir', b''),
                    (root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.1.0\n'),
                    (root + '/README.md', 'file', b'readme')]
        for extra in ([], [(root + '/LICENSE', 'file', (ROOT / 'LICENSE').read_bytes())]):
            with self.subTest(licensed=bool(extra)):
                directory, archive = self.release('0.1.0', original + extra)
                MOD.verify(directory, 'v0.1.0', archive)
                expanded, (offset, size) = MOD.unpack_binary(directory, archive)
                with expanded.open('rb') as source:
                    source.seek(offset)
                    self.assertEqual(source.read(size), original[1][2])

    def test_license_member_cannot_bypass_archive_checks(self):
        root = 'git-on-v0.1.0-' + MOD.TARGET
        original = [(root, 'dir', b''), (root + '/git-on', 'file', b'#!/bin/sh\n'),
                    (root + '/README.md', 'file', b'readme')]
        license_member = (root + '/LICENSE', 'file', b'license')
        cases = [[license_member, license_member],
                 [(root + '/LICENSE', 'symlink', '/tmp/license')],
                 [(root + '/LICENSE', 'hardlink', root + '/README.md')],
                 [(root + '/LICENSE', 'file', bytes(MOD.MAX_METADATA + 1))]]
        for extra in cases:
            with self.subTest(extra=extra[0][:2], duplicate=len(extra) > 1):
                directory, archive = self.release('0.1.0', original + extra)
                with self.assertRaises(RuntimeError):
                    MOD.unpack_binary(directory, archive)
                self.assertEqual(self.old.read_bytes(), b'old executable\n')

    def test_reject_symlink_destination_and_parent(self):
        outside = self.root / 'outside'
        outside.write_bytes(b'keep')
        self.old.unlink()
        self.old.symlink_to(outside)
        result = subprocess.run(['sh', str(SCRIPT), '--github-cli'], env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('symlink', result.stderr)
        self.assertEqual(outside.read_bytes(), b'keep')
        self.assertTrue(self.old.is_symlink())
        link = self.root / 'linked'
        link.symlink_to(self.bin, target_is_directory=True)
        result = subprocess.run(['sh', str(SCRIPT), '--bin-dir', str(link)], env=self.env, capture_output=True, text=True)
        self.assertIn('symlink', result.stderr)

    def test_invalid_versions_fail_before_download(self):
        for value in ('../x', '1.2', '01.2.3', '1.2.3-rc.1', 'latest', 'v1.2.3;touch /tmp/x'):
            with self.subTest(value=value):
                self.assertIn('stable version', self.run_script('--version', value, success=False).stderr)
        self.assertFalse((self.releases / 'requests.jsonl').exists())

    def test_reject_unsafe_release_selection(self):
        directory = self.releases / 'v0.1.0'
        original = json.loads((directory / 'api.json').read_text())
        cases = []
        for field, value in [('draft', True), ('prerelease', True), ('tag_name', 'v0.1.0-rc.1')]:
            data = json.loads(json.dumps(original))
            data[field] = value
            cases.append(data)
        data = json.loads(json.dumps(original))
        data['assets'][0]['browser_download_url'] = 'https://example.com/evil'
        cases.append(data)
        data = json.loads(json.dumps(original))
        data['assets'].append(data['assets'][0])
        cases.append(data)
        data = json.loads(json.dumps(original))
        data['assets'][0]['name'] = '../evil'
        cases.append(data)
        for data in cases:
            with self.subTest(data=data):
                (directory / 'api.json').write_text(json.dumps(data))
                self.run_script(success=False)

    def test_checksum_manifest_and_release_metadata_validation(self):
        directory, archive = self.release('0.1.0')
        original = (directory / 'SHA256SUMS').read_text()
        for text in (original + original.splitlines()[0] + '\n', original.replace('release.json', '../release.json'),
                     original.splitlines()[0] + '\n', original + 'invalid\n'):
            (directory / 'SHA256SUMS').write_text(text)
            self.api(directory, 'v0.1.0', archive)
            self.assertIn('SHA256SUMS', self.run_script(success=False).stderr)
        (directory / 'release.json').write_text(json.dumps({'version': '0.2.0'}))
        self.checksums(directory, archive)
        self.api(directory, 'v0.1.0', archive)
        self.assertIn('metadata does not match', self.run_script(success=False).stderr)

    def test_platform_and_libc_preflight(self):
        for system, architecture, libc in [('Darwin', 'x86_64', 'glibc 2.35'),
            ('Linux', 'aarch64', 'glibc 2.35'), ('Linux', 'x86_64', 'glibc 2.34'),
            ('Linux', 'x86_64', ''), ('Linux', 'x86_64', 'musl 1.2')]:
            with self.subTest(system=system, architecture=architecture, libc=libc), \
                    patch.object(MOD.platform, 'system', return_value=system), \
                    patch.object(MOD.platform, 'machine', return_value=architecture), \
                    patch.object(MOD.os, 'confstr', return_value=libc), self.assertRaises(RuntimeError):
                MOD.supported_host()
        with patch.object(MOD.os, 'geteuid', return_value=0), self.assertRaisesRegex(RuntimeError, 'without sudo'):
            MOD.supported_host()

    def test_https_transport_limits_and_truncation(self):
        class Response(io.BytesIO):
            headers = {'Content-Length': '10'}
        response = Response(b'short')
        opener = unittest.mock.Mock()
        opener.open.return_value = response
        with patch.object(MOD.urllib.request, 'build_opener', return_value=opener), self.assertRaisesRegex(RuntimeError, 'truncated'):
            MOD.download('https://example.com/file', self.root / 'download', 100)
        opener.open.return_value = Response(b'short')
        with patch.object(MOD.urllib.request, 'build_opener', return_value=opener), self.assertRaisesRegex(RuntimeError, 'oversized'):
            MOD.download('https://example.com/file', self.root / 'download', 5)
        request = MOD.urllib.request.Request('https://example.com')
        with self.assertRaisesRegex(RuntimeError, 'non-HTTPS'):
            MOD.HTTPSOnly().redirect_request(request, None, 302, '', {}, 'http://example.com/file')

    def test_replace_failure_preserves_old_binary(self):
        directory, archive = self.release('0.1.0')
        with patch.object(MOD.os, 'replace', side_effect=OSError('replacement failed')), self.assertRaises(OSError):
            MOD.install(directory, archive, 'v0.1.0', self.bin)
        self.assertEqual(self.old.read_bytes(), b'old executable\n')
        self.assertFalse(list(self.bin.glob('.git-on-install-*')))

    def test_hidden_extended_headers_and_unexpected_types_are_rejected(self):
        directory, archive = self.release('0.1.0')
        ordinary_tar = gzip.decompress((directory / archive).read_bytes())
        for kind, size in ((tarfile.XHDTYPE, 2 * 1024 * 1024),
                           (tarfile.GNUTYPE_LONGNAME, 2 * 1024 * 1024),
                           (tarfile.GNUTYPE_SPARSE, 0), (tarfile.CONTTYPE, 0)):
            with self.subTest(kind=kind):
                header = tarfile.TarInfo('ignored extended metadata')
                header.type, header.size = kind, size
                payload = header.tobuf(format=tarfile.GNU_FORMAT) + bytes(size)
                (directory / archive).write_bytes(gzip.compress(payload + ordinary_tar))
                self.checksums(directory, archive)
                self.api(directory, 'v0.1.0', archive)
                self.assertIn('unsafe paths', self.run_script(success=False).stderr)
        root = archive[:-7]
        for kind in (tarfile.GNUTYPE_SPARSE, tarfile.CONTTYPE):
            binary_header = tarfile.TarInfo(root + '/git-on')
            binary_header.type = kind
            binary_header.size = 32
            # Replace the real binary header while retaining its data and names.
            raw = bytearray(ordinary_tar)
            raw[512:1024] = binary_header.tobuf(format=tarfile.GNU_FORMAT)
            (directory / archive).write_bytes(gzip.compress(raw))
            self.checksums(directory, archive)
            self.api(directory, 'v0.1.0', archive)
            self.assertIn('unexpected member type', self.run_script(success=False).stderr)

    def test_expanded_archive_and_readme_limits(self):
        directory, archive = self.release('0.1.0')
        with patch.object(MOD, 'MAX_ARCHIVE', 100), patch.object(MOD, 'MAX_METADATA', 100):
            (directory / archive).write_bytes(gzip.compress(bytes(2 * 1024 * 1024)))
            with self.assertRaisesRegex(RuntimeError, 'Expanded archive'):
                MOD.unpack_binary(directory, archive)
        root = archive[:-7]
        self.release('0.1.0', [(root, 'dir', b''), (root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.1.0\n'),
                               (root + '/README.md', 'file', bytes(MOD.MAX_METADATA + 1))])
        self.assertIn('size limit', self.run_script(success=False).stderr)

    def test_anonymous_latest_and_pinned_install_use_one_release(self):
        directory, archive = self.release('0.1.0')
        def fake_download(url, destination, limit):
            if url.startswith('https://api.github.com/'):
                shutil.copyfile(directory / 'api.json', destination)
            else:
                self.assertTrue(url.startswith('https://github.com/progbeat/git-on/releases/download/v0.1.0/'))
                shutil.copyfile(directory / url.rsplit('/', 1)[1], destination)
        for arguments, endpoint in (([], 'latest'), (['--version', '0.1.0'], 'tags/v0.1.0')):
            with patch.object(MOD, 'download', side_effect=fake_download) as download, \
                    patch.object(MOD, 'gh', side_effect=AssertionError('Unexpected authentication')), \
                    patch('sys.stdout', new_callable=io.StringIO):
                MOD.main(['--bin-dir', str(self.bin), *arguments])
            self.assertEqual(download.call_count, 4)
            self.assertEqual(download.call_args_list[0].args[0], 'https://api.github.com/repos/progbeat/git-on/releases/' + endpoint)
            self.assertIn(b'git-on 0.1.0', self.old.read_bytes())

    def test_bad_metadata_shapes_fail_cleanly(self):
        directory = self.releases / 'v0.1.0'
        original = json.loads((directory / 'api.json').read_text())
        bad_names = []
        for name in ([], {}):
            data = json.loads(json.dumps(original))
            data['assets'][0]['name'] = name
            bad_names.append(data)
        for data in ([], None, dict(original, assets=[None, None, None]), *bad_names):
            (directory / 'api.json').write_text(json.dumps(data))
            result = self.run_script(success=False)
            self.assertNotIn('Traceback', result.stderr)
        directory, archive = self.release('0.1.0')
        for data in ([], None, {'version': '0.1.0', 'target': MOD.TARGET,
                              'baseline': 'Ubuntu 22.04 / glibc 2.35', 'commit': None}):
            (directory / 'release.json').write_text(json.dumps(data))
            self.checksums(directory, archive)
            self.api(directory, 'v0.1.0', archive)
            result = self.run_script(success=False)
            self.assertNotIn('Traceback', result.stderr)

    def test_python_version_relative_directory_and_shared_directory(self):
        with patch.object(MOD.sys, 'version_info', (3, 7)), self.assertRaisesRegex(RuntimeError, 'Python 3.8'):
            MOD.supported_host()
        self.assertIn('absolute path', self.run_script('--bin-dir', 'relative', success=False).stderr)
        self.bin.chmod(0o777)
        self.assertIn('not writable by other users', self.run_script(success=False).stderr)

    def test_version_timeout_cleans_staging_and_preserves_old_binary(self):
        directory, archive = self.release('0.1.0')
        with patch.object(MOD.subprocess, 'run', side_effect=subprocess.TimeoutExpired('git-on', 15)), \
                self.assertRaises(subprocess.TimeoutExpired):
            MOD.install(directory, archive, 'v0.1.0', self.bin)
        self.assertEqual(self.old.read_bytes(), b'old executable\n')
        self.assertFalse(list(self.bin.glob('.git-on-install-*')))

    def test_interruption_cleans_staging_and_preserves_old_binary(self):
        root = 'git-on-v0.1.0-' + MOD.TARGET
        self.release('0.1.0', [(root, 'dir', b''), (root + '/git-on', 'file', b'#!/bin/sh\nexec sleep 60\n'),
                               (root + '/README.md', 'file', b'readme')])
        process = subprocess.Popen(['sh', str(SCRIPT), '--github-cli'], env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 10
            while not list(self.bin.glob('.git-on-install-*')) and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(list(self.bin.glob('.git-on-install-*')))
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=10)
            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertIn('interrupted', stderr)
            self.assertEqual(self.old.read_bytes(), b'old executable\n')
            self.assertFalse(list(self.bin.glob('.git-on-install-*')))
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()


def packaged_acceptance(distribution, destination):
    case = InstallerTests()
    case.setUp()
    try:
        version = json.loads((distribution / 'release.json').read_text())['version']
        case.release(version, distribution=distribution)
        archive = distribution / ('git-on-v' + version + '-' + MOD.TARGET + '.tar.gz')
        with tarfile.open(archive, 'r:gz') as package:
            license_file = package.extractfile(archive.name[:-7] + '/LICENSE')
            if license_file is None or license_file.read() != (ROOT / 'LICENSE').read_bytes():
                raise RuntimeError('Packaged license differs from the source license')
        case.run_script('--bin-dir', str(destination))
        case.run_script('--bin-dir', str(destination))
        binary = destination / 'git-on'
        result = subprocess.run([str(binary), '--version'], capture_output=True, text=True, check=True)
        if result.stdout.strip() != 'git-on ' + version:
            raise RuntimeError('Installed packaged version differs')
        print('Verified the actual distribution through install.sh, including repeat installation: ' + str(binary))
    finally:
        case.doCleanups()


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--dist', type=Path)
    parser.add_argument('--install-dir', type=Path)
    args, remaining = parser.parse_known_args()
    if args.dist:
        if args.install_dir is None or not args.install_dir.is_absolute():
            parser.error('--dist requires an absolute --install-dir')
        packaged_acceptance(args.dist.resolve(), args.install_dir)
    else:
        unittest.main(argv=[sys.argv[0], *remaining])
