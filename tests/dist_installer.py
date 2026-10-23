#!/usr/bin/env python3
"""Exercise the ACTUAL corrected cargo-dist installer and packaged host binary.
No release is created and no network is used: a curl fixture supplies CI artifacts.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--dist',type=Path,required=True)
p.add_argument('--install-dir',type=Path,required=True)
p.add_argument('--without-gnu-hash',action='store_true',help='verify complete macOS installer with only stock shasum')
args=p.parse_args()
dist=args.dist.resolve()
install=args.install_dir.resolve()
with tempfile.TemporaryDirectory() as scratch:
    root=Path(scratch)
    programs=root/'programs'; programs.mkdir()
    curl=programs/'curl'
    curl.write_text(f'#!{sys.executable}\n'+'''import os,sys,shutil
from pathlib import Path
args=sys.argv[1:]
if '--version' in args:print('curl 8.0.0');sys.exit(0)
if '--help' in args:print('--proto --tlsv1.2');sys.exit(0)
url=next((a for a in args if a.startswith('https://')),None)
if not url:sys.exit(1)
flag='--output' if '--output' in args else '-o'
shutil.copyfile(Path(os.environ['FIXTURE_DIST'])/url.rsplit('/',1)[1],args[args.index(flag)+1])
''')
    curl.chmod(0o755)
    env=dict(os.environ,PATH=str(programs)+os.pathsep+os.environ['PATH'],GIT_ON_INSTALL_DIR=str(install),GIT_ON_NO_MODIFY_PATH='1',GIT_ON_DISABLE_UPDATE='1',HOME=str(root/'home'),TMPDIR=str(root),FIXTURE_DIST=str(dist))
    if args.without_gnu_hash:
        assert sys.platform=='darwin', 'stock macOS hash-tool scenario requires macOS'
        env['PATH']=str(programs)+':/usr/bin:/bin:/usr/sbin'
        assert shutil.which('sha256sum',path=env['PATH']) is None, 'GNU hash tool was not excluded'
        assert shutil.which('shasum',path=env['PATH']) is not None, 'stock macOS shasum required'
    for _ in range(2):
        subprocess.run(['sh',str(dist/'git-on-installer.sh')],env=env,check=True,timeout=90)
        subprocess.run([str(install/'git-on'),'--version'],check=True,timeout=10)
    before=(install/'git-on').read_bytes()
    # Actual archive mismatch and truncation must leave the installed executable.
    for archive in dist.glob('git-on-*.tar.gz'):
        original=archive.read_bytes()
        try:
            archive.write_bytes(original[:max(1,len(original)//2)])
            result=subprocess.run(['sh',str(dist/'git-on-installer.sh')],env=env,capture_output=True,timeout=90)
            if archive.name == ('git-on-aarch64-apple-darwin.tar.gz' if sys.platform=='darwin' else 'git-on-x86_64-unknown-linux-gnu.tar.gz'):
                assert result.returncode!=0,(archive,result.stdout,result.stderr)
                assert b'checksum mismatch' in result.stderr,result.stderr
                assert (install/'git-on').read_bytes()==before
        finally: archive.write_bytes(original)
    assert not list(root.glob('tmp.*')), 'installer leaked temporary state'
