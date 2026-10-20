#!/usr/bin/env python3
"""Install the real executable in a network-isolated Docker image without Python/jq.

Build the tiny root filesystem entirely from local Linux utilities and their
shared libraries. No registry, package manager, network or privileged container
is needed. Python only constructs the test fixture outside the container.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile

from test_installer import CURL_FIXTURE, InstallerTests, TOOLS, ROOT


def copy_executable(source, destination, root):
    source = Path(source).resolve()
    target = root / destination.lstrip('/')
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    target.chmod(0o755)
    result = subprocess.run(['ldd', str(source)], capture_output=True, text=True)
    if result.returncode and 'not a dynamic executable' not in result.stderr:
        raise RuntimeError(result.stdout + result.stderr)
    for library in re.findall(r'(?:=>\s+|^\s*)(/[^\s]+)', result.stdout, re.MULTILINE):
        output = root / library.lstrip('/')
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(Path(library).resolve(), output)
        output.chmod(stat.S_IMODE(Path(library).stat().st_mode))


def main(binary):
    binary = binary.resolve()
    version = subprocess.check_output([str(binary), '--version'], text=True).strip().removeprefix('git-on ')
    case = InstallerTests()
    case.setUp()
    image = None
    try:
        case.release(version, binary=binary.read_bytes())
        with tempfile.TemporaryDirectory(prefix='git-on-no-python-') as directory:
            root = Path(directory) / 'root'
            root.mkdir()
            for tool in set(TOOLS + ['sh']):
                source = shutil.which(tool)
                if not source:
                    raise RuntimeError('Missing host test tool: ' + tool)
                copy_executable(source, '/bin/' + tool, root)
            # The real executable needs these same runtime libraries in production.
            copy_executable(binary, '/fixture-binary', root)
            shutil.copytree(case.releases, root / 'fixtures')
            (root / 'tools').mkdir()
            (root / 'tools/curl').write_text(CURL_FIXTURE)
            (root / 'tools/curl').chmod(0o755)
            shutil.copyfile(ROOT / 'install.sh', root / 'install.sh')
            script = r'''#!/bin/sh
set -eu
export PATH=/tools:/bin HOME=/work/home INSTALLER_FIXTURES=/work/releases LC_ALL=C
for tool in python python3 jq; do
    if command -v "$tool" >/dev/null 2>&1; then echo "Unexpected runtime: $tool"; exit 1; fi
done
mkdir -p "$HOME"
cp -r /fixtures "$INSTALLER_FIXTURES"
printf v0.1.0 > "$INSTALLER_FIXTURES/latest"
/bin/sh /install.sh
"$HOME/.local/bin/git-on" --version
printf 'v%s' VERSION > "$INSTALLER_FIXTURES/latest"
/bin/sh /install.sh
[ "$("$HOME/.local/bin/git-on" --version)" = "git-on VERSION" ]
/bin/sh /install.sh --version v0.1.0
[ "$("$HOME/.local/bin/git-on" --version)" = 'git-on 0.1.0' ]
/bin/sh /install.sh --version VERSION
before=$(sha256sum "$HOME/.local/bin/git-on")
: > "$INSTALLER_FIXTURES/fail-download"
if /bin/sh /install.sh; then echo 'Expected download failure'; exit 1; fi
[ "$(sha256sum "$HOME/.local/bin/git-on")" = "$before" ]
rm "$INSTALLER_FIXTURES/fail-download"
printf corrupt >> "$INSTALLER_FIXTURES/vVERSION/git-on-vVERSION-x86_64-unknown-linux-gnu.tar.gz"
if /bin/sh /install.sh; then echo 'Expected checksum failure'; exit 1; fi
[ "$(sha256sum "$HOME/.local/bin/git-on")" = "$before" ]
printf '%s\n' 'PASS: no Python/jq in filesystem; real binary latest/pinned/upgrade; download and checksum failure preservation'
'''.replace('VERSION', version)
            (root / 'run-test.sh').write_text(script)
            for path in root.rglob('*'):
                if path.is_dir():
                    path.chmod(0o755)
                elif not path.stat().st_mode & 0o111:
                    path.chmod(0o644)
            # Explicitly assert the complete image payload before import.
            assert not any(p.name.startswith(('python', 'libpython')) or p.name == 'jq' for p in root.rglob('*'))
            archive = Path(directory) / 'root.tar'
            with tarfile.open(archive, 'w') as package:
                for entry in root.iterdir():
                    package.add(entry, arcname=entry.name)
            image = subprocess.check_output(['docker', 'import', str(archive)], text=True).strip()
            subprocess.run(['docker', 'run', '--rm', '--network', 'none', '--read-only',
                            '--user', f'{os.getuid()}:{os.getgid()}', '--tmpfs', '/work:rw,exec,mode=1777',
                            '--tmpfs', '/tmp:rw,mode=1777', image, '/bin/sh', '/run-test.sh'], check=True)
    finally:
        if image:
            subprocess.run(['docker', 'image', 'rm', image], check=True, stdout=subprocess.DEVNULL)
        case.doCleanups()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    main(parser.parse_args().binary)
