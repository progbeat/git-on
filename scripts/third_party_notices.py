#!/usr/bin/env python3
"""Collect the exact target's locked normal/build dependency license texts.

Development-only crates are excluded. Known bundled C-library licenses are
included with their Rust bindings. Missing texts fail rather than guessing.
"""
import argparse
import json
from pathlib import Path
import subprocess


def collect(metadata):
    packages={p['id']:p for p in metadata['packages']}
    nodes={n['id']:n for n in metadata['resolve']['nodes']}
    todo=[metadata['resolve']['root']];selected=set()
    while todo:
        identifier=todo.pop()
        if identifier in selected:continue
        selected.add(identifier)
        todo.extend(d['pkg'] for d in nodes[identifier]['deps'] if any(k['kind']!='dev' for k in d['dep_kinds']))
    sections=[]
    for p in sorted((packages[i] for i in selected if packages[i]['source'] is not None),key=lambda p:(p['name'],p['version'])):
        root=Path(p['manifest_path']).parent.resolve()
        paths={f for f in root.iterdir() if f.is_file() and f.name.lower().startswith(('license','copying','copyright','notice'))}
        if p['license_file']:paths.add(root/p['license_file'])
        bundled={'libgit2-sys':['libgit2/COPYING','libgit2/deps/*/LICENSE*','libgit2/deps/*/COPYING*'],
                 'libz-sys':['src/zlib/LICENSE'], 'libssh2-sys':['libssh2/COPYING'], 'curl-sys':['curl/COPYING']}
        for pattern in bundled.get(p['name'],[]):paths.update(root.glob(pattern))
        if not paths:raise ValueError('missing license text: '+p['name'])
        sections.append('\n## '+p['name']+' '+p['version']+'\nDeclared license: '+str(p['license'])+'\n')
        for path in sorted(paths):
            if not path.resolve().is_relative_to(root):raise ValueError('license path escapes package: '+p['name'])
            if not path.is_file():raise ValueError('missing declared license: '+str(path))
            sections.append('\n### '+str(path.relative_to(root))+'\n\n'+path.read_text().rstrip()+'\n')
    return ''.join(sections)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--target',required=True)
    p.add_argument('--output',type=Path,default=Path('THIRD_PARTY_NOTICES'))
    args=p.parse_args()
    metadata=json.loads(subprocess.check_output(['cargo','metadata','--locked','--offline','--format-version=1','--filter-platform',args.target]))
    text='Third-party software notices for git-on\nTarget: '+args.target+'\nThe project LICENSE is distributed separately.\n'+collect(metadata)
    args.output.write_text(text)
    print('Wrote locked runtime/build dependency license texts to '+str(args.output))


if __name__=='__main__':main()
