#!/usr/bin/env python3
"""Read-only report for the existing Linux CI host; never changes capabilities."""
import gzip
import json
import os
from pathlib import Path

kernel=os.uname().release
report={'kernel':kernel,'uid':os.getuid(),'fuse_device':Path('/dev/fuse').exists(),'kernel_passthrough_config':'unknown'}
for file in [Path('/boot/config-'+kernel),Path('/proc/config.gz'),Path('/lib/modules')/kernel/'build/.config']:
    try:
        data=gzip.decompress(file.read_bytes()).decode() if file.suffix=='.gz' else file.read_text()
    except (OSError,UnicodeError):continue
    for line in data.splitlines():
        if line.startswith('CONFIG_FUSE_PASSTHROUGH=') or line=='# CONFIG_FUSE_PASSTHROUGH is not set':
            report['kernel_passthrough_config']=line
            report['kernel_config_source']=str(file)
    if report['kernel_passthrough_config']!='unknown':break
try:
    for line in Path('/proc/self/status').read_text().splitlines():
        if line.split(':',1)[0] in ['CapEff','CapPrm','CapAmb','CapBnd','NoNewPrivs']:
            key,value=line.split(':',1);report[key]=value.strip()
except OSError:pass
print(json.dumps(report,sort_keys=True))
