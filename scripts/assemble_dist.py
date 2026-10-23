#!/usr/bin/env python3
"""Assemble tested cargo-dist artifacts and an exact cross-job SHA-256 inventory."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tomllib

source=Path('target/distrib')
output=Path('dist')
output.mkdir(exist_ok=False)
version=tomllib.loads(Path('Cargo.toml').read_text())['package']['version']
for name in ('git-on-aarch64-apple-darwin.tar.gz','git-on-x86_64-unknown-linux-gnu.tar.gz','git-on-installer.sh','source.tar.gz'):
    shutil.copy2(source/name,output/name)
    digest=hashlib.sha256((output/name).read_bytes()).hexdigest()
    (output/(name+'.sha256')).write_text(digest+' *'+name+'\n')
manifest={'version':version,'commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'cargo_dist':'0.33.0','targets':{'x86_64-unknown-linux-gnu':{'baseline':'Ubuntu 22.04 / glibc 2.35'},'aarch64-apple-darwin':{'baseline':'macOS 13+; macFUSE libfuse2 for mounts'}}}
(output/'release.json').write_text(json.dumps(manifest,sort_keys=True)+'\n')
(output/'SHA256SUMS').write_text(''.join(hashlib.sha256(p.read_bytes()).hexdigest()+'  '+p.name+'\n' for p in sorted(output.iterdir())))
