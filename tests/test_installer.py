#!/usr/bin/env python3
"""Black-box shell installer tests; transport fixtures are shell programs.

The installer runs with an explicit tool PATH containing neither Python nor jq.
--dist additionally installs the real packaged executable (latest and pinned).
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / 'install.sh'
TARGET = 'x86_64-unknown-linux-gnu'
TOOLS = 'awk cat chmod cp getconf gzip id mkdir mktemp mv rm sha256sum stat tar timeout uname sleep'.split()
CURL_FIXTURE = r'''#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$INSTALLER_FIXTURES/requests"
out= url=
while [ "$#" -gt 0 ]; do
    case $1 in
        -o) out=$2; shift 2;;
        -w|--proto|--proto-redir|--connect-timeout|--max-time|--max-filesize) shift 2;;
        --tlsv1.2|-fsSL) shift;;
        https://*) url=$1; shift;;
        *) exit 90;;
    esac
done
base=https://github.com/progbeat/git-on/releases
case $url in
    "$base/latest")
        if [ -f "$INSTALLER_FIXTURES/latest-url" ]; then cat "$INSTALLER_FIXTURES/latest-url";
        else printf '%s/tag/%s' "$base" "$(cat "$INSTALLER_FIXTURES/latest")"; fi
        if [ -f "$INSTALLER_FIXTURES/move-latest" ]; then cp "$INSTALLER_FIXTURES/move-latest" "$INSTALLER_FIXTURES/latest"; fi;;
    "$base/download/"*)
        path=${url#"$base/download/"}
        if [ -f "$INSTALLER_FIXTURES/fail-download" ]; then printf partial > "$out"; exit 22; fi
        cp "$INSTALLER_FIXTURES/$path" "$out";;
    *) exit 91;;
esac
'''
GH_FIXTURE = r'''#!/bin/sh
set -eu
printf 'gh %s\n' "$*" >> "$INSTALLER_FIXTURES/requests"
case $1 in
    api)
        endpoint=$4
        case $endpoint in */latest) tag=$(cat "$INSTALLER_FIXTURES/latest");; *) tag=${endpoint##*/};; esac
        if [ -f "$INSTALLER_FIXTURES/gh-tag" ]; then cat "$INSTALLER_FIXTURES/gh-tag"; else printf '%s\n' "$tag"; fi;;
    release)
        tag=$3
        shift 3
        name= dest=
        while [ "$#" -gt 0 ]; do
            case $1 in --repo) shift 2;; --pattern) name=$2; shift 2;; --dir) dest=$2; shift 2;; *) exit 92;; esac
        done
        if [ -f "$INSTALLER_FIXTURES/fail-download" ]; then printf partial > "$dest/$name"; exit 1; fi
        cp "$INSTALLER_FIXTURES/$tag/$name" "$dest/$name";;
    *) exit 93;;
esac
'''


def tools_directory(path):
    path.mkdir()
    for tool in TOOLS:
        source = shutil.which(tool)
        if source is None:
            raise RuntimeError('Required test tool is missing: ' + tool)
        (path / tool).symlink_to(source)
    for name, source in [('curl', CURL_FIXTURE), ('gh', GH_FIXTURE)]:
        (path / name).write_text(source)
        (path / name).chmod(0o755)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='git-on-installer-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.releases = self.root / 'releases'
        self.releases.mkdir()
        self.bin = self.root / 'home with spaces/.local/bin'
        self.bin.mkdir(parents=True)
        self.bin.chmod(0o755)
        self.old = self.bin / 'git-on'
        self.old.write_bytes(b'old executable\n')
        self.fakebin = self.root / 'tools'
        tools_directory(self.fakebin)
        self.env = dict(HOME=str(self.root / 'home with spaces'), PATH=str(self.fakebin),
                        LC_ALL='C', INSTALLER_FIXTURES=str(self.releases))
        self.release('0.1.0')

    def release(self, version, members=None, distribution=None, binary=None):
        directory = self.releases / ('v' + version)
        directory.mkdir(exist_ok=True)
        name = 'git-on-v' + version + '-' + TARGET
        archive = name + '.tar.gz'
        if distribution:
            for filename in (archive, 'release.json', 'SHA256SUMS'):
                shutil.copyfile(distribution / filename, directory / filename)
        else:
            if binary is None:
                binary = ('#!/bin/sh\nprintf "git-on ' + version + '\\n"\n').encode()
            if members is None:
                members = [(name, 'dir', b''), (name + '/git-on', 'file', binary),
                           (name + '/README.md', 'file', b'documentation')]
                if version != '0.1.0':
                    members.append((name + '/LICENSE', 'file', (ROOT / 'LICENSE').read_bytes()))
            with tarfile.open(directory / archive, 'w:gz', format=tarfile.USTAR_FORMAT) as package:
                for filename, kind, content in members:
                    entry = tarfile.TarInfo(filename)
                    entry.mode = 0o755 if filename.endswith('/git-on') else 0o644
                    if kind == 'dir':
                        entry.type = tarfile.DIRTYPE
                    elif kind in ('symlink', 'hardlink'):
                        entry.type = tarfile.SYMTYPE if kind == 'symlink' else tarfile.LNKTYPE
                        entry.linkname = content
                    else:
                        entry.size = len(content)
                    package.addfile(entry, io.BytesIO(content) if kind == 'file' else None)
            (directory / 'release.json').write_text(json.dumps({'version': version, 'commit': 'a'*40,
                'target': TARGET, 'baseline': 'Ubuntu 22.04 / glibc 2.35', 'rust': '1.90.0'}))
            self.checksums(directory, archive)
        (self.releases / 'latest').write_text('v' + version)
        return directory, archive

    def checksums(self, directory, archive):
        (directory / 'SHA256SUMS').write_text(''.join(
            hashlib.sha256((directory / filename).read_bytes()).hexdigest() + '  ' + filename + '\n'
            for filename in (archive, 'release.json')))

    def run_script(self, *args, success=True, pipe=False, github_cli=False):
        command = ['/bin/sh', '-s', '--'] if pipe else ['/bin/sh', str(SCRIPT)]
        if github_cli:
            command.append('--github-cli')
        result = subprocess.run(command + list(args), input=SCRIPT.read_text() if pipe else None,
                                env=self.env, capture_output=True, text=True, timeout=30)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(self.old.read_bytes(), b'old executable\n')
        self.assertEqual(list(self.bin.glob('.git-on-install.*')), [])
        return result

    def requests(self):
        path = self.releases / 'requests'
        return path.read_text().splitlines() if path.exists() else []

    def test_latest_repeat_upgrade_without_python_or_jq(self):
        for tool in ('python', 'python3', 'jq'):
            self.assertIsNone(shutil.which(tool, path=self.env['PATH']))
        self.run_script(pipe=True)
        self.assertEqual(stat.S_IMODE(self.old.stat().st_mode), 0o755)
        first = self.old.read_bytes()
        self.run_script()
        self.assertEqual(self.old.read_bytes(), first)
        self.release('0.2.0')
        self.run_script()
        self.assertIn(b'git-on 0.2.0', self.old.read_bytes())
        self.assertEqual(sum('/releases/latest' in r for r in self.requests()), 3)

    def test_latest_is_resolved_once_before_any_asset_request(self):
        self.release('0.2.0')
        (self.releases / 'latest').write_text('v0.1.0')
        (self.releases / 'move-latest').write_text('v0.2.0')
        self.run_script()
        self.assertIn(b'git-on 0.1.0', self.old.read_bytes())
        downloads = [r for r in self.requests() if '/download/' in r]
        self.assertEqual(len(downloads), 2)
        self.assertTrue(all('/download/v0.1.0/' in r for r in downloads))
        self.assertTrue(all("--proto =https --proto-redir =https --tlsv1.2" in r for r in self.requests()))

    def test_pinned_version_custom_directory_and_legacy_layout(self):
        self.release('0.2.0')
        for version in ('0.1.0', 'v0.1.0'):
            destination = self.root / ('custom bin ' + version)
            self.run_script('--version', version, '--bin-dir', str(destination))
            self.assertIn(b'git-on 0.1.0', (destination / 'git-on').read_bytes())
        self.assertFalse(any('/releases/latest' in r for r in self.requests()))

    def test_gh_latest_and_pinned_need_no_external_jq(self):
        self.run_script(github_cli=True)
        self.run_script('--version', 'v0.1.0', github_cli=True)
        self.assertTrue(all(r.startswith('gh ') for r in self.requests()))
        self.assertTrue(any('select(.draft == false and .prerelease == false)' in r for r in self.requests()))

    def test_invalid_versions_and_arguments_fail_before_download(self):
        for version in ('', 'v', '../x', '1.2', '01.2.3', '1.2.3-rc.1', 'latest', '1.2.3\n4.5.6'):
            with self.subTest(version=version):
                self.run_script('--version', version, success=False)
        for args in (['--version'], ['--bin-dir'], ['--unknown'], ['--bin-dir', 'relative'], ['--bin-dir', '/tmp/../bin']):
            self.run_script(*args, success=False)
        self.assertFalse(self.requests())

    def test_unexpected_latest_url_and_gh_tag_are_rejected(self):
        for url in ('http://github.com/progbeat/git-on/releases/tag/v0.1.0',
                    'https://example.invalid/releases/tag/v0.1.0',
                    'https://github.com/progbeat/git-on/releases/tag/v0.1.0-rc.1'):
            (self.releases / 'latest-url').write_text(url)
            self.run_script(success=False)
        for tag in ('', 'v0.1.0-rc.1', 'v0.2.0'):
            (self.releases / 'gh-tag').write_text(tag)
            self.run_script('--version', '0.1.0', github_cli=True, success=False)

    def test_download_hash_and_manifest_failures_preserve_old_binary(self):
        (self.releases / 'fail-download').touch()
        for gh in (False, True):
            self.run_script(github_cli=gh, success=False)
        (self.releases / 'fail-download').unlink()
        directory, archive = self.release('0.1.0')
        original = (directory / archive).read_bytes()
        (directory / archive).write_bytes(original + b'changed')
        self.assertIn('Checksum', self.run_script(success=False).stderr)
        (directory / archive).write_bytes(original)
        sums = (directory / 'SHA256SUMS').read_text()
        for content in ('', 'garbage\n', sums + sums, sums.replace(archive, '../' + archive)):
            (directory / 'SHA256SUMS').write_text(content)
            self.assertIn('SHA256SUMS', self.run_script(success=False).stderr)

    def test_binary_probe_failure_and_timeout_preserve_old_binary(self):
        for binary in (b'#!/bin/sh\necho git-on 99.0.0\n', b'broken executable', b'#!/bin/sh\nexit 127\n'):
            self.release('0.1.0', binary=binary)
            self.assertIn('version check', self.run_script(success=False).stderr)
        self.release('0.1.0', binary=b'#!/bin/sh\nexec sleep 60\n')
        self.assertIn('version check', self.run_script(success=False).stderr)

    def test_version_probe_kills_a_process_ignoring_term(self):
        self.release('0.1.0', binary=b'#!/bin/sh\ntrap "" TERM\nwhile :; do sleep 1; done\n')
        timeout = self.fakebin / 'timeout'
        timeout.unlink()
        timeout.write_text('#!/bin/sh\nfor arg; do shift; [ "$arg" != 15 ] || arg=1; set -- "$@" "$arg"; done\nexec ' +
                           shutil.which('timeout') + ' "$@"\n')
        timeout.chmod(0o755)
        process = subprocess.Popen(['/bin/sh', str(SCRIPT)], env=self.env, start_new_session=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            stdout, stderr = process.communicate(timeout=6)
            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertEqual(self.old.read_bytes(), b'old executable\n')
            self.assertFalse(list(self.bin.glob('.git-on-install.*')))
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()

    def test_archive_links_and_duplicate_binary_are_rejected(self):
        root = 'git-on-v0.1.0-' + TARGET
        binary = (root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.1.0\n')
        for members in ([binary, binary], [(root + '/git-on', 'symlink', '/bin/sh')],
                        [(root + '/git-on', 'hardlink', root + '/other')], [],
                        [(root + '/git-on', 'dir', b'')]):
            self.release('0.1.0', members=members)
            self.run_script(success=False)

    def test_duplicate_members_cannot_bypass_guard_with_early_exit(self):
        root = 'git-on-v0.1.0-' + TARGET
        # Concatenation still prints one valid version if the first script exits.
        # A version probe alone therefore cannot detect duplicate members.
        binary = (root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.1.0\nexit 0\n')
        self.release('0.1.0', members=[binary, binary])
        self.run_script(success=False)

    def test_only_fixed_binary_bytes_are_extracted(self):
        root = 'git-on-v0.1.0-' + TARGET
        outside = self.root / 'escape'
        self.release('0.1.0', members=[(root + '/git-on', 'file', b'#!/bin/sh\necho git-on 0.1.0\n'),
            (str(outside), 'file', b'no'), ('../../escape', 'file', b'no'),
            (root + '/link', 'symlink', str(outside))])
        self.run_script()
        self.assertFalse(outside.exists())
        self.assertFalse((self.bin / 'escape').exists())
        self.assertEqual(sorted(p.name for p in self.bin.iterdir()), ['git-on'])

    def test_destination_symlinks_directories_and_permissions(self):
        outside = self.root / 'outside'
        outside.write_bytes(b'keep')
        self.old.unlink()
        self.old.symlink_to(outside)
        result = subprocess.run(['/bin/sh', str(SCRIPT)], env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.old.is_symlink())
        self.assertEqual(outside.read_bytes(), b'keep')
        self.old.unlink()
        self.old.mkdir()
        result = subprocess.run(['/bin/sh', str(SCRIPT)], env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.old.rmdir()
        self.old.write_bytes(b'old executable\n')
        alias = self.root / 'alias'
        alias.symlink_to(self.bin)
        self.run_script('--bin-dir', str(alias), success=False)
        for mode in (0o775, 0o777, 0o555):
            self.bin.chmod(mode)
            try:
                self.assertIn('--bin-dir', self.run_script(success=False).stderr)
            finally:
                self.bin.chmod(0o755)

    def test_replace_failure_preserves_old_binary(self):
        (self.fakebin / 'mv').unlink()
        (self.fakebin / 'mv').write_text('#!/bin/sh\nexit 1\n')
        (self.fakebin / 'mv').chmod(0o755)
        self.run_script(success=False)

    def test_directory_swap_before_cd_cannot_redirect_installation(self):
        outside = self.root / 'outside-bin'
        outside.mkdir(mode=0o755)
        (outside / 'git-on').write_bytes(b'outside executable\n')
        saved = self.root / 'saved-bin'
        mkdir = self.fakebin / 'mkdir'
        mkdir.unlink()
        mkdir.write_text('#!/bin/sh\n' + shutil.which('mv') + ' -- "$RACE_BIN" "$RACE_SAVED"\n' +
                         shutil.which('ln') + ' -s -- "$RACE_OUTSIDE" "$RACE_BIN"\nexec ' +
                         shutil.which('mkdir') + ' "$@"\n')
        mkdir.chmod(0o755)
        result = subprocess.run(['/bin/sh', str(SCRIPT)], capture_output=True, text=True,
            env=dict(self.env, RACE_BIN=str(self.bin), RACE_SAVED=str(saved), RACE_OUTSIDE=str(outside)), timeout=30)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual((saved / 'git-on').read_bytes(), b'old executable\n')
        self.assertEqual((outside / 'git-on').read_bytes(), b'outside executable\n')

    def test_platform_and_dependency_failures_before_download(self):
        for name, source in [('uname', '#!/bin/sh\necho unsupported\n'),
                             ('getconf', '#!/bin/sh\necho glibc 2.34\n'),
                             ('id', '#!/bin/sh\necho 0\n')]:
            tool = self.fakebin / name
            original = tool.readlink()
            tool.unlink()
            tool.write_text(source)
            tool.chmod(0o755)
            self.run_script(success=False)
            tool.unlink()
            tool.symlink_to(original)
        (self.fakebin / 'sha256sum').unlink()
        self.assertIn('sha256sum', self.run_script(success=False).stderr)
        self.assertFalse(self.requests())

    def test_path_aliases_and_shadow_diagnostics_do_not_execute_old_binary(self):
        alias = self.root / 'bin alias'
        alias.symlink_to(self.bin)
        self.env['PATH'] = str(alias) + os.pathsep + self.env['PATH']
        self.assertNotIn('PATH selects', self.run_script().stdout)
        marker = self.root / 'executed-shadow'
        earlier = self.fakebin / 'git-on'
        earlier.write_text('#!/bin/sh\n: > "' + str(marker) + '"\n')
        earlier.chmod(0o755)
        self.env['PATH'] = str(self.fakebin) + os.pathsep + str(alias)
        self.assertIn('PATH selects', self.run_script().stdout)
        self.assertFalse(marker.exists())

    def test_relative_and_empty_path_entries_survive_directory_pinning(self):
        for path in ('tools', ':tools', 'tools:'):
            result = subprocess.run(['/bin/sh', str(SCRIPT)], cwd=self.root,
                env=dict(self.env, PATH=path), capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_truncated_pipe_does_not_start_or_drop_arguments(self):
        source = SCRIPT.read_text()
        start = source.rfind('{ git_on_install')
        boundaries = [i+1 for i,c in enumerate(source) if c=='\n' and i+1<=start]
        boundaries += list(range(start, len(source.rstrip())))
        for boundary in boundaries:
            with self.subTest(boundary=boundary):
                subprocess.run(['/bin/sh', '-s', '--', '--version', '0.1.0'], input=source[:boundary],
                               env=self.env, capture_output=True, text=True, timeout=5)
                self.assertEqual(self.old.read_bytes(), b'old executable\n')
                self.assertFalse(self.requests())

    def test_interruption_cleans_staging_and_preserves_old_binary(self):
        self.release('0.1.0', binary=b'#!/bin/sh\nexec sleep 60\n')
        process = subprocess.Popen(['/bin/sh', str(SCRIPT)], env=self.env, start_new_session=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 10
            while not list(self.bin.glob('.git-on-install.*/git-on')) and time.monotonic()<deadline:
                time.sleep(.02)
            self.assertTrue(list(self.bin.glob('.git-on-install.*/git-on')))
            os.killpg(process.pid, signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=10)
            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertEqual(self.old.read_bytes(), b'old executable\n')
            self.assertFalse(list(self.bin.glob('.git-on-install.*')))
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()

    def test_signal_after_atomic_commit_reports_no_false_rollback(self):
        mv = self.fakebin / 'mv'
        mv.unlink()
        mv.write_text('#!/bin/sh\n' + shutil.which('mv') + ' "$@" || exit\nkill -TERM "$PPID"\n')
        mv.chmod(0o755)
        result = subprocess.run(['/bin/sh', str(SCRIPT)], env=self.env, capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'git-on 0.1.0', self.old.read_bytes())
        self.assertNotIn('unchanged', result.stderr)
        self.assertFalse(list(self.bin.glob('.git-on-install.*')))


def packaged_acceptance(distribution, destination):
    case = InstallerTests()
    case.setUp()
    try:
        version = json.loads((distribution / 'release.json').read_text())['version']
        case.release(version, distribution=distribution)
        archive = 'git-on-v' + version + '-' + TARGET + '.tar.gz'
        # Freeze the legacy publication contract: extra assets/checksum entries
        # break saved Python installers. No release repacking is necessary.
        assert {p.name for p in distribution.iterdir() if p.is_file()} == {archive, 'release.json', 'SHA256SUMS'}
        assert {line.split()[1] for line in (distribution/'SHA256SUMS').read_text().splitlines()} == {archive, 'release.json'}
        with tarfile.open(distribution / archive, 'r:gz') as package:
            license_file = package.extractfile(archive[:-7] + '/LICENSE')
            assert license_file.read() == (ROOT / 'LICENSE').read_bytes()
        for args in ([], ['--version', version], ['--version', 'v'+version]):
            case.run_script('--bin-dir', str(destination), *args)
        result = subprocess.run([str(destination/'git-on'), '--version'], capture_output=True, text=True, check=True)
        assert result.stdout.strip() == 'git-on ' + version
        print('Installed real packaged binary using shell, latest/pinned/repeat paths; Python/jq absent from installer PATH')
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
