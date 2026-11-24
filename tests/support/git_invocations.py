"""Observe native Git calls from the fixture without enabling Git trace files."""
import json
import os
import shutil
import sys


def record_git_invocations(root, env):
    native = shutil.which('git', path=env['PATH'])
    assert native, 'native Git is missing from the fixture PATH'
    programs = root / 'recorded-git'
    programs.mkdir()
    log = root / 'git-invocations.jsonl'
    log.touch()
    git = programs / 'git'
    git.write_text(f"""#!{sys.executable}
import json, os, sys
with open({str(log)!r}, 'a') as output:
    output.write(json.dumps(sys.argv[1:]) + '\\n')
os.execv({native!r}, [{native!r}, *sys.argv[1:]])
""")
    git.chmod(0o700)
    return dict(env, PATH=str(programs) + os.pathsep + env['PATH']), log


def git_invocations(log):
    return [json.loads(line) for line in log.read_text().splitlines()]
