#!/usr/bin/env python3
"""Dispatcher and fail-closed checksum regressions; no network or installation."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('hardening', ROOT / 'scripts/harden_dist_installer.py')
hardening = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hardening)


class DispatcherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.downloads = self.root / 'downloads'
        self.downloads.mkdir()
        self.marker = self.root / 'executed'
        self.log = self.root / 'curl.log'
        curl = self.bin / 'curl'
        curl.write_text(f'#!{sys.executable}\n' + '''import os,shlex,sys
from pathlib import Path
args=sys.argv[1:]
with open(os.environ['CURL_LOG'],'a') as f:f.write(repr(args)+'\\n')
path=Path(args[args.index('-o')+1])
capture='import json,os,sys;json.dump(sys.argv[1:],open(os.environ["ARGV_LOG"],"w"))'
path.write_text('printf "%s\\\\n" "${GIT_ON_INSTALL_DIR:-$HOME/.local/bin}" > "$MARKER"\\n'+shlex.quote(sys.executable)+' -c '+shlex.quote(capture)+' "$@"\\nexit '+os.environ.get('CHILD_STATUS','0')+'\\n')
sys.exit(int(os.environ.get('DOWNLOAD_STATUS','0')))
''')
        curl.chmod(0o755)
        self.argv_log = self.root / 'argv.json'
        self.env = dict(os.environ, PATH=str(self.bin)+':/usr/bin:/bin', TMPDIR=str(self.downloads), CURL_LOG=str(self.log), MARKER=str(self.marker), HOME=str(self.root), ARGV_LOG=str(self.argv_log))

    def run_installer(self, *args, **env):
        result = subprocess.run(['sh', str(ROOT/'install.sh'), *args], env=dict(self.env, **env), capture_output=True, timeout=10)
        self.assertEqual(list(self.downloads.iterdir()), [])
        return result

    def test_latest_explicit_optional_v_bin_dir_and_repeat_update(self):
        for args in [[], ['--version','1.3.0'], ['--version','v1.3.0'], ['--version=2.0.0','--bin-dir',str(self.root/'custom')]]:
            for _ in range(2):
                result = self.run_installer(*args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.marker.read_text().strip(), str(self.root/('custom' if '--bin-dir' in args else '.local/bin')))
        self.assertIn('/v2.0.0/git-on-installer.sh', self.log.read_text())

    def test_missing_release_installer_fails_on_http_not_a_local_version_check(self):
        for version in ['0.1.0', 'v1.2.0', 'unpublished-tag']:
            result = self.run_installer('--version', version, DOWNLOAD_STATUS='22')
            self.assertEqual(result.returncode, 22)
            self.assertIn('/download/v'+version.removeprefix('v')+'/git-on-installer.sh', self.log.read_text())
            self.assertFalse(self.marker.exists())
        self.assertNotIn(b'supported', result.stderr)

    def test_normal_install_is_quiet_and_all_other_args_are_delegated(self):
        normal = self.run_installer()
        self.assertEqual(normal.returncode, 0, normal.stderr)
        self.assertNotIn(b'--github-cli', normal.stdout + normal.stderr)
        self.assertNotIn(b'removed', normal.stdout + normal.stderr)
        self.log.unlink()
        for flag in ['--not-an-option', '--github-cli']:
            result = self.run_installer(flag)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(self.argv_log.read_text()), [flag])
        forwarded = ['--quiet', '', 'space value', '*', '--verbose', '--', 'two\nlines']
        result = self.run_installer('--quiet', '--version', '2.0.0', *forwarded[1:], '--bin-dir', str(self.root/'chosen'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.argv_log.read_text()), forwarded)
        self.assertEqual(self.marker.read_text().strip(), str(self.root/'chosen'))

    def test_owned_options_require_values(self):
        for args in [['--version'], ['--bin-dir'], ['--quiet', '--version'], ['--verbose', '--bin-dir']]:
            result = self.run_installer(*args)
            self.assertNotEqual(result.returncode, 0, args)
            self.assertFalse(self.marker.exists())
            self.assertFalse(self.log.exists())

    def test_failed_partial_download_never_executes_and_exit_propagates(self):
        result = self.run_installer('--version','1.3.0', DOWNLOAD_STATUS='22')
        self.assertEqual(result.returncode, 22, result.stderr)
        self.assertFalse(self.marker.exists())
        result = self.run_installer('--version','1.3.0', CHILD_STATUS='23')
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertTrue(self.marker.exists())

    def test_incomplete_bootstrap_cannot_start(self):
        source = (ROOT/'install.sh').read_text().rsplit('git_on_install "$@"',1)[0]
        result = subprocess.run(['sh','-s','--','--version','1.3.0'], input=source.encode(), env=self.env, capture_output=True, timeout=10)
        self.assertFalse(self.log.exists())

    def test_truncation_inside_final_invocation_cannot_discard_arguments(self):
        source = (ROOT/'install.sh').read_text()
        start = source.rindex('git_on_install "$@"')
        # Every nonempty prefix of the call, including the bare command token,
        # must remain inert until its complete argument-preserving group arrives.
        for end in range(start+1, len(source.rstrip())):
            with self.subTest(end=end-start):
                subprocess.run(['sh','-s','--','--version','1.3.0','--bin-dir',str(self.root/'chosen')], input=source[:end].encode(), env=self.env, capture_output=True, timeout=10)
                self.assertFalse(self.log.exists(), source[start:end])
                self.assertFalse(self.marker.exists())


class ChecksumTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.archive = self.root/'input.tar.gz'
        self.archive.write_bytes(b'complete archive')
        self.digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()

    def verify(self, value=None, style='sha256', tool='shasum', failing=False):
        programs = self.root/'programs'
        programs.mkdir(exist_ok=True)
        for p in programs.iterdir(): p.unlink()
        if tool:
            path = programs/tool
            original = shutil.which(tool)
            if original and not failing:
                path.symlink_to(original)
            else:
                path.write_text('#!/bin/sh\nexit 7\n' if failing else '#!/bin/sh\nprintf "'+self.digest+'  %s\\n" "$3"\n')
                path.chmod(0o755)
        script = 'set -u\nerr() { echo "$*" >&2; exit 1; }\ncheck_cmd() { command -v "$1" >/dev/null 2>&1; }\n'+hardening.VERIFY+'\nverify_checksum "$1" "$2" "$3"\n'
        return subprocess.run(['/bin/sh','-c',script,'verify',str(self.archive),style,self.digest if value is None else value], env=dict(os.environ, PATH=str(programs)),capture_output=True,timeout=10)

    def test_linux_and_macos_hash_tools(self):
        for tool in ['sha256sum','shasum']:
            result = self.verify(tool=tool)
            self.assertEqual(result.returncode,0,result.stderr)

    def test_missing_malformed_mismatch_unavailable_and_failed_hash_tool(self):
        for options in [dict(value=''),dict(value='bad'),dict(value='z'*64),dict(value='0'*64),dict(style='false'),dict(tool=None),dict(failing=True)]:
            result = self.verify(**options)
            self.assertNotEqual(result.returncode,0,options)
        self.archive.write_bytes(b'truncated')
        self.assertNotEqual(self.verify().returncode,0)


if __name__ == '__main__': unittest.main(verbosity=2)
