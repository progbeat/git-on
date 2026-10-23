#!/usr/bin/env python3
"""Fail closed when artifacts change across jobs or the release set is incomplete."""
import hashlib
import json
from pathlib import Path
import re
import sys


def verify(directory):
    directory=Path(directory)
    expected={}
    for line in (directory/'SHA256SUMS').read_text().splitlines():
        match=re.fullmatch(r'([0-9a-f]{64})  ([A-Za-z0-9_.-]+)',line)
        if not match or match[2] in expected: raise ValueError('malformed/duplicate artifact inventory')
        expected[match[2]]=match[1]
    actual={p.name for p in directory.iterdir()}
    if actual != set(expected)|{'SHA256SUMS'}: raise ValueError('artifact set changed between jobs')
    required={'git-on-aarch64-apple-darwin.tar.gz','git-on-x86_64-unknown-linux-gnu.tar.gz','git-on-installer.sh','release.json','source.tar.gz'}
    if not required<=set(expected): raise ValueError('incomplete two-platform release')
    for name,digest in expected.items():
        p=directory/name
        if p.is_symlink() or not p.is_file() or hashlib.sha256(p.read_bytes()).hexdigest()!=digest: raise ValueError('artifact SHA-256 mismatch: '+name)
    installer=(directory/'git-on-installer.sh').read_text()
    if '# git-on: fail-closed SHA-256' not in installer or 'skipping sha256' in installer: raise ValueError('uncorrected cargo-dist installer')
    for name in required-{'release.json','git-on-installer.sh','source.tar.gz'}:
        if hashlib.sha256((directory/name).read_bytes()).hexdigest() not in installer: raise ValueError('installer archive hash differs: '+name)
    manifest=json.loads((directory/'release.json').read_text())
    if set(manifest['targets'])!={'x86_64-unknown-linux-gnu','aarch64-apple-darwin'}: raise ValueError('incomplete target provenance')
    print('Verified complete cargo-dist artifact inventory')


if __name__=='__main__': verify(sys.argv[1] if len(sys.argv)>1 else 'dist')
