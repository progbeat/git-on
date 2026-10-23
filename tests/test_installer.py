#!/usr/bin/env python3
"""Dispatcher and fail-closed checksum regressions; no network or installation."""
import hashlib
import importlib.util
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
        curl.write_text(f'#!{sys.executable}\n' + '''import os,sys
from pathlib import Path
args=sys.argv[1:]
with open(os.environ['CURL_LOG'],'a') as f:f.write(repr(args)+'\\n')
if '-w' in args:
 print('https://github.com/progbeat/git-on/releases/tag/'+os.environ.get('LATEST','v1.3.0'),end='')
else:
 path=Path(args[args.index('-o')+1])
 path.write_text('printf "%s\\\\n" "$GIT_ON_INSTALL_DIR" > "$MARKER"\\nexit '+os.environ.get('CHILD_STATUS','0')+'\\n')
sys.exit(int(os.environ.get('DOWNLOAD_STATUS','0')))
''')
        curl.chmod(0o755)
        self.env = dict(os.environ, PATH=str(self.bin)+':/usr/bin:/bin', TMPDIR=str(self.downloads), CURL_LOG=str(self.log), MARKER=str(self.marker), HOME=str(self.root))

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

    def test_old_versions_and_unreleased_latest_are_rejected(self):
        for version in ['0.1.0','1.2.999','v1.2.0']:
            result = self.run_installer('--version',version)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b'1.3.0 and later', result.stderr)
            self.assertFalse(self.log.exists())
        result = self.run_installer(LATEST='v1.2.0')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_normal_install_has_no_deprecation_chatter_and_explicit_flag_is_actionable(self):
        normal = self.run_installer()
        self.assertEqual(normal.returncode, 0, normal.stderr)
        self.assertNotIn(b'--github-cli', normal.stdout + normal.stderr)
        self.assertNotIn(b'removed', normal.stdout + normal.stderr)
        self.log.unlink()
        result = self.run_installer('--github-cli')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'--github-cli', result.stderr)
        self.assertIn(b'HTTPS', result.stderr)
        self.assertNotIn(b'1.3.0', result.stderr)
        self.assertNotIn(b'removed', result.stderr)
        self.assertFalse(self.log.exists())

    def test_invalid_options_and_github_cli_migration(self):
        for args in [['--version',''], ['--version='], ['--github-cli'], ['--version','x;touch bad'], ['--version','1.3.0-rc.1'], ['--version','01.3.0'], ['--bin-dir','relative'], ['--version'], ['--bogus']]:
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
