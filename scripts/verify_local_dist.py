#!/usr/bin/env python3
"""Verify native build inputs BEFORE global regeneration can rehash artifacts."""
import hashlib
import json
from pathlib import Path
import re
import subprocess

TARGETS=('aarch64-apple-darwin','x86_64-unknown-linux-gnu')


def verify(directory, commit):
    directory=Path(directory)
    for target in TARGETS:
        name='git-on-'+target+'.tar.gz'
        archive=directory/name
        checksum=(directory/(name+'.sha256')).read_text()
        match=re.fullmatch(r'([0-9a-f]{64}) [ *]'+re.escape(name)+r'\n?',checksum)
        if not match or archive.is_symlink() or not archive.is_file(): raise ValueError('invalid native archive/checksum: '+name)
        digest=hashlib.sha256(archive.read_bytes()).hexdigest()
        if digest!=match[1]: raise ValueError('native archive changed across jobs: '+name)
        provenance=json.loads((directory/('provenance-'+target+'.json')).read_text())
        if provenance.get('commit')!=commit or provenance.get('target')!=target: raise ValueError('native artifact source/target mismatch: '+name)
        manifest=json.loads((directory/(target+'-dist-manifest.json')).read_text())
        if manifest['artifacts'][name].get('checksums',{}).get('sha256')!=digest: raise ValueError('native cargo-dist manifest mismatch: '+name)
    print('Verified both native archives and their build provenance before generation')


if __name__=='__main__': verify('target/distrib',subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip())
