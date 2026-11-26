"""Observe native Git calls from the fixture without enabling Git trace files."""
import json
import os
import shlex
import shutil
import sys


def record_git_invocations(root, env, *, include_env=(), interpreter=sys.executable,
                           native=None, native_fallback=None):
    native = shutil.which('git', path=env['PATH']) if native is None else os.fspath(native)
    assert native, 'native Git is missing from the fixture PATH'
    fallback = os.fspath(native_fallback) if native_fallback is not None else None
    programs = root / 'recorded-git'
    programs.mkdir()
    log = root / 'git-invocations.jsonl'
    log.touch()
    git = programs / 'git'
    script = f"""import json, os, sys
argv = sys.argv[1:]
keys = {include_env!r}
entry = {{'argv': argv, 'env': {{key: os.environ.get(key) for key in keys}}}} if keys else argv
with open({str(log)!r}, 'a') as output:
    output.write(json.dumps(entry) + '\\n')
try:
    os.execv({native!r}, [{native!r}, *argv])
except FileNotFoundError:
    fallback = {fallback!r}
    if fallback is None:
        raise
    os.execv(fallback, [fallback, *argv])
"""
    git.write_text(f'#!/bin/sh\nexec {shlex.quote(str(interpreter))} -I -S -c {shlex.quote(script)} "$@"\n')
    git.chmod(0o700)
    return dict(env, PATH=str(programs) + os.pathsep + env['PATH']), log


def git_invocations(log):
    return [json.loads(line) for line in log.read_text().splitlines()]
