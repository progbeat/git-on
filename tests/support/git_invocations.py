"""Record native Git invocations without asking Git to write a trace."""
import json
import os
import shlex
import shutil
import sys


def record_git_invocations(root, env):
    native = shutil.which('git', path=env['PATH'])
    assert native, 'native Git is missing from the fixture PATH'
    programs = root / 'recorded-git'
    programs.mkdir()
    log = root / 'git-invocations.jsonl'
    log.touch()
    script = f"""import json, os, sys
with open({str(log)!r}, 'a') as output:
    output.write(json.dumps(sys.argv[1:]) + '\\n')
os.execv({native!r}, [{native!r}, *sys.argv[1:]])
"""
    git = programs / 'git'
    git.write_text(f'#!/bin/sh\nexec {shlex.quote(sys.executable)} -I -S -c {shlex.quote(script)} "$@"\n')
    git.chmod(0o700)
    return dict(env, PATH=str(programs) + os.pathsep + env['PATH']), log


def git_invocations(log):
    return [json.loads(line) for line in log.read_text().splitlines()]
