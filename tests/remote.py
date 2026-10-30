#!/usr/bin/env python3
"""Deterministic native Git, HTTPS and OpenSSH acceptance. Keys are disposable.

The source probe runs the production loader without FUSE; --require-fuse also
executes the installed CLI through actual mounts. No transport is mocked.
"""
import argparse
import contextlib
import errno
import fcntl
import http.server
import json
import os
import pty
from pathlib import Path
import pwd
import random
import re
import select
import shlex
import shutil
import signal
import socket
import ssl
import sys
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
PARSER = argparse.ArgumentParser()
PARSER.add_argument('--probe', type=Path, default=ROOT / 'target/debug/examples/source-probe')
PARSER.add_argument('--binary', type=Path, default=ROOT / 'target/debug/git-on')
PARSER.add_argument('--require-fuse', action='store_true')
PARSER.add_argument('--million', action='store_true')
PARSER.add_argument('--evidence-output',type=Path,help='Machine-readable completed remote-million evidence')
PARSER.add_argument('--before-resource-probe', type=Path, help='Master source probe for resource red-before evidence')
PARSER.add_argument('--before-binary', type=Path, help='Authentic pre-shutdown-fix binary for focused red-before replay')
PARSER.add_argument('--before-lifecycle-binary', type=Path, help='Authentic pre-review-lifecycle-fix binary for mounted SSH shutdown replay')
PARSER.add_argument('--before-policy-binary', type=Path, help='Authentic e427743 binary for ignore-acquisition errno red-before replay')
OPTIONS, ARGS = PARSER.parse_known_args()
# Artifact arguments are relative to the invoking harness, not a forked
# fixture's cwd. Resolve them before any child changes directories.
for artifact in ('probe','binary','before_binary','before_lifecycle_binary','before_policy_binary','before_resource_probe','evidence_output'):
    selected=getattr(OPTIONS,artifact)
    if selected is not None: setattr(OPTIONS,artifact,selected.resolve())

def run(argv, **kwargs):
    result=subprocess.run(list(map(str, argv)), capture_output=True, **kwargs)
    if result.returncode: raise RuntimeError('fixture command failed: '+result.stderr.decode())
    return result.stdout

def private_storage_fault(fault, root="Path.cwd().parent"):
    """Corrupt the storage used by the selected import path, before a demand."""
    if fault=='odb':
        return f"packs={root}/'remote-repository/objects/pack'; packs.rename(packs.with_name('blocked-packs')); packs.write_bytes(b'blocked')\n"
    assert fault=='raw'
    return (f"parts=next(({root}).glob('raw-parts-*')); target={root}/'unexpected-target'\n"
            "for index in range(256):\n path=parts/f'{index:x}-0'\n if not os.path.lexists(path): path.symlink_to(target)\n")

def exec_fixture_child(argv, cwd, env):
    try:
        os.chdir(cwd)
        os.execve(argv[0], argv, env)
    except BaseException as error:
        try: os.write(2,('fixture exec failed: '+str(error)[:1024]+'\n').encode())
        except BaseException: pass
    # A forked fixture must never return to the inherited unittest loop, even
    # when chdir/exec or writing its diagnostic fails.
    os._exit(127)

class Fixture:
    def __init__(self):
        self.temp = tempfile.TemporaryDirectory(prefix='git-on-transport-')
        self.root = Path(self.temp.name)
        self.repo = self.root / 'server.git'
        self.cwd = self.root / 'outside'
        self.cwd.mkdir()
        self.env = {k:v for k,v in os.environ.items() if not k.startswith(('GIT_', 'SSH_'))}
        self.env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1', XDG_CACHE_HOME=str(self.root / 'cache'), TMPDIR=str(self.root), LC_ALL='C')
        self.git('init', '--bare', '--quiet', str(self.repo), at=self.root)
        self.git('config', 'uploadpack.allowFilter', 'true')
        self.git('config', 'uploadpack.allowAnySHA1InWant', 'true')
        self.git('config', 'http.uploadpack', 'true')
        self.git('config', 'user.name', 'Disposable Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.blob = self.git('hash-object', '-w', '--stdin', data=b'expected\n')
        self.other = self.git('hash-object', '-w', '--stdin', data=b'unrelated payload\n')
        self.ignore = self.git('hash-object', '-w', '--stdin', data=b'out/\n')
        self.subtree = self.git('mktree', data=f'100644 blob {self.blob}\tfile\n'.encode())
        self.tree = self.git('mktree', data=f'100644 blob {self.ignore}\t.gitignore\n040000 tree {self.subtree}\tdeep\n100644 blob {self.other}\tother\n'.encode())
        self.commit = self.git('commit-tree', self.tree, data=b'fixture\n')
        self.git('update-ref', 'refs/heads/main', self.commit)
        self.git('symbolic-ref', 'HEAD', 'refs/heads/main')
        self.requests = []
        self.denied_oids = set()
        self.deny_auth = False
        self.stalled_oids = set()
        self.stalled = threading.Event()
        self.resume = threading.Event()
        self.children = []
        self.handles = []
    def git(self, *args, data=None, at=None):
        return run(['git', '-C', at or self.repo, *args], input=data, env=self.env).decode().strip()
    def close(self):
        self.resume.set()
        if hasattr(self,'server'): self.server.server_close()
        for process in reversed(self.children):
            if process.poll() is None:
                process.terminate()
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=5)
        for handle in self.handles: handle.close()
        self.temp.cleanup()
    def https(self):
        key, cert = self.root/'tls.key', self.root/'tls.crt'
        run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-subj', '/CN=localhost', '-addext', 'subjectAltName=IP:127.0.0.1', '-keyout', key, '-out', cert])
        backend = Path(run(['git', '--exec-path']).decode().strip()) / 'git-http-backend'
        fixture = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self): self.handle_git()
            def do_POST(self): self.handle_git()
            def log_message(self, *_): pass
            def handle_git(self):
                path, _, query = self.path.partition('?')
                data = self.rfile.read(int(self.headers.get('Content-Length', '0')))
                wants = [x.decode() for x in re.findall(rb'want ([0-9a-f]{40})', data)]
                stalled_request = bool(fixture.stalled_oids.intersection(wants))
                if stalled_request:
                    fixture.stalled.set()
                    if not fixture.resume.wait(timeout=30):
                        self.send_error(504,'controlled transport stall timed out'); return
                if fixture.deny_auth or fixture.denied_oids.intersection(wants):
                    status = 401 if fixture.deny_auth else 403
                    self.send_response(status); self.send_header('Content-Length', '0'); self.end_headers()
                    fixture.requests.append(dict(path=path, request_bytes=len(data), response_bytes=0, wants=wants, status=status))
                    return
                env = dict(fixture.env, GIT_PROJECT_ROOT=str(fixture.root), GIT_HTTP_EXPORT_ALL='1', PATH_INFO=path, QUERY_STRING=query, REQUEST_METHOD=self.command, CONTENT_TYPE=self.headers.get('Content-Type', ''), CONTENT_LENGTH=str(len(data)), SERVER_PROTOCOL='HTTP/1.1', REMOTE_ADDR='127.0.0.1')
                if self.headers.get('Git-Protocol'): env['HTTP_GIT_PROTOCOL'] = self.headers['Git-Protocol']
                # upload-pack can exit nonzero after sending a valid protocol
                # error. Preserve its CGI response as a native server fault.
                output = subprocess.run([backend], input=data, env=env, capture_output=True).stdout
                headers, sep, body = output.partition(b'\r\n\r\n')
                if not sep: raise RuntimeError('missing CGI headers')
                status = 200
                fields = []
                for line in headers.split(b'\r\n'):
                    name, _, value = line.partition(b':')
                    if name.lower() == b'status': status = int(value.strip().split()[0])
                    else: fields.append((name.decode(), value.strip().decode()))
                try:
                    self.send_response(status)
                    for name, value in fields: self.send_header(name, value)
                    self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLEOFError):
                    # A cancellation regression deliberately closes the client
                    # before the held server request can send its response.
                    if not stalled_request: raise
                    return
                fixture.requests.append(dict(path=path, request_bytes=len(data), response_bytes=len(body), wants=[x.decode() for x in re.findall(rb'want ([0-9a-f]{40})', data)], headers=self.headers.get_all('X-Git-On-Fixture', [])))
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); context.load_cert_chain(cert, key)
        server.socket = context.wrap_socket(server.socket, server_side=True)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        self.server = server
        self.env['GIT_SSL_CAINFO'] = str(cert)
        return f'https://127.0.0.1:{server.server_port}/server.git'
    def ssh(self, encrypted=False, protocol_v2=False):
        sshd = shutil.which('sshd') or '/usr/sbin/sshd'
        if not Path(sshd).exists(): raise RuntimeError('OpenSSH server missing; install openssh-server')
        host, identity = self.root/'host', self.root/'identity'
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', host])
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', 'fixture-password' if encrypted else '', '-f', identity])
        authorized = self.root/'authorized'; authorized.write_bytes((self.root/'identity.pub').read_bytes())
        with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port = sock.getsockname()[1]
        config = self.root/'sshd_config'
        config.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nPidFile {self.root}/sshd.pid\nAuthorizedKeysFile {authorized}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nLogLevel VERBOSE\n')
        if protocol_v2:
            with config.open('a') as output:output.write('AcceptEnv GIT_PROTOCOL\n')
        errors = (self.root/'sshd.log').open('w+b'); self.handles.append(errors)
        server = subprocess.Popen([sshd, '-D', '-e', '-f', str(config)], stderr=errors)
        self.children.append(server); self.ssh_server = server
        deadline = time.monotonic()+5
        while time.monotonic()<deadline:
            if server.poll() is not None:
                errors.seek(0); raise RuntimeError('OpenSSH server failed: '+errors.read().decode())
            try:
                with socket.create_connection(('127.0.0.1',port),timeout=.1): break
            except OSError: time.sleep(.02)
        else: raise RuntimeError('OpenSSH server did not listen')
        known = self.root/'known_hosts'
        known.write_text(f'[127.0.0.1]:{port} '+(self.root/'host.pub').read_text())
        command = f'ssh -F /dev/null -i {identity} -o IdentitiesOnly=yes -o UserKnownHostsFile={known} -o StrictHostKeyChecking=yes'
        self.env['GIT_SSH_COMMAND'] = command
        # The askpass fixture counts OpenSSH invocations and never uses real keys.
        askpass = self.root/'askpass'
        askpass.write_text(f'#!/bin/sh\nprintf "prompt\\n" >> "{self.root}/prompts"\nprintf "fixture-password\\n"\n')
        askpass.chmod(0o700)
        self.env.update(SSH_ASKPASS=str(askpass), SSH_ASKPASS_REQUIRE='force', DISPLAY='fixture:0')
        return f'ssh://{pwd.getpwuid(os.getuid()).pw_name}@127.0.0.1:{port}{self.repo}'
    def probe(self, source, revision='HEAD', verify=False):
        process = subprocess.Popen([str(OPTIONS.probe.resolve()), source, revision, *(['--verify'] if verify else [])], cwd=self.cwd, env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.children.append(process)
        return process
    def line(self, process):
        ready, _, _ = select.select([process.stdout], [], [], 20)
        if not ready: raise AssertionError('probe output timed out')
        line = process.stdout.readline()
        if not line:
            server_log=(self.root/'sshd.log').read_text() if (self.root/'sshd.log').exists() else ''
            raise AssertionError('probe failed: '+process.stderr.read().decode()+'\n'+server_log)
        return json.loads(line)
    def operation(self, process, text):
        process.stdin.write((text+'\n').encode()); process.stdin.flush()
        value = self.line(process); report = self.line(process)
        return value, report
    def finish(self, process):
        process.stdin.write(b'exit\n'); process.stdin.flush()
        _, errors = process.communicate(timeout=20)
        if process.returncode: raise AssertionError(errors.decode())

class RemoteDiagnosticsTests(unittest.TestCase):
    """Controlled native-tool failures exercise diagnostics without network access."""
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='git-on-diagnostics-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tools = self.root / 'tools'; self.tools.mkdir()
        self.native_git = shutil.which('git')
        self.env = {k:v for k,v in os.environ.items() if not k.startswith(('GIT_', 'SSH_'))}
        self.env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1',
                        XDG_CACHE_HOME=str(self.root / 'cache'), TMPDIR=str(self.root),
                        LC_ALL='C')

    def native_version(self, version, ssh_failure=False):
        wrapper = self.tools / 'git'
        wrapper.write_text(f'''#!{sys.executable}
import json, os, sys
args = sys.argv[1:]
with open({str(self.root / 'native-requests')!r}, 'a') as output:
    output.write(json.dumps(args) + '\\n')
if args == ['--version']:
    print('git version ' + {version!r}); sys.exit(0)
if {ssh_failure!r} and 'ls-remote' in args and '--get-url' not in args:
    sys.stderr.write('git@github.com: Permission denied (publickey).\\n'); sys.exit(128)
os.execv({self.native_git!r}, [{self.native_git!r}, *args])
''')
        wrapper.chmod(0o700)
        self.env['PATH'] = str(self.tools) + os.pathsep + os.environ['PATH']

    def failure(self, source='https://github.com/owner/repository.git'):
        result = subprocess.run([str(OPTIONS.probe), source, 'HEAD'], cwd=self.root,
                                env=self.env, capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(list(self.root.glob('git-on-*')), [])
        return result.stderr.decode()

    def test_missing_native_git_names_requirement_and_safe_next_step(self):
        self.env['PATH'] = str(self.tools)
        error = self.failure()
        for detail in ('native Git', 'not found on PATH', '2.45.1', 'install'):
            self.assertIn(detail, error)

    def test_old_native_git_reports_detected_version_without_weakening_requirement(self):
        self.native_version('2.43.0')
        error = self.failure()
        for detail in ('2.43.0', '2.45.1', 'upgrade', 'PATH'):
            self.assertIn(detail, error)

    def test_local_verification_without_native_git_has_safe_install_advice(self):
        repository = self.root / 'repository'
        run([self.native_git, 'init', '--quiet', str(repository)], env=self.env)
        run([self.native_git, '-C', str(repository), '-c', 'user.name=Fixture',
             '-c', 'user.email=fixture@example.invalid', 'commit', '--quiet',
             '--allow-empty', '-m', 'verification fixture'], env=self.env)
        self.env['PATH'] = str(self.tools)
        result = subprocess.run([str(OPTIONS.probe), 'local', 'HEAD', '--verify'],
                                cwd=repository, env=self.env, capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        for detail in ('native Git', 'not found on PATH', 'revision resolution', 'install'):
            self.assertIn(detail, result.stderr.decode())
        self.assertEqual(list(self.root.glob('git-on-*')), [])

    def test_github_publickey_failure_offers_conditional_https_without_retry(self):
        for source in ('git@github.com:owner/repository.git',
                       'ssh://git@github.com/owner/repository.git'):
            with self.subTest(source=source):
                self.native_version('2.45.1', ssh_failure=True)
                (self.root / 'native-requests').unlink(missing_ok=True)
                error = self.failure(source)
                for detail in ('publickey', 'if this GitHub repository is public',
                               'https://github.com/owner/repository.git',
                               'private', 'credentials'):
                    self.assertIn(detail, error)
                requests = (self.root / 'native-requests').read_text()
                self.assertNotIn('https://github.com/owner/repository.git', requests)


class RemoteTests(unittest.TestCase):
    def setUp(self):
        self.f = Fixture(); self.addCleanup(self.f.close)
    def fetched_oid(self, source, revision):
        """Native fetch is the oracle; the caller repository is never its target."""
        f=self.f
        with tempfile.TemporaryDirectory(dir=f.root,prefix='fetch-oracle-') as directory:
            f.git('init','--bare','--quiet',directory,at=f.root)
            result=subprocess.run(['git','-C',directory,'fetch','--no-tags',
                                   '--filter=tree:0','--refmap=','--',source,revision],
                                  env=f.env,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,0,result.stderr)
            rows=(Path(directory)/'FETCH_HEAD').read_bytes().splitlines()
            selected=[row.split(b'\t',1)[0].decode() for row in rows
                      if b'\tnot-for-merge\t' not in row]
            self.assertEqual(len(selected),1,rows)
            return selected[0]
    def test_fetch_accepts_native_short_ref_names_and_refspecs(self):
        f=self.f; source='file://'+str(f.repo)
        f.git('update-ref','refs/tags/release',f.commit)
        for revision in ('heads/main','tags/release','+main','',
                         'main:refs/heads/private-copy','main:deep'):
            with self.subTest(revision=revision):
                self.assertEqual(self.fetched_oid(source,revision),f.commit)
                process=f.probe(source,revision)
                initial=f.line(process)
                self.assertEqual(initial['tree'],f.tree)
                self.assertEqual(set(initial['objects']),{f.commit,f.tree})
                value,read=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n')
                self.assertNotIn(f.other,read['objects'])
                f.finish(process)
    def test_fetch_accepts_relative_absolute_and_non_utf8_repository_paths(self):
        f=self.f
        weird=f.root/os.fsdecode(b'server-\xff.git')
        weird.symlink_to(f.repo,target_is_directory=True)
        for source in (str(f.repo),'../server.git',str(weird)):
            with self.subTest(source=source):
                process=f.probe(source,'main')
                self.assertEqual(f.line(process)['tree'],f.tree)
                value,_=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n'); f.finish(process)
    def test_fetch_branch_tag_collision_follows_native_precedence(self):
        f=self.f; source='file://'+str(f.repo)
        other_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        other=f.git('commit-tree',other_tree,data=b'tag collision fixture\n')
        f.git('update-ref','refs/tags/main',other)
        for revision in ('main','heads/main','tags/main'):
            with self.subTest(revision=revision):
                oid=self.fetched_oid(source,revision)
                process=f.probe(source,revision)
                self.assertEqual(f.line(process)['tree'],f.git('rev-parse',oid+'^{tree}'))
                f.finish(process)
    def test_fetch_colon_is_a_literal_refspec_and_does_not_touch_caller_refs(self):
        f=self.f; source='file://'+str(f.repo)
        f.cwd=f.repo
        before=f.git('for-each-ref','--format=%(refname) %(objectname)')
        process=f.probe(source,'main:refs/heads/private-copy')
        self.assertEqual(f.line(process)['tree'],f.tree)
        f.finish(process)
        self.assertEqual(f.git('for-each-ref','--format=%(refname) %(objectname)'),before)
    def test_native_fetch_reference_hook_sees_private_refs_and_repository(self):
        f=self.f; f.cwd=f.repo; hooks=f.root/'hooks'; hooks.mkdir(); log=f.root/'hook-log'
        hook=hooks/'reference-transaction'
        hook.write_text(f'''#!{sys.executable}
import json, os, sys
with open({str(log)!r}, 'a') as output:
    output.write(json.dumps(dict(phase=sys.argv[1], git_dir=os.environ.get('GIT_DIR'),
                                cwd=os.getcwd(), changes=sys.stdin.read())) + '\\n')
''')
        hook.chmod(0o700)
        f.git('config','core.hooksPath',str(hooks))
        before=f.git('for-each-ref','--format=%(refname) %(objectname)')
        process=f.probe('file://'+str(f.repo),'main:refs/heads/private-copy')
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n'); f.finish(process)
        self.assertEqual(f.git('for-each-ref','--format=%(refname) %(objectname)'),before)
        transactions=[json.loads(line) for line in log.read_text().splitlines()]
        selected=[entry for entry in transactions if 'refs/heads/private-copy' in entry['changes']]
        self.assertTrue(selected,transactions)
        self.assertTrue({'prepared','committed'}.issubset({entry['phase'] for entry in selected}))
        for entry in selected:
            git_dir=Path(entry['git_dir'])
            if not git_dir.is_absolute(): git_dir=Path(entry['cwd'])/git_dir
            self.assertNotEqual(git_dir.resolve(),f.repo.resolve())
            self.assertEqual(git_dir.name,'remote-repository')
            self.assertFalse(git_dir.exists(),'private hook target survived completed run')
    def test_fetch_invalid_revision_reports_native_git_error(self):
        f=self.f; source='file://'+str(f.repo)
        for revision in ('missing-branch','main:bad..destination'):
            with self.subTest(revision=revision):
                process=f.probe(source,revision)
                _,errors=process.communicate(timeout=20)
                self.assertNotEqual(process.returncode,0)
                self.assertTrue(b"couldn't find remote ref" in errors or b'invalid refspec' in errors,errors)
                self.assertNotIn(b'not advertised',errors)
    def test_fetch_non_utf8_ref_is_passed_to_git_unchanged(self):
        f=self.f; revision=os.fsdecode(b'branch-\xff')
        f.git('update-ref','refs/heads/'+revision,f.commit)
        process=f.probe('file://'+str(f.repo),revision)
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n'); f.finish(process)
    def test_native_fetch_receives_the_original_source_and_refspec(self):
        f=self.f; native=shutil.which('git'); tools=f.root/'logged-tools'; tools.mkdir()
        log=f.root/'git-argv'; wrapper=tools/'git'
        wrapper.write_text(f'''#!{sys.executable}
import json, os, sys
with open({str(log)!r}, 'a') as output:
    output.write(json.dumps(sys.argv[1:]) + '\\n')
os.execv({native!r}, [{native!r}, *sys.argv[1:]])
''')
        wrapper.chmod(0o700)
        config=f.root/'rewrite-config'
        config.write_text(f'[url "file://{f.root}/"]\ninsteadOf = fixture:\n')
        f.env.update(GIT_CONFIG_GLOBAL=str(config),PATH=str(tools)+os.pathsep+f.env['PATH'])
        source='fixture:server.git'; revision='+heads/main:refs/heads/private-copy'
        process=f.probe(source,revision)
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n'); f.finish(process)
        calls=[json.loads(line) for line in log.read_text().splitlines()]
        self.assertTrue(any('fetch' in call and call[-3:]==['--',source,revision]
                            for call in calls),calls)
    def test_fetch_multiple_selected_objects_and_blob_never_execute_command(self):
        f=self.f; source='file://'+str(f.repo)
        other_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        other=f.git('commit-tree',other_tree,data=b'second fetch target\n')
        f.git('update-ref','refs/heads/other',other)
        sentinel=f.root/'executed'
        for revision in ('refs/heads/*:refs/remotes/private/*',f.blob):
            with self.subTest(revision=revision):
                result=subprocess.run([str(OPTIONS.binary),source,revision,'--',
                                       sys.executable,'-c',
                                       f'from pathlib import Path; Path({str(sentinel)!r}).touch()'],
                                      cwd=f.cwd,env=f.env,capture_output=True,timeout=20)
                self.assertEqual(result.returncode,125,result.stderr)
                self.assertNotIn(b'Usage:',result.stderr)
                self.assertFalse(sentinel.exists())
                self.assertEqual(list(f.root.glob('git-on-*')),[])
    def test_fixture_exec_failure_exits_without_reentering_test_runner(self):
        f=self.f; pid,fd=pty.fork(); reaped=False; transcript=bytearray()
        if pid==0:
            exec_fixture_child([str(f.root/'missing-fixture-executable')],f.cwd,f.env)
            os._exit(99)
        try:
            deadline=time.monotonic()+5
            while time.monotonic()<deadline:
                ready,_,_=select.select([fd],[],[],.05)
                if ready:
                    with contextlib.suppress(OSError): transcript.extend(os.read(fd,65536))
                done,status=os.waitpid(pid,os.WNOHANG)
                if done:
                    reaped=True; break
            self.assertTrue(reaped,'failed fixture exec did not exit')
            # Exit can race the select timeout; after reap, drain any diagnostic
            # already queued in the PTY before checking it.
            while select.select([fd],[],[],0)[0]:
                try: chunk=os.read(fd,65536)
                except OSError: break
                if not chunk: break
                transcript.extend(chunk)
            self.assertEqual(os.waitstatus_to_exitcode(status),127,transcript)
            self.assertIn(b'fixture exec failed:',transcript)
            self.assertNotIn(b'Traceback',transcript)
        finally:
            if not reaped:
                with contextlib.suppress(ProcessLookupError): os.kill(pid,signal.SIGKILL)
                with contextlib.suppress(ChildProcessError): os.waitpid(pid,0)
            os.close(fd)
    def test_review_fixture_binary_stdout_is_preserved(self):
        payload=b' \t\r\n\v\f\x00\xffbinary\x80\x00 \t\r\n\v\f'
        actual=run([sys.executable,'-c','import sys; sys.stdout.buffer.write(sys.stdin.buffer.read())'],input=payload)
        self.assertEqual(actual,payload)
    def test_review_explicit_remote_refs_do_not_expand_as_short_names(self):
        f=self.f
        other_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        other=f.git('commit-tree',other_tree,data=b'collision fixture\n')
        for name in ('refs/heads/HEAD','refs/tags/HEAD','refs/heads/refs/heads/main','refs/tags/refs/heads/main'):
            f.git('update-ref',name,other)
        url=f.https(); self.addCleanup(f.server.shutdown)
        advertisement=run(['git','ls-remote','--symref','--',url],env=f.env)
        for revision in ('HEAD','refs/heads/main'):
            with self.subTest(revision=revision):
                self.assertIn(f'{f.commit}\t{revision}\n'.encode(),advertisement)
                process=f.probe(url,revision)
                self.assertEqual(f.line(process)['tree'],f.tree)
                value,_=f.operation(process,'read deep/file'); self.assertEqual(value['data'],'expected\n')
                f.finish(process)
    def test_review_unrelated_non_utf8_remote_ref_preserves_ascii_selection(self):
        f=self.f; name=b'refs/heads/unrelated-\xff'
        f.git('update-ref',os.fsdecode(name),f.commit)
        url=f.https(); self.addCleanup(f.server.shutdown)
        advertisement=run(['git','ls-remote','--symref','--',url],env=f.env)
        self.assertIn(f.commit.encode()+b'\t'+name+b'\n',advertisement)
        for revision in ('HEAD','refs/heads/main'):
            with self.subTest(revision=revision):
                process=f.probe(url,revision)
                self.assertEqual(f.line(process)['tree'],f.tree)
                value,_=f.operation(process,'read deep/file'); self.assertEqual(value['data'],'expected\n')
                f.finish(process)
    def test_local_unverified_loader_resolves_native_git_revisions(self):
        f=self.f; f.cwd=f.repo
        f.git('tag','-a','-m','local annotated tag','local-tag',f.commit)
        # A replacement ref must not redirect the exact libgit2 object reads.
        other_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        replacement=f.git('commit-tree',other_tree,data=b'replacement fixture\n')
        f.git('update-ref','refs/replace/'+f.commit,replacement)
        child=f.git('commit-tree',f.tree,'-p',f.commit,data=b'local ancestry fixture\n')
        f.git('update-ref','refs/heads/main',child)
        f.git('update-ref','refs/replace/'+child,replacement)
        nested=f.commit; kind='commit'
        for depth in range(70):
            nested=f.git('hash-object','-t','tag','-w','--stdin',data=f'object {nested}\ntype {kind}\ntag local-depth-{depth}\ntagger Fixture <fixture@example.invalid> 0 +0000\n\nlegacy local tag\n'.encode())
            kind='tag'
        revisions=(('HEAD',f.tree,'deep/file'),('HEAD:deep',f.subtree,'file'),
                   ('HEAD^',f.tree,'deep/file'),('HEAD~1',f.tree,'deep/file'),
                   (f.tree,f.tree,'deep/file'),('local-tag',f.tree,'deep/file'),
                   (nested,f.tree,'deep/file'))
        for revision,tree,path in revisions:
            with self.subTest(revision=revision):
                process=f.probe('local',revision)
                self.assertEqual(f.line(process)['tree'],tree)
                value,_=f.operation(process,'read '+path); self.assertEqual(value['data'],'expected\n')
                f.finish(process)
    def test_local_abbreviations_and_non_utf8_tree_paths_use_native_git(self):
        f=self.f; f.cwd=f.repo
        name=b'deep-\xff'
        tree=f.git('mktree',data=b'040000 tree '+f.subtree.encode()+b'\t'+name+b'\n')
        commit=f.git('commit-tree',tree,data=b'non-UTF8 local path\n')
        f.git('update-ref','refs/heads/main',commit)
        for revision,expected,path in ((commit[:12],tree,'deep-'+os.fsdecode(b'\xff')+'/file'),
                                       (os.fsdecode(b'HEAD:'+name),f.subtree,'file')):
            with self.subTest(revision=revision):
                resolved=f.git('rev-parse','--verify',revision)
                native=f.git('rev-parse','--verify',resolved+'^{tree}')
                self.assertEqual(native,expected)
                process=f.probe('local',revision)
                self.assertEqual(f.line(process)['tree'],expected)
                # The probe's text-operation protocol is UTF-8; select the
                # non-UTF8 subtree via Git, then read its ordinary child.
                if path=='file':
                    value,_=f.operation(process,'read file')
                    self.assertEqual(value['data'],'expected\n')
                f.finish(process)
    def test_local_native_object_database_environment_is_used_for_exact_reads(self):
        f=self.f; f.cwd=f.repo; original=dict(f.env)
        objects=f.root/'external-objects'; objects.mkdir()
        writer=dict(original,GIT_OBJECT_DIRECTORY=str(objects))
        def write(*args,data):
            return run(['git','-C',f.repo,*args],input=data,env=writer).decode().strip()
        blob=write('hash-object','-w','--stdin',data=b'external object database\n')
        tree=write('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
        commit=write('commit-tree',tree,data=b'external ODB fixture\n')
        self.assertNotEqual(commit,f.commit)
        for variable in ('GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES'):
            with self.subTest(variable=variable):
                f.env=dict(original,**{variable:str(objects)})
                self.assertEqual(f.git('rev-parse','--verify',commit+'^{tree}'),tree)
                process=f.probe('local',commit)
                self.assertEqual(f.line(process)['tree'],tree)
                value,_=f.operation(process,'read file')
                self.assertEqual(value['data'],'external object database\n'); f.finish(process)
                if OPTIONS.require_fuse:
                    result=subprocess.run([str(OPTIONS.binary),commit,'--','cat','file'],
                                          cwd=f.cwd,env=f.env,capture_output=True,timeout=20)
                    self.assertEqual(result.returncode,0,result.stderr)
                    self.assertEqual(result.stdout,b'external object database\n')
                    self.assertEqual(list(f.root.glob('git-on-*')),[])
    def test_local_namespace_environment_does_not_leave_snapshot_pins(self):
        f=self.f; f.cwd=f.repo; unscoped=dict(f.env)
        f.env['GIT_NAMESPACE']='snapshot-input-namespace'
        def refs():
            return run(['git','-C',f.repo,'for-each-ref','--format=%(refname) %(objectname)'],
                       env=unscoped)
        before=refs()
        self.assertEqual(f.git('rev-parse','--verify','HEAD'),f.commit)
        process=f.probe('local','HEAD')
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n'); f.finish(process)
        self.assertEqual(refs(),before)
        if OPTIONS.require_fuse:
            result=subprocess.run([str(OPTIONS.binary),'HEAD','--','cat','deep/file'],
                                  cwd=f.cwd,env=f.env,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual(result.stdout,b'expected\n')
            self.assertEqual(refs(),before)
            self.assertEqual(list(f.root.glob('git-on-*')),[])
    def test_local_common_directory_reads_and_releases_pins_in_the_native_repository(self):
        f=self.f; f.cwd=f.repo; unscoped=dict(f.env); common=f.root/'common.git'
        run(['git','clone','--bare','--no-local','--quiet',f.repo,common],env=f.env)
        f.git('config','user.name','Common Repository',at=common)
        f.git('config','user.email','common@example.invalid',at=common)
        blob=f.git('hash-object','-w','--stdin',data=b'common repository\n',at=common)
        tree=f.git('mktree',data=f'100644 blob {blob}\tfile\n'.encode(),at=common)
        commit=f.git('commit-tree',tree,data=b'common directory fixture\n',at=common)
        f.git('update-ref','refs/heads/main',commit,at=common)
        def refs(repository):
            return run(['git','-C',repository,'for-each-ref','--format=%(refname) %(objectname)'],
                       env=unscoped)
        before={repository:refs(repository) for repository in (f.repo,common)}
        f.env.update(GIT_DIR=str(f.repo),GIT_COMMON_DIR=str(common))
        self.assertEqual(f.git('rev-parse','--verify',commit+'^{tree}'),tree)
        process=f.probe('local',commit)
        self.assertEqual(f.line(process)['tree'],tree)
        value,_=f.operation(process,'read file')
        self.assertEqual(value['data'],'common repository\n'); f.finish(process)
        for repository in before: self.assertEqual(refs(repository),before[repository])
        if OPTIONS.require_fuse:
            result=subprocess.run([str(OPTIONS.binary),commit,'--','cat','file'],
                                  cwd=f.cwd,env=f.env,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual(result.stdout,b'common repository\n')
            for repository in before: self.assertEqual(refs(repository),before[repository])
            ready=f.root/'common-ready'
            code=f'import os,time; from pathlib import Path; assert Path("file").read_text() == "common repository\\n"; Path({str(ready)!r}).write_text(str(os.getpid())); time.sleep(30)'
            process=subprocess.Popen([str(OPTIONS.binary),commit,'--',sys.executable,'-c',code],
                                     cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            f.children.append(process)
            deadline=time.monotonic()+10
            while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
            self.assertTrue(ready.exists(),'common-directory snapshot did not execute its command')
            child=int(ready.read_text()); runs=list(f.root.glob('git-on-*'))
            self.assertEqual(len(runs),1)
            process.kill(); process.wait(timeout=5)
            with contextlib.suppress(ProcessLookupError): os.kill(child,signal.SIGTERM)
            process.communicate(timeout=5)
            cleanup=subprocess.run([str(OPTIONS.binary),'--cleanup',str(runs[0])],
                                   cwd=f.cwd,env=unscoped,capture_output=True,timeout=15)
            self.assertEqual(cleanup.returncode,0,cleanup.stderr)
            self.assertFalse(runs[0].exists())
            for repository in before: self.assertEqual(refs(repository),before[repository])
    def test_local_c_quoted_alternate_with_colon_and_non_utf8_path_matches_native_git(self):
        f=self.f; f.cwd=f.repo; objects=f.root/os.fsdecode(b'alternate:objects-\xff'); objects.mkdir()
        writer=dict(f.env,GIT_OBJECT_DIRECTORY=str(objects))
        def write(*args,data):
            return run(['git','-C',f.repo,*args],input=data,env=writer).decode().strip()
        blob=write('hash-object','-w','--stdin',data=b'quoted alternate\n')
        tree=write('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
        commit=write('commit-tree',tree,data=b'quoted alternate fixture\n')
        quoted='"'+''.join('\\%03o'%byte if byte<32 or byte>=127 else
                           '\\'+chr(byte) if byte in (34,92) else chr(byte)
                           for byte in os.fsencode(objects))+'"'
        f.env['GIT_ALTERNATE_OBJECT_DIRECTORIES']=quoted
        self.assertEqual(f.git('rev-parse','--verify',commit+'^{tree}'),tree)
        process=f.probe('local',commit)
        self.assertEqual(f.line(process)['tree'],tree)
        value,_=f.operation(process,'read file')
        self.assertEqual(value['data'],'quoted alternate\n'); f.finish(process)
        if OPTIONS.require_fuse:
            result=subprocess.run([str(OPTIONS.binary),commit,'--','cat','file'],
                                  cwd=f.cwd,env=f.env,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual(result.stdout,b'quoted alternate\n')
            self.assertEqual(list(f.root.glob('git-on-*')),[])
    def test_local_hash_prefixed_alternate_components_match_native_git(self):
        if not OPTIONS.require_fuse: self.skipTest('actual FUSE execution explicitly required')
        f=self.f; f.cwd=f.repo; original=dict(f.env)
        objects=f.repo/'#alternate-objects'; objects.mkdir()
        writer=dict(original,GIT_OBJECT_DIRECTORY=str(objects))
        def write(*args,data):
            return run(['git','-C',f.repo,*args],input=data,env=writer).decode().strip()
        payload=b'literal hash alternate\n'
        blob=write('hash-object','-w','--stdin',data=payload)
        tree=write('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
        commit=write('commit-tree',tree,data=b'hash-prefixed alternate fixture\n')
        def object_files(directory):
            return {str(path.relative_to(directory)):path.read_bytes()
                    for path in directory.rglob('*') if path.is_file()}
        before_refs=run(['git','-C',f.repo,'for-each-ref'],env=original)
        before_objects={directory:object_files(directory) for directory in (f.repo/'objects',objects)}
        marker=f.root/'command-output'
        code='from pathlib import Path; import sys; data=Path("file").read_bytes(); Path(sys.argv[1]).write_bytes(data); sys.stdout.buffer.write(data)'
        cases=(('unquoted','#alternate-objects',False),
               ('quoted','"#alternate-objects"',True),
               ('empty-components',':#alternate-objects::',False),
               ('quoted-with-empty-components',':"#alternate-objects"::',True),
               ('comment-then-quoted','#ignored:"#alternate-objects"',True))
        for name,value,accepted in cases:
            with self.subTest(component=name):
                env=dict(original,GIT_ALTERNATE_OBJECT_DIRECTORIES=value)
                native_tree=subprocess.run(['git','-C',f.cwd,'rev-parse','--verify',commit+'^{tree}'],
                                           env=env,capture_output=True,timeout=10)
                native_file=subprocess.run(['git','-C',f.cwd,'show',commit+':file'],
                                           env=env,capture_output=True,timeout=10)
                self.assertEqual(native_tree.returncode==0,accepted,native_tree.stderr)
                self.assertEqual(native_file.returncode==0,accepted,native_file.stderr)
                if accepted:
                    self.assertEqual(native_tree.stdout.decode().strip(),tree)
                    self.assertEqual(native_file.stdout,payload)
                marker.unlink(missing_ok=True)
                result=subprocess.run([str(OPTIONS.binary),commit,'--',sys.executable,'-c',code,str(marker)],
                                      cwd=f.cwd,env=env,capture_output=True,timeout=20)
                self.assertEqual(run(['git','-C',f.repo,'for-each-ref'],env=original),before_refs)
                for directory in before_objects:
                    self.assertEqual(object_files(directory),before_objects[directory])
                self.assertEqual(list(f.root.glob('git-on-*')),[])
                if accepted:
                    self.assertEqual(result.returncode,0,result.stderr)
                    self.assertEqual(result.stdout,payload)
                    self.assertEqual(marker.read_bytes(),payload)
                else:
                    self.assertEqual(result.returncode,125,result.stderr)
                    self.assertEqual(result.stdout,b'')
                    self.assertFalse(marker.exists(),'rejected native object environment executed its command')
    def test_local_relative_object_environments_follow_native_worktree_setup(self):
        f=self.f; worktree=f.root/'worktree'
        run(['git','init','--quiet',worktree],env=f.env)
        subdir=worktree/'nested'; subdir.mkdir()
        outside=f.root/'explicit-context'; outside.mkdir()
        for mode in ('object-directory','alternate','quoted-alternate','mixed-alternates','explicit-worktree'):
            with self.subTest(mode=mode):
                objects=(outside if mode=='explicit-worktree' else f.root)/('relative-'+mode)
                objects.mkdir()
                if mode=='object-directory':
                    # Discovery first sees the relative location from the
                    # invocation directory; object reads use Git's setup cwd.
                    (worktree/objects.name).mkdir()
                writer=dict(f.env,GIT_OBJECT_DIRECTORY=str(objects))
                def write(*args,data):
                    return run(['git','-C',f.repo,*args],input=data,env=writer).decode().strip()
                payload=('native relative '+mode+'\n').encode()
                blob=write('hash-object','-w','--stdin',data=payload)
                tree=write('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
                commit=write('commit-tree',tree,data=b'relative object environment\n')
                relative=objects.name if mode=='explicit-worktree' else '../'+objects.name
                env=dict(f.env)
                if mode=='object-directory': env['GIT_OBJECT_DIRECTORY']=relative
                elif mode=='alternate': env['GIT_ALTERNATE_OBJECT_DIRECTORIES']=relative
                elif mode=='mixed-alternates':
                    env['GIT_ALTERNATE_OBJECT_DIRECTORIES']='missing:"'+relative+'":missing-again'
                else: env['GIT_ALTERNATE_OBJECT_DIRECTORIES']='"'+relative+'"'
                cwd=outside if mode=='explicit-worktree' else subdir
                if mode=='explicit-worktree':
                    env.update(GIT_DIR=str(worktree/'.git'),GIT_WORK_TREE=str(worktree))
                native=run(['git','-C',cwd,'rev-parse','--verify','--end-of-options',commit+'^{tree}'],env=env).decode().strip()
                self.assertEqual(native,tree)
                before=run(['git','-C',worktree,'for-each-ref'],env=f.env)
                previous_cwd,previous_env=f.cwd,f.env
                try:
                    f.cwd,f.env=cwd,env
                    process=f.probe('local',commit)
                    self.assertEqual(f.line(process)['tree'],tree)
                    value,_=f.operation(process,'read file')
                    self.assertEqual(value['data'].encode(),payload); f.finish(process)
                    if OPTIONS.require_fuse:
                        result=subprocess.run([str(OPTIONS.binary),commit,'--','cat','file'],
                                              cwd=cwd,env=env,capture_output=True,timeout=20)
                        self.assertEqual(result.returncode,0,result.stderr)
                        self.assertEqual(result.stdout,payload)
                finally: f.cwd,f.env=previous_cwd,previous_env
                self.assertEqual(run(['git','-C',worktree,'for-each-ref'],env=f.env),before)
    def test_local_relative_alternates_keep_native_bare_invocation_context(self):
        f=self.f; objects=f.root/'bare-relative-objects'; objects.mkdir()
        writer=dict(f.env,GIT_OBJECT_DIRECTORY=str(objects))
        def write(*args,data):
            return run(['git','-C',f.repo,*args],input=data,env=writer).decode().strip()
        blob=write('hash-object','-w','--stdin',data=b'bare relative alternate\n')
        tree=write('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
        commit=write('commit-tree',tree,data=b'bare alternate environment\n')
        subdir=f.repo/'nested'; subdir.mkdir()
        for mode,cwd,relative in (('bare-root',f.repo,'../'+objects.name),
                                  ('bare-subdirectory',subdir,'../../'+objects.name),
                                  ('explicit-bare-directory',f.cwd,'../'+objects.name)):
            with self.subTest(mode=mode):
                env=dict(f.env,GIT_ALTERNATE_OBJECT_DIRECTORIES='"'+relative+'"')
                if mode=='explicit-bare-directory': env['GIT_DIR']=str(f.repo)
                self.assertEqual(run(['git','-C',cwd,'rev-parse','--verify',commit+'^{tree}'],env=env).decode().strip(),tree)
                before=run(['git','-C',f.repo,'for-each-ref'],env=f.env)
                previous_cwd,previous_env=f.cwd,f.env
                try:
                    f.cwd,f.env=cwd,env
                    process=f.probe('local',commit)
                    self.assertEqual(f.line(process)['tree'],tree)
                    value,_=f.operation(process,'read file')
                    self.assertEqual(value['data'],'bare relative alternate\n'); f.finish(process)
                    if OPTIONS.require_fuse:
                        result=subprocess.run([str(OPTIONS.binary),commit,'--','cat','file'],
                                              cwd=cwd,env=env,capture_output=True,timeout=20)
                        self.assertEqual(result.returncode,0,result.stderr)
                        self.assertEqual(result.stdout,b'bare relative alternate\n')
                finally: f.cwd,f.env=previous_cwd,previous_env
                self.assertEqual(run(['git','-C',f.repo,'for-each-ref'],env=f.env),before)
    def test_https_fresh_runs_lazy_objects_ref_race_and_local_equivalence(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        process=f.probe(url); start=f.line(process)
        self.assertEqual(set(start['objects']), {f.commit, f.tree})
        self.assertFalse((f.cwd/'.git').exists())
        value, listed=f.operation(process, 'list ')
        self.assertEqual(set(value['names']), {'.gitignore','deep','other'})
        self.assertEqual(set(listed['objects']), {f.commit,f.tree})
        moved_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        f.git('update-ref', 'refs/heads/main', f.git('commit-tree', moved_tree, data=b'moved\n'))
        value, read=f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        self.assertEqual(set(read['objects']), {f.commit,f.tree,f.subtree,f.blob})
        self.assertNotIn(f.other, read['objects']); f.finish(process)
        requests=len(f.requests)
        repeated=f.probe(url, f.commit); f.line(repeated); f.operation(repeated,'read deep/file'); f.finish(repeated)
        # Each invocation resolves its revision and fetches its demanded objects.
        new=f.requests[requests:]
        self.assertEqual({oid for request in new for oid in request['wants']},{f.commit,f.tree,f.subtree,f.blob})
        expected=run(['git','-C',f.repo,'show',f.commit+':deep/file'],env=f.env)
        self.assertEqual(expected,value['data'].encode())
        print('HTTPS request/byte evidence:', json.dumps(f.requests), flush=True)
    def test_review_configuration_values_are_applied_once(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        config=f.root/'global_config'; config.write_text('[http]\nextraHeader = X-Git-On-Fixture: once\n')
        f.env['GIT_CONFIG_GLOBAL']=str(config)
        process=f.probe(url); f.line(process); f.operation(process,'read deep/file'); f.finish(process)
        self.assertTrue(f.requests)
        self.assertTrue(all(x['headers']==['once'] for x in f.requests),f.requests)
    def test_review_url_rewrites_are_applied_once_for_urls_and_remote_names(self):
        f=self.f; config=f.root/'global_config'
        wrong=f.root/'does-not-exist'
        config.write_text(f'[url "file://{wrong}/"]\ninsteadOf = https://fixture.invalid/\n[url "file://{f.root}/"]\ninsteadOf = file://{wrong}/\n')
        f.env['GIT_CONFIG_GLOBAL']=str(config)
        for named in (False,True):
            if named:
                f.git('init','--quiet',str(f.cwd),at=f.root); f.git('remote','add','fixture','https://fixture.invalid/server.git',at=f.cwd)
            process=f.probe('fixture' if named else 'https://fixture.invalid/server.git')
            _,errors=process.communicate(timeout=20)
            self.assertNotEqual(process.returncode,0)
            self.assertIn(str(wrong).encode(),errors)
    def test_review_named_remote_preserves_uploadpack_configuration(self):
        f=self.f; f.cwd=f.repo
        f.git('remote','add','fixture','file://'+str(f.repo))
        f.git('config','remote.fixture.uploadpack','/nonexistent/git-on-uploadpack-fixture')
        process=f.probe('fixture'); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0)
        self.assertIn(b'/nonexistent/git-on-uploadpack-fixture',errors)
    def signing(self):
        f=self.f; key=f.root/'signature'
        run(['ssh-keygen','-q','-t','ed25519','-N','','-f',key])
        allowed=f.root/'allowed-signers'
        allowed.write_text('fixture@example.invalid '+(f.root/'signature.pub').read_text())
        f.env.update(GIT_CONFIG_COUNT='2',GIT_CONFIG_KEY_0='gpg.format',GIT_CONFIG_VALUE_0='ssh',GIT_CONFIG_KEY_1='gpg.ssh.allowedSignersFile',GIT_CONFIG_VALUE_1=str(allowed))
        commit=f.git('-c','gpg.format=ssh','-c','user.signingkey='+str(key),'commit-tree','-S',f.tree,data=b'signed fixture\n')
        f.git('-c','gpg.format=ssh','-c','user.signingkey='+str(key),'tag','-s','-m','signed tag','signed',commit)
        return commit,allowed
    def test_review_peeled_tag_advertisement_rows_are_not_selectable_refs(self):
        f=self.f; signed,_=self.signing()
        url=f.https(); self.addCleanup(f.server.shutdown)
        advertisement=run(['git','ls-remote','--symref','--',url],env=f.env)
        tag=f.git('rev-parse','refs/tags/signed')
        self.assertNotEqual(tag,signed)
        self.assertIn(f'{tag}\trefs/tags/signed\n'.encode(),advertisement)
        self.assertIn(f'{signed}\trefs/tags/signed^{{}}\n'.encode(),advertisement)
        # Both native signatures verify: silently accepting the helper would
        # otherwise succeed while verifying a different selected object.
        for kind,oid in (('tag',tag),('commit',signed)):
            oracle=subprocess.run(['git','-C',str(f.repo),'verify-'+kind,oid],env=f.env,capture_output=True,timeout=10)
            self.assertEqual(oracle.returncode,0,oracle.stderr)
        for verify in (False,True):
            with self.subTest(verify=verify):
                process=f.probe(url,'refs/tags/signed',verify=verify)
                self.assertEqual(f.line(process)['tree'],f.tree); f.finish(process)
                requests=len(f.requests)
                process=f.probe(url,'refs/tags/signed^{}',verify=verify)
                _,errors=process.communicate(timeout=20)
                self.assertNotEqual(process.returncode,0,'peeled advertisement helper accepted as an exact ref')
                self.assertIn(b'invalid refspec',errors)
                self.assertFalse(any(x['wants'] for x in f.requests[requests:]),'unsupported selector fetched objects')
    def test_native_ssh_signature_tag_refspec_replacements_and_failures(self):
        f=self.f; signed,allowed=self.signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        replacement_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        replacement=f.git('commit-tree',replacement_tree,data=b'different replacement\n')
        f.git('update-ref','refs/replace/'+signed,replacement)
        for revision in (signed,'signed','+refs/tags/signed',
                         'refs/tags/signed:refs/tags/private-copy'):
            process=f.probe(url,revision,verify=True); report=f.line(process)
            self.assertEqual(report['tree'],f.tree)
            value,_=f.operation(process,'read deep/file')
            self.assertEqual(value['data'],'expected\n'); f.finish(process)
        # Local verification uses the same effective inherited git -c settings.
        f.cwd=f.repo
        process=f.probe('local',signed,verify=True); f.line(process); f.operation(process,'read deep/file'); f.finish(process)
        # Native Git resolves REV:path to its tree, which cannot carry a
        # signature. Do not silently verify a different revision prefix.
        process=f.probe('local',signed+':deep',verify=True)
        _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'raw trees',errors)
        raw=subprocess.run(['git','-C',str(f.repo),'--no-replace-objects','cat-file','commit',signed],capture_output=True,check=True,env=f.env).stdout
        invalid=f.git('hash-object','-t','commit','-w','--stdin',data=raw.replace(b'signed fixture',b'altered fixture'))
        for revision in (invalid,f.commit,f.tree,f.blob):
            process=f.probe('local',revision,verify=True); _,errors=process.communicate(timeout=20)
            self.assertNotEqual(process.returncode,0,revision)
            self.assertTrue(b'execution forbidden' in errors or b'raw trees' in errors,errors)
        allowed.write_text('')
        process=f.probe('local',signed,verify=True); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'execution forbidden',errors)
    def test_local_verification_keeps_native_object_database_environment(self):
        f=self.f; self.signing(); original=dict(f.env); f.cwd=f.repo
        objects=f.root/'signed-external-objects'; objects.mkdir()
        defaults=f.repo/'objects'
        writer=dict(original,GIT_OBJECT_DIRECTORY=str(objects),
                    GIT_ALTERNATE_OBJECT_DIRECTORIES=str(defaults))
        signed=run(['git','-C',f.repo,'-c','user.signingkey='+str(f.root/'signature'),
                    'commit-tree','-S',f.tree],input=b'external signed commit\n',
                   env=writer).decode().strip()
        cases=(writer,dict(original,GIT_ALTERNATE_OBJECT_DIRECTORIES=str(objects)))
        for env in cases:
            with self.subTest(variables={key:value for key,value in env.items()
                                         if key in ('GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES')}):
                f.env=env
                native=subprocess.run(['git','-C',f.repo,'verify-commit',signed],
                                      env=f.env,capture_output=True,timeout=10)
                self.assertEqual(native.returncode,0,native.stderr)
                process=f.probe('local',signed,verify=True)
                self.assertEqual(f.line(process)['tree'],f.tree)
                value,_=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n'); f.finish(process)
    def test_native_openpgp_verification_uses_git_configuration(self):
        f=self.f; home=f.root/'gnupg'; home.mkdir(mode=0o700)
        env=dict(f.env,GNUPGHOME=str(home))
        run(['gpg','--batch','--pinentry-mode','loopback','--passphrase','','--quick-generate-key','Fixture <fixture@example.invalid>','ed25519','sign','0'],env=env)
        wrapper=f.root/'gpg-program'; wrapper.write_text(f'#!/bin/sh\nGNUPGHOME="{home}" exec gpg "$@"\n'); wrapper.chmod(0o700)
        f.git('config','gpg.program',str(wrapper)); f.git('config','user.signingkey','fixture@example.invalid')
        signed=f.git('commit-tree','-S',f.tree,data=b'OpenPGP fixture\n')
        f.cwd=f.repo
        process=f.probe('local',signed,verify=True); f.line(process); f.operation(process,'read deep/file'); f.finish(process)
        empty=f.root/'unknown-gnupg'; empty.mkdir(mode=0o700)
        wrapper.write_text(f'#!/bin/sh\nGNUPGHOME="{empty}" exec gpg "$@"\n')
        process=f.probe('local',signed,verify=True); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'execution forbidden',errors)
    def test_review_stalled_transport_cancels_and_cleans_descendants(self):
        f=self.f; wrapper=f.root/'hang-ssh'; pid=f.root/'transport-pid'
        wrapper.write_text(f'#!/bin/sh\necho $$ > "{pid}"\nexec sleep 300\n'); wrapper.chmod(0o700)
        f.env['GIT_SSH_COMMAND']=str(wrapper)
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),'ssh://fixture@example.invalid/repo','HEAD','--','true'],cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+5
        while not pid.exists() and time.monotonic()<deadline: time.sleep(.01)
        self.assertTrue(pid.exists())
        transport_fd=os.pidfd_open(int(pid.read_text()))
        self.addCleanup(os.close,transport_fd)
        process.send_signal(signal.SIGTERM)
        try: process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            signal.pidfd_send_signal(transport_fd,signal.SIGKILL)
            raise
        self.assertEqual(process.returncode,-signal.SIGTERM)
        # Reaping the Git leader can precede delivery of its group's SIGKILL to
        # the transport. Observe this process's bounded death, without PID reuse
        # or depending on whether the container's PID 1 reaps adopted zombies.
        self.assertTrue(select.select([transport_fd],[],[],2)[0],
                        'owned stalled transport survived cancellation')
        self.assertEqual(list(f.root.glob('git-on-*')),[])
    def test_startup_transport_all_handled_signals(self):
        f=self.f; wrapper=f.root/'hanging-selected-ssh'; pid=f.root/'transport-pid'
        wrapper.write_text(f'#!/bin/sh\necho $$ > "{pid}.new"\nmv "{pid}.new" "{pid}"\nexec sleep 300\n'); wrapper.chmod(0o700)
        f.env['GIT_SSH_COMMAND']=str(wrapper)
        for sig in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGQUIT):
            with self.subTest(signal=sig):
                pid.unlink(missing_ok=True)
                process=subprocess.Popen([str(OPTIONS.binary.resolve()),'ssh://fixture@example.invalid/repo','HEAD','--','true'],cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                f.children.append(process)
                deadline=time.monotonic()+5
                while not pid.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.01)
                self.assertTrue(pid.exists(),'owned startup transport never began')
                owned=int(pid.read_text())
                descriptor=os.pidfd_open(owned)
                try:
                    process.send_signal(sig); _,errors=process.communicate(timeout=5)
                    self.assertEqual(process.returncode,-sig,errors)
                    # Reaping Git can precede its transport processing the same
                    # group's SIGKILL. Wait for this exact process's bounded
                    # death, including on loaded runners and unreaped zombies.
                    self.assertTrue(select.select([descriptor],[],[],2)[0],
                                    'owned startup transport survived cancellation')
                    self.assertEqual(list(f.root.glob('git-on-*')),[])
                finally:
                    if not select.select([descriptor],[],[],0)[0]:
                        with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(descriptor,signal.SIGKILL)
                    os.close(descriptor)
                    if process.poll() is None:
                        process.kill(); process.communicate(timeout=5)
    def test_sigkill_during_native_config_startup_requires_external_process_cleanup(self):
        f=self.f; native=str(Path(shutil.which('git')).resolve())
        config=f.root/'config-fifo'; os.mkfifo(config,0o600)
        programs=f.root/'native-git-wrapper'; programs.mkdir()
        pidfile=f.root/'native-git-pid'; wrapper=programs/'git'
        wrapper.write_text(f'#!/bin/sh\nprintf "%s\\n" "$$" > "{pidfile}.new"\nmv "{pidfile}.new" "{pidfile}"\nexec "{native}" "$@"\n'); wrapper.chmod(0o700)
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),'file://'+str(f.repo),'HEAD','--','true'],
            cwd=f.cwd,env=dict(f.env,PATH=str(programs)+os.pathsep+f.env['PATH'],GIT_CONFIG_GLOBAL=str(config)),
            stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process); native_pid=None; descriptor=None; directory=None
        def dispose():
            if descriptor is not None and not select.select([descriptor],[],[],0)[0]:
                with contextlib.suppress(ProcessLookupError): os.killpg(native_pid,signal.SIGKILL)
            if process.poll() is None: process.kill()
            process.communicate(timeout=10)
            if directory is not None and directory.exists():
                subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
            if descriptor is not None: os.close(descriptor)
        self.addCleanup(dispose)
        deadline=time.monotonic()+5
        while not pidfile.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.01)
        self.assertTrue(pidfile.exists(),'actual native Git did not start')
        native_pid=int(pidfile.read_text()); descriptor=os.pidfd_open(native_pid)
        self.assertEqual(os.getpgid(native_pid),native_pid)
        directory,=f.root.glob('git-on-*')
        process.kill(); self.assertEqual(process.wait(timeout=5),-signal.SIGKILL)
        self.assertFalse(select.select([descriptor],[],[],.1)[0],'startup native operation unexpectedly stopped with runner death')
        state=json.loads((directory/'.state').read_text())
        self.assertIsNone(state['pin_ref']); self.assertFalse((directory/'ssh-control-owner').exists())
        self.assertFalse(any(line.split()[4]==str(directory/'mount') for line in Path('/proc/self/mountinfo').read_text().splitlines()))
        with (directory/'.lock').open('rb') as held: fcntl.flock(held,fcntl.LOCK_EX|fcntl.LOCK_NB)
        # This live pidfd identifies only the disposable child captured above.
        os.killpg(native_pid,signal.SIGKILL)
        self.assertTrue(select.select([descriptor],[],[],5)[0])
        process.communicate(timeout=5)
        cleanup=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
        self.assertEqual(cleanup.returncode,0,cleanup.stderr); self.assertFalse(directory.exists())
        print('SIGKILL startup evidence:',json.dumps(dict(native_child_survives=True,mount_present=False,
              pin_present=False,ssh_owner_present=False,run_lock_released=True,explicit_recovery_succeeded=True)),flush=True)
    def test_ssh_encrypted_identity_one_prompt_and_transport_reuse(self):
        f=self.f; url=f.ssh(encrypted=True)
        process=f.probe(url); f.line(process)
        f.operation(process,'read deep/file'); f.operation(process,'read other')
        self.assertEqual((f.root/'prompts').read_text().splitlines(), ['prompt'])
        f.finish(process)
        log=(f.root/'sshd.log').read_text()
        self.assertEqual(log.count('Accepted publickey'),1,log)
    def test_ssh_unlocked_agent_has_no_prompts(self):
        f=self.f; url=f.ssh(encrypted=True)
        agent=run(['ssh-agent','-s']).decode()
        for line in agent.splitlines():
            if line.startswith(('SSH_AUTH_SOCK=', 'SSH_AGENT_PID=')):
                key,value=line.split(';',1)[0].split('=',1); f.env[key]=value
        self.addCleanup(lambda: run(['ssh-agent','-k'], env=f.env))
        run(['ssh-add', f.root/'identity'], env=f.env)
        (f.root/'prompts').unlink()
        process=f.probe(url); f.line(process); f.operation(process,'read deep/file'); f.finish(process)
        self.assertFalse((f.root/'prompts').exists())
    def test_refusal_filter_missing_oid_and_connection_loss(self):
        f=self.f; url=f.ssh()
        process=f.probe(url); f.line(process)
        # Stop the owned ControlMaster, without affecting other SSH connections.
        sockets=list(f.root.glob('*/ssh-control')); self.assertEqual(len(sockets),1)
        run(['ssh','-F','/dev/null','-S',sockets[0],'-O','exit','git-on-control'])
        process.stdin.write(b'read deep/file\n'); process.stdin.flush()
        _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'remote fetch of exact object',errors)
        f.git('config','uploadpack.allowFilter','false')
        process=f.probe(url); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'does not advertise object filtering',errors)
    def test_review_connection_loss_cannot_reconnect_with_explicit_proxy_option(self):
        f=self.f; url=f.ssh()
        f.env['GIT_SSH_COMMAND'] += ' -o ProxyCommand=none'
        process=f.probe(url); f.line(process)
        sockets=list(f.root.glob('*/ssh-control')); self.assertEqual(len(sockets),1)
        run(['ssh','-F','/dev/null','-S',sockets[0],'-O','exit','git-on-control'])
        process.stdin.write(b'read deep/file\n'); process.stdin.flush()
        _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0,'lost master was replaced by another authenticated connection')
        self.assertIn(b'remote fetch of exact object',errors)
    def test_real_mount_remote_disposable_cleanup_and_cancellation(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        result=subprocess.run(['git','-c','http.sslCAInfo='+f.env['GIT_SSL_CAINFO'],'on',url,'main','--','sh','-c','cat deep/file; mkdir out; printf kept > out/result'],cwd=f.cwd,env=dict(f.env,PATH=str(OPTIONS.binary.resolve().parent)+':'+f.env['PATH']),capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr); self.assertEqual(result.stdout,b'expected\n')
        self.assertEqual(list(f.root.glob('git-on-*')),[])
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--','sleep','30'],cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+10
        while time.monotonic()<deadline and not any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()): time.sleep(.02)
        self.assertTrue(any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()),'remote run never mounted')
        process.send_signal(signal.SIGTERM); process.communicate(timeout=10)
        self.assertEqual(process.returncode,-signal.SIGTERM)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def test_named_https_remote_and_inherited_git_config_parameters(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        f.git('init','--quiet',str(f.cwd),at=f.root)
        f.git('remote','add','fixture',url,at=f.cwd)
        f.env['GIT_CONFIG_PARAMETERS']="'http.extraHeader'='X-Git-On-Fixture: inherited'"
        process=f.probe('fixture'); f.line(process)
        value,_=f.operation(process,'read deep/file'); f.finish(process)
        self.assertEqual(value['data'],'expected\n')
        self.assertTrue(all(x['headers']==['inherited'] for x in f.requests))

    def test_stat_repeated_reads_and_large_partial_reads_fetch_only_demand(self):
        f=self.f; payload=bytes(range(256))*(64*1024)
        large=f.git('hash-object','-w','--stdin',data=payload)
        tree=f.git('mktree',data=f'100644 blob {large}\tlarge\n100644 blob {f.other}\tother\n'.encode())
        commit=f.git('commit-tree',tree,data=b'large fixture\n')
        f.git('update-ref','refs/heads/main',commit)
        url=f.https(); self.addCleanup(f.server.shutdown)
        process=f.probe(url); initial=f.line(process)
        self.assertEqual(set(initial['objects']),{commit,tree})
        size,stat=f.operation(process,'stat large')
        self.assertEqual(size['size'],len(payload)); self.assertEqual(stat['metrics']['blob_loads'],0)
        request_count=len(f.requests)
        for offset in (0,len(payload)-4096,512,512):
            value,report=f.operation(process,f'range {offset} 4096 large')
            self.assertEqual(bytes(value['bytes']),payload[offset:offset+4096])
            self.assertEqual(report['metrics']['blob_loads'],1)
            self.assertEqual(report['metrics']['spool_writes'],1)
            self.assertNotIn(f.other,report['objects'])
        self.assertEqual(len(f.requests),request_count)
        _,report=f.operation(process,'release'); self.assertEqual(report['metrics']['active_spool_bytes'],0)
        f.finish(process)

    def test_authentication_and_exact_oid_tree_blob_refusals(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        f.env['GIT_TERMINAL_PROMPT']='0'; f.env['GIT_ASKPASS']='/bin/false'
        f.deny_auth=True
        process=f.probe(url); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'authentication failed',errors)
        f.deny_auth=False
        unreachable=f.git('commit-tree',f.tree,data=b'unreachable exact commit\n')
        f.git('config','uploadpack.allowAnySHA1InWant','false')
        f.git('config','uploadpack.allowReachableSHA1InWant','false')
        process=f.probe(url,'1'*40); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'1'*40,errors)
        # Explicit server refusal of an existing unadvertised object is a
        # distinct fault from an absent OID, and cannot cause an eager fallback.
        f.denied_oids.add(unreachable)
        process=f.probe(url,unreachable); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'HTTP 403',errors)
        self.assertTrue(any(unreachable in request['wants'] and request.get('status')==403
                            for request in f.requests),f.requests)
        f.denied_oids.clear(); f.git('config','uploadpack.allowAnySHA1InWant','true')
        for denied in (f.subtree,f.blob):
            shutil.rmtree(f.root/'cache',ignore_errors=True)
            process=f.probe(url); f.line(process); f.denied_oids.add(denied)
            process.stdin.write(b'read deep/file\n'); process.stdin.flush()
            _,errors=process.communicate(timeout=20)
            self.assertNotEqual(process.returncode,0); self.assertIn(denied.encode(),errors)
            self.assertIn(b'remote object loading failed',errors)
            f.denied_oids.clear()

    def test_signed_tags_unsigned_targets_lightweight_and_nested_tags(self):
        f=self.f; signed,_=self.signing(); f.cwd=f.repo
        key=f.root/'signature'
        f.git('-c','gpg.format=ssh','-c','user.signingkey='+str(key),'tag','-s','-m','bind unsigned','signed-unsigned',f.commit)
        f.git('tag','light-signed',signed); f.git('tag','light-unsigned',f.commit)
        for revision in ('signed-unsigned','light-signed'):
            self.assertEqual(subprocess.run(['git','-C',str(f.repo),'verify-tag' if revision=='signed-unsigned' else 'verify-commit',revision],env=f.env,capture_output=True).returncode,0)
            process=f.probe('local',revision,verify=True); self.assertEqual(f.line(process)['tree'],f.tree); f.finish(process)
        process=f.probe('local','light-unsigned',verify=True); process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0)
        target=f.commit; kind='commit'
        for depth in range(1,65):
            target=f.git('hash-object','-t','tag','-w','--stdin',data=f'object {target}\ntype {kind}\ntag nested-{depth}\ntagger Fixture <fixture@example.invalid> 0 +0000\n\nnested\n'.encode())
            kind='tag'
            if depth in (63,64):
                name=f'signed-nested-{depth}'
                f.git('-c','gpg.format=ssh','-c','user.signingkey='+str(key),'tag','-s','-m','nested signed',name,target)
                process=f.probe('local',name,verify=True)
                self.assertEqual(f.line(process)['tree'],f.tree); f.finish(process)

    def test_native_verifier_moving_ref_keeps_exact_distinct_tree(self):
        f=self.f; signed,allowed=self.signing(); f.cwd=f.repo
        different_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        different=f.git('commit-tree',different_tree,data=b'distinct tree B\n')
        f.git('update-ref','refs/heads/main',signed)
        verifier=f.root/'ssh-verifier'
        verifier.write_text(f'#!/bin/sh\ngit --git-dir="{f.repo}" update-ref refs/heads/main {different}\nexec ssh-keygen "$@"\n')
        verifier.chmod(0o700)
        f.env.update(GIT_CONFIG_COUNT='3',GIT_CONFIG_KEY_2='gpg.ssh.program',GIT_CONFIG_VALUE_2=str(verifier))
        process=f.probe('local','HEAD',verify=True)
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file'); self.assertEqual(value['data'],'expected\n'); f.finish(process)
        self.assertEqual(f.git('rev-parse','HEAD'),different)

    def openpgp_signing(self):
        f=self.f; signing=f.root/'gpg-sign'; signing.mkdir(mode=0o700)
        self.addCleanup(run,['gpgconf','--kill','all'],env=dict(f.env,GNUPGHOME=str(signing)))
        run(['gpg','--batch','--pinentry-mode','loopback','--passphrase','','--quick-generate-key','Fixture <fixture@example.invalid>','ed25519','sign','0'],env=dict(f.env,GNUPGHOME=str(signing)))
        wrapper=f.root/'gpg-oracle'; wrapper.write_text(f'#!/bin/sh\nGNUPGHOME="{signing}" exec gpg "$@"\n'); wrapper.chmod(0o700)
        f.git('config','gpg.program',str(wrapper)); f.git('config','user.signingkey','fixture@example.invalid')
        signed=f.git('commit-tree','-S',f.tree,data=b'trust fixture\n')
        f.git('tag','-s','-m','trust fixture tag','openpgp-policy-tag',signed)
        tag=f.git('rev-parse','refs/tags/openpgp-policy-tag')
        public=run(['gpg','--batch','--export'],env=dict(f.env,GNUPGHOME=str(signing)))
        verification=f.root/'gpg-public'; verification.mkdir(mode=0o700)
        self.addCleanup(run,['gpgconf','--kill','all'],env=dict(f.env,GNUPGHOME=str(verification)))
        run(['gpg','--batch','--import'],input=public,env=dict(f.env,GNUPGHOME=str(verification)))
        wrapper.write_text(f'#!/bin/sh\nGNUPGHOME="{verification}" exec gpg "$@"\n')
        return signed,tag

    def test_openpgp_invalid_and_unknown_trust_match_native_git(self):
        f=self.f; signed,tag=self.openpgp_signing()
        f.cwd=f.repo
        for operation,oid in (('verify-commit',signed),('verify-tag',tag)):
            native=subprocess.run(['git','-C',str(f.repo),operation,'--raw',oid],env=f.env,capture_output=True,timeout=20)
            self.assertEqual(native.returncode,0,native.stderr)
            self.assertIn(b'[GNUPG:] TRUST_UNDEFINED ',native.stderr)
            process=f.probe('local',oid,verify=True)
            self.assertEqual(f.line(process)['tree'],f.tree); f.finish(process)
        raw=subprocess.run(['git','-C',str(f.repo),'cat-file','commit',signed],env=f.env,capture_output=True,check=True).stdout
        invalid=f.git('hash-object','-t','commit','-w','--stdin',data=raw.replace(b'trust fixture',b'changed fixture'))
        native=subprocess.run(['git','-C',str(f.repo),'verify-commit','--raw',invalid],env=f.env,capture_output=True,timeout=20)
        self.assertNotEqual(native.returncode,0,native.stderr)
        self.assertIn(b'[GNUPG:] BADSIG ',native.stderr)
        process=f.probe('local',invalid,verify=True)
        _,errors=process.communicate(timeout=20); self.assertNotEqual(process.returncode,0)
        self.assertIn(b'execution forbidden',errors)

    def test_openpgp_configured_trust_rejection_prevents_local_and_remote_execution(self):
        f=self.f; signed,tag=self.openpgp_signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        sentinel=f.root/'command-executed'
        command=[sys.executable,'-c','from pathlib import Path; import sys; Path(sys.argv[1]).touch()',str(sentinel)]
        env=dict(f.env,PATH=str(OPTIONS.binary.parent)+':'+f.env['PATH'])
        for policy in ('repository','git-c'):
            # The -c case must override a permissive repository setting, then
            # survive Git::capture and replay into the remote private repository.
            f.git('config','gpg.minTrustLevel','fully' if policy=='repository' else 'undefined')
            config=[] if policy=='repository' else ['-c','gpg.minTrustLevel=fully']
            runner=[str(OPTIONS.binary)] if policy=='repository' else ['git',*config,'on']
            for operation,oid in (('verify-commit',signed),('verify-tag',tag)):
                native=subprocess.run(['git','-C',str(f.repo),*config,operation,'--raw',oid],env=f.env,capture_output=True,timeout=20)
                self.assertEqual(native.returncode,1,native.stderr)
                self.assertIn(b'[GNUPG:] GOODSIG ',native.stderr)
                self.assertIn(b'[GNUPG:] TRUST_UNDEFINED ',native.stderr)
                for source,operands in (('local',[oid]),('remote',[url,oid])):
                    with self.subTest(policy=policy,operation=operation,source=source):
                        result=subprocess.run([*runner,*operands,'--verify','--',*command],cwd=f.repo,env=env,capture_output=True,timeout=30)
                        self.assertEqual(result.returncode,125,result.stderr)
                        self.assertIn(f'native Git {operation} failed'.encode(),result.stderr)
                        self.assertIn(b'execution forbidden',result.stderr)
                        self.assertNotIn(b'mount snapshot',result.stderr)
                        self.assertFalse(sentinel.exists(),'trust-rejected command executed')
                        self.assertEqual(result.stdout,b'')
                        self.assertEqual(list(f.root.glob('git-on-*')),[])
                        print('configured native trust rejection evidence:',json.dumps(dict(policy=policy,operation=operation,source=source,native_status=native.returncode,runner_status=result.returncode,command_executed=sentinel.exists())),flush=True)

    def test_real_mount_openpgp_default_policy_executes_unknown_trust_commit_and_tag(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; signed,tag=self.openpgp_signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        sentinel=f.root/'command-executed'
        code='from pathlib import Path; import sys; Path(sys.argv[1]).write_bytes(Path("deep/file").read_bytes())'
        for operation,oid in (('verify-commit',signed),('verify-tag',tag)):
            native=subprocess.run(['git','-C',str(f.repo),operation,'--raw',oid],env=f.env,capture_output=True,timeout=20)
            self.assertEqual(native.returncode,0,native.stderr)
            self.assertIn(b'[GNUPG:] TRUST_UNDEFINED ',native.stderr)
            for source,operands in (('local',[oid]),('remote',[url,oid])):
                with self.subTest(operation=operation,source=source):
                    sentinel.unlink(missing_ok=True)
                    result=subprocess.run([str(OPTIONS.binary),*operands,'--verify','--',sys.executable,'-c',code,str(sentinel)],cwd=f.repo,env=f.env,capture_output=True,timeout=30)
                    self.assertEqual(result.returncode,0,result.stderr)
                    self.assertEqual(sentinel.read_bytes(),b'expected\n')
                    self.assertEqual(list(f.root.glob('git-on-*')),[])
                    print('native default trust execution evidence:',json.dumps(dict(operation=operation,source=source,native_status=native.returncode,runner_status=result.returncode,command_bytes=sentinel.stat().st_size)),flush=True)

    def test_real_mount_native_git_c_verification_and_caught_transport_failure(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; signed,allowed=self.signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        env=dict(f.env,PATH=str(OPTIONS.binary.resolve().parent)+':'+f.env['PATH'])
        for key in list(env):
            if key.startswith('GIT_CONFIG_KEY_') or key.startswith('GIT_CONFIG_VALUE_') or key=='GIT_CONFIG_COUNT': del env[key]
        for cwd,operands in ((f.cwd,[url,signed]),(f.repo,[signed])):
            result=subprocess.run(['git','-c','gpg.format=ssh','-c','gpg.ssh.allowedSignersFile='+str(allowed),'on','--verify',*operands,'--','cat','deep/file'],cwd=cwd,env=env,capture_output=True,timeout=30)
            self.assertEqual(result.returncode,0,result.stderr); self.assertEqual(result.stdout,b'expected\n')
        # The command catches EIO and returns 0. The runner must still report 125.
        f.denied_oids.add(f.blob)
        code="import errno\ntry:\n open('deep/file').read()\nexcept OSError as e:\n assert e.errno == errno.EIO\nelse:\n raise AssertionError('transport fault not observed')\n"
        result=subprocess.run([str(OPTIONS.binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,125,result.stderr)
        self.assertIn(f.blob.encode(),result.stderr); self.assertIn(b'execution incomplete',result.stderr)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def test_remote_never_requires_or_modifies_global_cache(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        original=dict(f.env)
        blocked=f.root/'cache-path-is-a-file';blocked.write_bytes(b'unchanged cache path')
        historical=f.root/'historical-cache'
        for version in ('objects-v1','objects-v2'):
            directory=historical/'git-on'/version;directory.mkdir(parents=True)
            (directory/'keep').write_bytes(b'old user data')
        for mode in ('unset','blocked','existing'):
            with self.subTest(mode=mode):
                f.env=dict(original)
                if mode=='unset':
                    f.env.pop('HOME',None);f.env.pop('XDG_CACHE_HOME',None)
                else:f.env['XDG_CACHE_HOME']=str(blocked if mode=='blocked' else historical)
                process=f.probe(url);f.line(process)
                value,report=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n')
                self.assertEqual(set(report['objects']),{f.commit,f.tree,f.subtree,f.blob})
                f.finish(process)
                self.assertEqual(blocked.read_bytes(),b'unchanged cache path')
                for version in ('objects-v1','objects-v2'):
                    directory=historical/'git-on'/version
                    self.assertEqual(list(directory.iterdir()),[directory/'keep'])
                    self.assertEqual((directory/'keep').read_bytes(),b'old user data')
        f.env=original

    def test_separate_remote_runs_fetch_demanded_objects_again(self):
        f=self.f;url=f.https();self.addCleanup(f.server.shutdown)
        demands=[]
        for _ in range(2):
            start=len(f.requests);process=f.probe(url);f.line(process)
            value,_=f.operation(process,'read deep/file');self.assertEqual(value['data'],'expected\n')
            f.finish(process)
            demands.append({oid for request in f.requests[start:] for oid in request['wants']})
        self.assertEqual(demands,[{f.commit,f.tree,f.subtree,f.blob}]*2)
        self.assertFalse((f.root/'cache').exists(),'remote run created a global cache')

    def test_large_remote_objects_reuse_run_odb_and_fresh_runs_stream(self):
        f=self.f; ids=[];object_mib=16 # Above both the 8 MiB decoded cache and native Git streaming threshold.
        for byte in b'abc':
            with tempfile.TemporaryFile() as output:
                writer=subprocess.Popen(['git','-C',str(f.repo),'hash-object','-w','--stdin'],env=f.env,stdin=subprocess.PIPE,stdout=output)
                for _ in range(object_mib): writer.stdin.write(bytes([byte])*(1024*1024))
                writer.stdin.close(); self.assertEqual(writer.wait(),0); output.seek(0); ids.append(output.read().decode().strip())
        tree=f.git('mktree',data=''.join(f'100644 blob {oid}\t{name}\n' for name,oid in zip('abc',ids)).encode())
        commit=f.git('commit-tree',tree,data=b'large run-local reuse fixture\n');f.git('update-ref','refs/heads/main',commit)
        url=f.https();self.addCleanup(f.server.shutdown)
        process=f.probe(url);f.line(process)
        for name in 'abc':
            value,last=f.operation(process,f'range 13 16 {name}')
            self.assertEqual(bytes(value['bytes']),name.encode()*16)
        self.assertEqual(last['metrics']['remote_fetches'],5)
        self.assertEqual(last['metrics']['spool_writes'],3)
        requests=len(f.requests)
        value,last=f.operation(process,'range 13 16 a');self.assertEqual(bytes(value['bytes']),b'a'*16)
        self.assertEqual(len(f.requests),requests,'available run object fetched again')
        self.assertEqual(last['metrics']['spool_writes'],3,'closed blob was decoded and written again')
        f.finish(process)
        # A separate invocation streams a newly fetched loose object again.
        requests=len(f.requests);process=f.probe(url);f.line(process)
        value,last=f.operation(process,'range 13 16 c');self.assertEqual(bytes(value['bytes']),b'c'*16)
        self.assertEqual(last['metrics']['remote_raw_blob_imports'],1)
        self.assertEqual(last['metrics']['blob_loads'],1)
        self.assertEqual({oid for request in f.requests[requests:] for oid in request['wants']},{commit,tree,ids[2]})
        f.finish(process)
        self.assertFalse((f.root/'cache').exists())
        print('large run object evidence:',json.dumps(dict(logical_blob_bytes=3*object_mib*1024*1024,metrics=last['metrics'])),flush=True)

    @unittest.skipUnless(sys.platform=='linux','controlling Bash job and /proc state observation')
    def test_materialization_native_git_tty_handoff_with_tostop(self):
        import native_job_control
        f=self.f;url=f.https();self.addCleanup(f.server.shutdown)
        # Exercise the native compatibility path; configured fsck stays native.
        f.env.update(GIT_CONFIG_COUNT='1',GIT_CONFIG_KEY_0='fetch.fsckObjects',GIT_CONFIG_VALUE_0='false')
        tools=f.root/'tostop-tools';tools.mkdir();(tools/'git').symlink_to(shutil.which('git'))
        shell=native_job_control.Shell(f);self.addCleanup(shell.close)
        shell.send(b'stty tostop\n');shell.expect(shell.prompt)
        f.stalled_oids={f.blob}
        runner_file=f.root/'tostop-runner'
        argv=['/bin/sh','-c','printf "%s" "$$" > "$1"; shift; exec "$@"','fixture',str(runner_file),str(OPTIONS.binary),url,'HEAD','--write-policy=all','--',sys.executable,'-c',"print('child-completed')"]
        shell.send((f'PATH={shlex.quote(str(tools))} TERM=xterm '+shlex.join(argv)+'; printf "STATUS:%d\\n" "$?"\n').encode())
        try:
            self.assertTrue(f.stalled.wait(10),'native fetch did not reach controlled TTY stall')
            self.assertTrue(runner_file.exists())
            runner=int(runner_file.read_text());foreground=os.tcgetpgrp(shell.fd)
            shell.groups.update((runner,foreground))
            self.assertNotEqual(foreground,runner,'native Git never received terminal foreground ownership')
            self.assertEqual(os.getsid(runner),shell.pid)
            deadline=time.monotonic()+1.3
            while time.monotonic()<deadline:
                shell.read()
                if native_job_control.state(runner)=='T':break
            self.assertNotEqual(native_job_control.state(runner),'T',f'runner stopped during native Git ownership: {bytes(shell.transcript)!r}')
            f.resume.set()
            output=shell.expect(shell.prompt,timeout=15)
            self.assertIn(b'STATUS:0',output)
            self.assertIn(b'\r\nchild-completed\r\n',output)
            self.assertEqual(list(f.root.glob('git-on-*')),[])
        finally:f.resume.set()

    @unittest.skipUnless(sys.platform=='linux','controlling Bash job and /proc state observation')
    def test_session_materialization_keeps_spinner_foreground_with_tostop(self):
        import native_job_control
        f=self.f;url=f.https();self.addCleanup(f.server.shutdown)
        tools=f.root/'session-tostop-tools';tools.mkdir();(tools/'git').symlink_to(shutil.which('git'))
        shell=native_job_control.Shell(f);self.addCleanup(shell.close)
        shell.send(b'stty tostop\n');shell.expect(shell.prompt)
        f.stalled_oids={f.blob};runner_file=f.root/'session-runner'
        argv=['/bin/sh','-c','printf "%s" "$$" > "$1"; shift; exec "$@"','fixture',str(runner_file),str(OPTIONS.binary),url,'HEAD','--write-policy=all','--',sys.executable,'-c',"print('session-child-completed')"]
        shell.send((f'PATH={shlex.quote(str(tools))} TERM=xterm '+shlex.join(argv)+'; printf "STATUS:%d\\n" "$?"\n').encode())
        try:
            self.assertTrue(f.stalled.wait(10),'session did not reach controlled stall')
            runner=int(runner_file.read_text());shell.groups.add(runner)
            self.assertEqual(os.tcgetpgrp(shell.fd),runner,'silent session stole terminal ownership')
            output=shell.expect(b'Preparing snapshot',timeout=5)
            self.assertNotEqual(native_job_control.state(runner),'T',output)
            f.resume.set();output=shell.expect(shell.prompt,timeout=15)
            self.assertIn(b'STATUS:0',output);self.assertIn(b'session-child-completed',output)
            self.assertEqual(list(f.root.glob('git-on-*')),[])
        finally:f.resume.set()

    def test_materialization_progress_is_delayed_tty_only_and_cleared_before_child_or_error(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        programs=f.root/'progress-tools'; programs.mkdir(); (programs/'git').symlink_to(shutil.which('git'))
        env=dict(f.env,PATH=str(programs),TERM='xterm')
        f.stalled_oids={f.blob}
        for outcome in ('success','error','cancel'):
            with self.subTest(outcome=outcome):
                shutil.rmtree(f.root/'cache',ignore_errors=True)
                f.resume.clear(); f.stalled.clear(); f.denied_oids=set()
                master,slave=pty.openpty()
                self.addCleanup(os.close,master)
                child="import sys; sys.stderr.write('CHILD-START\\n'); print('child output')"
                process=subprocess.Popen([str(OPTIONS.binary),url,'HEAD','--write-policy=all','--',sys.executable,'-c',child],cwd=f.cwd,env=env,stdout=subprocess.PIPE,stderr=slave)
                os.close(slave); f.children.append(process)
                self.assertTrue(f.stalled.wait(10),'materialization never reached controlled object stall')
                transcript=bytearray(); deadline=time.monotonic()+5
                while b'Preparing snapshot' not in transcript and time.monotonic()<deadline:
                    if select.select([master],[],[],.1)[0]: transcript.extend(os.read(master,65536))
                self.assertIn(b'Preparing snapshot',transcript)
                self.assertNotIn(b'CHILD-START',transcript)
                if outcome=='error': f.denied_oids={f.blob}
                if outcome=='cancel': process.send_signal(signal.SIGTERM)
                f.resume.set()
                output,_=process.communicate(timeout=20)
                while select.select([master],[],[],.05)[0]:
                    try: chunk=os.read(master,65536)
                    except OSError as error:
                        if error.errno==errno.EIO: break
                        raise
                    if not chunk: break
                    transcript.extend(chunk)
                self.assertIn(b'\x1b[2K',transcript)
                if outcome=='success':
                    self.assertEqual(process.returncode,0,transcript)
                    self.assertEqual(output,b'child output\n')
                    self.assertGreater(transcript.index(b'CHILD-START'),transcript.rfind(b'\x1b[2K'))
                else:
                    self.assertEqual(process.returncode,-signal.SIGTERM if outcome=='cancel' else 125,transcript)
                    self.assertNotIn(b'CHILD-START',transcript); self.assertEqual(output,b'')
                    if outcome=='error': self.assertIn(b'fetch of exact object',transcript)
                self.assertEqual(list(f.root.glob('git-on-*')),[])

    @unittest.skipUnless(sys.platform=='linux' and shutil.which('strace'), 'Linux strace object-I/O observation')
    def test_private_remote_objects_do_not_sync_disposable_payloads(self):
        f=self.f;trace=f.root/'private-object-io.trace'
        # The caller can request durability for its real repository. Only the
        # disposable private destination overrides that policy, not signatures
        # or object-integrity checks, and no source configuration is rewritten.
        env=dict(f.env,GIT_CONFIG_COUNT='2',GIT_CONFIG_KEY_0='core.fsync',GIT_CONFIG_VALUE_0='all',
            GIT_CONFIG_KEY_1='core.fsyncObjectFiles',GIT_CONFIG_VALUE_1='true')
        before=f.git('config','--list')
        result=subprocess.run(['strace','--kill-on-exit','-f','-yy','-e','trace=write,fsync,fdatasync',
            '-o',str(trace),str(OPTIONS.probe),'file://'+str(f.repo),'HEAD','--metrics-only'],
            cwd=f.cwd,env=env,input=b'read deep/file\nread deep/file\nreport\nexit\n',capture_output=True,timeout=40)
        self.assertEqual(result.returncode,0,result.stderr)
        rows=[json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([row['data'] for row in rows if 'data' in row],['expected\n']*2)
        reports=[row['metrics'] for row in rows if 'metrics' in row]
        self.assertEqual(reports[-1]['remote_fetches'],4)
        self.assertEqual(reports[-1]['remote_fetches'],reports[-2]['remote_fetches'])
        lines=[line for line in trace.read_text().splitlines() if '/remote-repository/objects/' in line]
        self.assertTrue(any(re.search(r'\bwrite\(',line) for line in lines),'object writes were not traced')
        self.assertEqual([line for line in lines if re.search(r'\b(fsync|fdatasync)\(',line)],[])
        self.assertEqual(f.git('config','--list'),before)

    def test_remote_raw_import_reuses_evicted_objects_without_loose_blob_copies(self):
        f=self.f;entries=[];oids=[]
        for index in range(140):
            oid=f.git('hash-object','-w','--stdin',data=f'raw-{index}\n'.encode());oids.append(oid)
            entries.append(f'100644 blob {oid}\tf{index:03d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'raw eviction fixture\n');f.git('update-ref','refs/heads/main',commit)
        process=f.probe('file://'+str(f.repo));f.line(process)
        for index in range(140):
            value,last=f.operation(process,f'read f{index:03d}')
            self.assertEqual(value['data'],f'raw-{index}\n')
        value,again=f.operation(process,'read f000');self.assertEqual(value['data'],'raw-0\n')
        self.assertEqual(again['metrics']['remote_fetches'],142)
        self.assertEqual(again['metrics']['blob_loads'],last['metrics']['blob_loads'],'evicted raw blob was unpacked again')
        destination=next(f.root.glob('*/remote-repository'))
        self.assertEqual(list((destination/'objects/pack').iterdir()),[])
        self.assertFalse(any((destination/'objects'/oid[:2]/oid[2:]).exists() for oid in oids),'received blobs were recompressed into loose files')
        self.assertEqual(again['metrics']['retained_raw_blob_entries'],140)
        self.assertLessEqual(again['metrics']['blob_cache_entries'],128)
        f.finish(process)

    def test_protocol_v2_reuses_native_clients_after_blob_eviction(self):
        f=self.f;count=140;entries=[];oids=[]
        for index in range(count):
            oid=f.git('hash-object','-w','--stdin',data=f'unique-{index}\n'.encode());oids.append(oid)
            entries.append(f'100644 blob {oid}\tf{index:04d}\n')
        tree=f.git('mktree',data=''.join(entries).encode());commit=f.git('commit-tree',tree,data=b'session reuse\n');f.git('update-ref','refs/heads/main',commit)
        trace=f.root/'native-processes.jsonl';f.env['GIT_TRACE2_EVENT']=str(trace)
        process=f.probe('file://'+str(f.repo));f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'read f{index:04d}')
            self.assertEqual(value['data'],f'unique-{index}\n')
        value,again=f.operation(process,'read f0000');self.assertEqual(value['data'],'unique-0\n')
        self.assertEqual(again['metrics']['blob_loads'],count)
        self.assertEqual(again['metrics']['remote_fetches'],count+2)
        f.finish(process)
        starts=[r['argv'] for line in trace.read_text().splitlines() if (r:=json.loads(line))['event']=='start']
        fetches=[args for args in starts if 'fetch' in args]
        decoders=[args for args in starts if 'cat-file' in args]
        self.assertEqual(len(fetches),1,'one native fetch process per lazy object returned')
        self.assertEqual(len(decoders),1,'one native decoder process per lazy object returned')
        self.assertEqual(again['metrics']['remote_session_fetch_operations'],count+1)
        self.assertEqual(again['metrics']['remote_session_decode_operations'],count+1)
        self.assertEqual(again['metrics']['remote_session_starts'],1)
        self.assertEqual(set(again['objects']),{commit,tree,*oids})

    @unittest.skipUnless(sys.platform=='linux', 'Linux process resource limits')
    def test_session_pack_and_raw_storage_respect_fsize_and_descriptor_limits(self):
        import hashlib,resource
        f=self.f;count=140;limit=8192;entries=[]
        for index in range(count):
            data=hashlib.shake_256(f'session-parts:{index}'.encode()).digest(limit)
            oid=f.git('hash-object','-w','--stdin',data=data)
            entries.append(f'100644 blob {oid}\tf{index:04d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'bounded session parts\n');f.git('update-ref','refs/heads/main',commit)
        def reduced_limits():
            resource.setrlimit(resource.RLIMIT_FSIZE,(limit,limit))
            resource.setrlimit(resource.RLIMIT_NOFILE,(64,64))
        process=subprocess.Popen([str(OPTIONS.probe),'file://'+str(f.repo),'HEAD','--metrics-only'],cwd=f.cwd,
            env=f.env,preexec_fn=reduced_limits,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        f.children.append(process);f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'range 0 32 f{index:04d}')
            self.assertEqual(value['bytes'],list(hashlib.shake_256(f'session-parts:{index}'.encode()).digest(32)))
            f.operation(process,'release')
        value,again=f.operation(process,'range 0 32 f0000')
        self.assertEqual(value['bytes'],list(hashlib.shake_256(b'session-parts:0').digest(32)))
        self.assertEqual(again['metrics']['blob_loads'],count)
        self.assertEqual(again['metrics']['remote_session_fetch_operations'],count+1)
        self.assertEqual(again['metrics']['remote_session_decode_operations'],count+1)
        self.assertEqual(again['metrics']['remote_session_starts'],1)
        parts=[p for directory in f.root.rglob('raw-parts-*') for p in directory.iterdir()]
        self.assertGreater(sum(p.stat().st_size for p in parts),limit)
        self.assertTrue(all(p.stat().st_size<=limit for p in parts),'a logical raw or pack file exceeded inherited FSIZE')
        self.assertLessEqual(len(list(Path(f'/proc/{process.pid}/fd').iterdir())),48)
        f.finish(process)

    def test_protocol_v2_ssh_session_reuses_owned_connection_and_fails_on_loss(self):
        f=self.f;url=f.ssh(protocol_v2=True)
        process=f.probe(url);f.line(process)
        value,report=f.operation(process,'read deep/file');self.assertEqual(value['data'],'expected\n')
        self.assertEqual(report['metrics']['remote_session_fetch_operations'],3)
        self.assertEqual(report['metrics']['remote_session_starts'],1)
        socket=next(f.root.rglob('ssh-control'))
        run(['ssh','-F','/dev/null','-S',socket,'-O','exit','fixture-host'],env=f.env)
        with self.assertRaises(AssertionError):f.operation(process,'read other')
        self.assertNotEqual(process.wait(timeout=10),0)
        self.assertEqual((f.root/'sshd.log').read_text().count('Accepted publickey'),1)

    def test_session_storage_fault_targets_the_active_parts_instead_of_unused_packs(self):
        f=self.f
        process=subprocess.Popen([str(OPTIONS.probe),'file://'+str(f.repo),'HEAD','--metrics-only'],cwd=f.cwd,
            env=f.env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        f.children.append(process);f.line(process)
        root=next(f.root.glob('*/remote-repository')).parent
        selected=f"Path({str(root)!r})"
        exec(private_storage_fault('odb',selected),{'Path':Path})
        value,report=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n')
        self.assertEqual(report['metrics']['remote_session_starts'],1)
        exec(private_storage_fault('raw',selected),{'Path':Path,'os':os})
        with self.assertRaisesRegex(AssertionError,'Too many levels of symbolic links'):
            f.operation(process,'read other')
        self.assertNotEqual(process.wait(timeout=10),0)
        self.assertFalse((root/'unexpected-target').exists())

    def test_session_imports_required_delta_pack_without_a_second_fetch(self):
        import hashlib,zlib
        f=self.f;native=shutil.which('git')
        base=b'a'*20000+b'base';wanted=b'a'*20000+b'changed'
        base_oid=f.git('hash-object','-w','--stdin',data=base)
        wanted_oid=f.git('hash-object','-w','--stdin',data=wanted)
        tree=f.git('mktree',data=f'100644 blob {wanted_oid}\tfile\n'.encode())
        commit=f.git('commit-tree',tree,data=b'delta session\n');f.git('update-ref','refs/heads/main',commit)
        def integer(value):
            output=bytearray()
            while True:
                byte=value&127;value>>=7;output.append(byte|(128 if value else 0))
                if not value:return bytes(output)
        def entry(kind,size):
            first=(kind<<4)|(size&15);size>>=4
            return bytes([first|(128 if size else 0)])+(integer(size) if size else b'')
        delta=integer(len(base))+integer(len(wanted))+bytes([0xb0,20000&255,20000>>8,7])+b'changed'
        data=b'PACK'+(2).to_bytes(4,'big')+(2).to_bytes(4,'big')
        data+=entry(3,len(base))+zlib.compress(base)
        data+=entry(7,len(delta))+bytes.fromhex(base_oid)+zlib.compress(delta)
        data+=hashlib.sha1(data).digest()
        tree_pack=run([native,'-C',f.repo,'pack-objects','--stdout'],input=(tree+'\n').encode(),env=f.env)
        tools=f.root/'delta-tools';tools.mkdir();wrapper=tools/'git'
        wrapper.write_text(f'''#!{sys.executable}
import os,sys
native={native!r}
args=sys.argv[1:]
if 'upload-pack' not in args:os.execv(native,[native,*args])
packs={dict([(tree,tree_pack),(wanted_oid,data)])!r}
def send(data):
 sys.stdout.buffer.write(f'{{len(data)+4:04x}}'.encode()+data)
def packet():
 header=sys.stdin.buffer.read(4)
 if not header:raise SystemExit(0)
 length=int(header,16)
 return length if length<4 else sys.stdin.buffer.read(length-4)
send(b'version 2\\n');send(b'fetch=shallow filter\\n');sys.stdout.buffer.write(b'0000');sys.stdout.buffer.flush()
while True:
 wanted=None
 while True:
  value=packet()
  if value==0:break
  if isinstance(value,bytes) and value.startswith(b'want '):wanted=value[5:].strip().decode()
 send(b'packfile\\n')
 for offset in range(0,len(packs[wanted]),60000):send(b'\\x01'+packs[wanted][offset:offset+60000])
 sys.stdout.buffer.write(b'0000');sys.stdout.buffer.flush()
''')
        wrapper.chmod(0o700);f.env['PATH']=str(tools)+os.pathsep+f.env['PATH']
        process=f.probe('file://'+str(f.repo));f.line(process)
        value,first=f.operation(process,'range 19990 100 file')
        self.assertEqual(bytes(value['bytes']),wanted[19990:])
        f.operation(process,'release')
        value,again=f.operation(process,'range 19990 100 file')
        self.assertEqual(bytes(value['bytes']),wanted[19990:])
        for field in ('remote_fetches','remote_fetch_operations','remote_decode_operations','remote_session_fetch_operations','blob_loads'):
            self.assertEqual(first['metrics'][field],again['metrics'][field])
        self.assertEqual(again['metrics']['remote_fetch_operations'],1)
        self.assertEqual(again['metrics']['remote_session_fetch_operations'],2)
        self.assertEqual(again['metrics']['remote_decode_operations'],2,'generic decoder did not run')
        self.assertEqual(set(again['objects']),{commit,tree,base_oid,wanted_oid})
        destination=next(f.root.rglob('remote-repository'))
        self.assertEqual(list((destination/'objects/pack').iterdir()),[])
        f.finish(process)

    def test_native_raw_import_verifies_oid_before_publishing_bytes(self):
        f=self.f;tools=f.root/'raw-decoder-tools';tools.mkdir()
        f.env.update(GIT_CONFIG_COUNT='1',GIT_CONFIG_KEY_0='fetch.fsckObjects',GIT_CONFIG_VALUE_0='false')
        real_git=shutil.which('git')
        wrapper=tools/'git'
        wrapper.write_text(f'''#!{sys.executable}
import os,subprocess,sys
native={real_git!r}
args=sys.argv[1:]
if 'cat-file' in args and '--batch' in args:
 result=subprocess.run([native,*args],input=sys.stdin.buffer.read(),stdout=subprocess.PIPE)
 data=result.stdout
 if b' blob ' in data.split(b'\\n',1)[0]:data=data.replace(b'expected\\n',b'changed!\\n',1)
 sys.stdout.buffer.write(data)
 raise SystemExit(result.returncode)
os.execv(native,[native,*args])
''')
        wrapper.chmod(0o700);f.env['PATH']=str(tools)+os.pathsep+f.env['PATH']
        process=f.probe('file://'+str(f.repo));f.line(process)
        with self.assertRaisesRegex(AssertionError,'Git object hash mismatch'):
            f.operation(process,'read deep/file')
        self.assertNotEqual(process.wait(timeout=10),0)

    @unittest.skipUnless(sys.platform=='linux', 'Linux /proc descriptor and RSS observation')
    def test_remote_pack_descriptors_stay_bounded_with_reduced_rlimit(self):
        import resource
        f=self.f
        entries=[]
        for index in range(192):
            blob=f.git('hash-object','-w','--stdin',data=f'object-{index}\n'.encode())
            entries.append(f'100644 blob {blob}\tf{index:03d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'descriptor fixture\n')
        f.git('update-ref','refs/heads/main',commit)
        limit=64
        def reduced_limit(): resource.setrlimit(resource.RLIMIT_NOFILE,(limit,limit))
        def exercise(binary,cache,expect_failure=False):
            process=subprocess.Popen([str(binary),'file://'+str(f.repo),'HEAD'],cwd=f.cwd,
                env=dict(f.env,XDG_CACHE_HOME=str(cache)),preexec_fn=reduced_limit,
                stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
            f.children.append(process)
            samples=[]; failure=None; last=None
            try:
                f.line(process)
                for index in range(192):
                    value,last=f.operation(process,f'read f{index:03d}')
                    self.assertEqual(value['data'],f'object-{index}\n')
                    if index in (15,63,127,191):
                        samples.append(dict(objects=index+1,descriptors=len(list(Path(f'/proc/{process.pid}/fd').iterdir()))))
                        if not expect_failure:
                            pack_dirs=list(f.root.glob('*/remote-repository/objects/pack'))
                            self.assertEqual(len(pack_dirs),1)
                            self.assertEqual(list(pack_dirs[0].iterdir()),[],'per-demand packs accumulated in the run ODB')
            except (AssertionError,BrokenPipeError) as error:
                failure=str(error)
                process.wait(timeout=10)
            if expect_failure:
                self.assertIsNotNone(failure,'old binary unexpectedly completed; red result unavailable')
                self.assertIn('Too many open files',failure)
            else:
                if failure: self.fail(failure)
                fetches=last['metrics']['remote_fetches']
                value,again=f.operation(process,'read f000')
                self.assertEqual(value['data'],'object-0\n')
                self.assertEqual(again['metrics']['remote_fetches'],fetches)
                f.finish(process)
                self.assertLessEqual(max(x['descriptors'] for x in samples),48)
            print('descriptor evidence:',json.dumps(dict(binary=str(binary),cache=str(cache),rlimit_nofile=limit,samples=samples,failure=failure,metrics=last['metrics'] if last else None)),flush=True)
            return last
        if OPTIONS.before_resource_probe:
            exercise(OPTIONS.before_resource_probe,f.root/'before-cache',True)
        cache=f.root/'resource-cache'
        for _ in range(2):
            report=exercise(OPTIONS.probe,cache)
            self.assertEqual(report['metrics']['blob_loads'],192)
            self.assertEqual(report['metrics']['remote_fetches'],194)
        self.assertFalse(cache.exists())

    @unittest.skipUnless(sys.platform=='linux', 'Linux /proc peak resident-memory observation')
    def test_first_large_loose_range_streams_without_retaining_full_payload(self):
        f=self.f
        for mib in (16,64,128):
            with tempfile.TemporaryFile() as output:
                writer=subprocess.Popen(['git','-C',str(f.repo),'hash-object','-w','--stdin'],env=f.env,stdin=subprocess.PIPE,stdout=output)
                for _ in range(mib): writer.stdin.write(b'x'*(1024*1024))
                writer.stdin.close(); self.assertEqual(writer.wait(),0); output.seek(0); blob=output.read().decode().strip()
            tree=f.git('mktree',data=f'100644 blob {blob}\tlarge\n'.encode())
            for binary in ([OPTIONS.before_resource_probe] if OPTIONS.before_resource_probe else [])+[OPTIONS.probe]:
                process=subprocess.Popen([str(binary),'local',tree],cwd=f.repo,env=f.env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
                f.children.append(process); f.line(process)
                value,report=f.operation(process,'range 1234 16 large')
                self.assertEqual(bytes(value['bytes']),b'x'*16)
                repeated,again=f.operation(process,'range 4321 16 large')
                self.assertEqual(repeated,value)
                self.assertEqual(again['metrics']['spool_writes'],1)
                status=Path(f'/proc/{process.pid}/status').read_text()
                peak_kib=int(re.search(r'^VmHWM:\s+(\d+)',status,re.M).group(1))
                if binary==OPTIONS.probe: self.assertEqual(report['metrics']['streamed_blob_loads'],1)
                print('loose range evidence:',json.dumps(dict(binary=str(binary),object_mib=mib,peak_resident_kib=peak_kib,metrics=report['metrics'])),flush=True)
                f.finish(process)

    def test_million_distinct_file_remote_fresh_runs_bounded_fetch(self):
        if not OPTIONS.million: self.skipTest('million-file acceptance explicitly selected in CI')
        import benchmark
        f=self.f; fixture=benchmark.digit_fixture(f.repo,1_000_000)
        commit=f.git('rev-parse','refs/bench/digits-1000000')
        f.git('update-ref','refs/heads/main',commit); tree=fixture['tree_oid']
        url=f.https(); self.addCleanup(f.server.shutdown)
        selected=f'{random.Random(0x5eed).randrange(1_000_000):06d}'; path='/'.join(selected)
        process=f.probe(url); initial=f.line(process)
        self.assertEqual(set(initial['objects']),{commit,tree}); f.finish(process)
        process=f.probe(url); f.line(process)
        value,read=f.operation(process,'read '+path); self.assertEqual(value['data'],selected+'\n')
        self.assertEqual(len(read['objects']),8)
        self.assertEqual(read['metrics']['blob_loads'],1)
        self.assertEqual(read['metrics']['tree_reads'],6); f.finish(process)
        wanted={oid for request in f.requests for oid in request['wants']}
        self.assertEqual(wanted,set(read['objects']))
        self.assertLess(sum(x['response_bytes'] for x in f.requests),128*1024)
        start=len(f.requests)
        process=f.probe(url); f.line(process); value,_=f.operation(process,'read '+path); f.finish(process)
        self.assertEqual(value['data'],selected+'\n')
        self.assertEqual({oid for request in f.requests[start:] for oid in request['wants']},set(read['objects']))
        fuse_runs=[]
        if OPTIONS.require_fuse:
            for index,command in enumerate((['true'],['cat',path],['cat',path])):
                if index<2: shutil.rmtree(f.root/'cache',ignore_errors=True)
                begin=len(f.requests)
                result=subprocess.run([str(OPTIONS.binary.resolve()),url,'HEAD','--',*command],cwd=f.cwd,env=f.env,capture_output=True,timeout=60)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,b'' if command[0]=='true' else (selected+'\n').encode())
                demands={oid for x in f.requests[begin:] for oid in x['wants']}
                self.assertEqual(demands, {commit,tree} if index==0 else set(read['objects']))
                fuse_runs.append(dict(command=command,returncode=result.returncode,stdout=result.stdout.decode(),objects=sorted(demands)))
        if OPTIONS.evidence_output:
            head=subprocess.check_output(['git','-C',str(Path(__file__).resolve().parents[1]),'rev-parse','HEAD'],text=True).strip()
            OPTIONS.evidence_output.parent.mkdir(parents=True,exist_ok=True)
            OPTIONS.evidence_output.write_text(json.dumps(dict(head=head,files=1000000,selected=selected,objects=read['objects'],fuse_runs=fuse_runs))+'\n')
        print('remote million evidence:',json.dumps(dict(fixture=fixture,selected=selected,objects=read['objects'],requests=f.requests)),flush=True)

    def test_ssh_denied_authentication_stops_before_mount(self):
        f=self.f; url=f.ssh(); (f.root/'authorized').write_text('')
        process=f.probe(url); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0)
        self.assertIn(b'authentication failed',errors)
        self.assertFalse((f.root/'prompts').exists())

    def test_pty_selected_encrypted_identity_one_prompt_multiple_lazy_reads(self):
        f=self.f; url=f.ssh(encrypted=True)
        env=dict(f.env)
        for key in ('SSH_ASKPASS','SSH_ASKPASS_REQUIRE','DISPLAY'): env.pop(key,None)
        pid,fd=pty.fork()
        if pid==0:
            exec_fixture_child([str(OPTIONS.probe),url,'HEAD'],f.cwd,env)
        transcript=bytearray()
        reaped=False
        def await_bytes(needle):
            deadline=time.monotonic()+20
            while needle not in transcript:
                ready,_,_=select.select([fd],[],[],max(0,deadline-time.monotonic()))
                if not ready: raise AssertionError('PTY timed out: '+transcript.decode(errors='replace'))
                try: chunk=os.read(fd,65536)
                except OSError: chunk=b''
                if not chunk: raise AssertionError('PTY closed: '+transcript.decode(errors='replace'))
                transcript.extend(chunk)
        try:
            await_bytes(b'Enter passphrase'); os.write(fd,b'fixture-password\n')
            await_bytes(b'"objects"')
            os.write(fd,b'read deep/file\n'); await_bytes(b'expected\\n')
            os.write(fd,b'read other\n'); await_bytes(b'unrelated payload\\n')
            os.write(fd,b'exit\n')
            deadline=time.monotonic()+10
            while time.monotonic()<deadline:
                done,status=os.waitpid(pid,os.WNOHANG)
                if done:
                    reaped=True
                    self.assertEqual(os.waitstatus_to_exitcode(status),0,transcript); break
                time.sleep(.01)
            else: raise AssertionError('PTY runner did not finish')
            self.assertEqual(transcript.count(b'Enter passphrase'),1,transcript)
            self.assertEqual((f.root/'sshd.log').read_text().count('Accepted publickey'),1)
        finally:
            if not reaped:
                with contextlib.suppress(ProcessLookupError): os.kill(pid,signal.SIGKILL)
                with contextlib.suppress(ChildProcessError): os.waitpid(pid,0)
            os.close(fd)

    def test_real_mount_remote_active_cleanup_and_sigkill_stale_recovery(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.ssh(); ready=f.root/'remote-ready'
        code="import json,os,time; from pathlib import Path; Path('out').mkdir(); Path('out/result').write_text('recover'); ready=Path(os.environ['READY']); pending=ready.with_suffix('.new'); pending.write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)})); pending.replace(ready); time.sleep(300)"
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=dict(f.env,READY=str(ready)),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+20
        while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
        self.assertTrue(ready.exists())
        state=json.loads(ready.read_text()); directory=Path(state['run'])
        control=['ssh','-F','/dev/null','-S',str(directory/'ssh-control'),'-O','check','git-on-control']
        checked=subprocess.run(control,capture_output=True,timeout=5)
        self.assertEqual(checked.returncode,0,checked.stderr)
        master=int(re.search(rb'pid=(\d+)',checked.stderr).group(1))
        master_fd=os.pidfd_open(master)
        def dispose_master():
            if not select.select([master_fd],[],[],0)[0]:
                with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(master_fd,signal.SIGTERM)
            os.close(master_fd)
        self.addCleanup(dispose_master)
        self.assertTrue((directory/'ssh-control-owner').is_file())
        def cleanup():
            with contextlib.suppress(ProcessLookupError): os.killpg(state['pid'],signal.SIGKILL)
            if process.poll() is None: process.kill()
            process.communicate(timeout=10)
            if directory.exists(): subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.addCleanup(cleanup)
        active=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
        self.assertNotEqual(active.returncode,0); self.assertTrue((directory/'backing').exists())
        recovery=json.loads((directory/'.state').read_text())
        pin=recovery['pin_ref']
        self.assertEqual(f.git('rev-parse',pin,at=directory/'remote-repository'),recovery['tree_oid'])
        mountpoint=directory/'mount'
        def mounted():
            return any(line.split()[4]==str(mountpoint) and 'git-on:'+recovery['id'] in line
                       for line in Path('/proc/self/mountinfo').read_text().splitlines())
        self.assertTrue(mounted())
        self.assertEqual((mountpoint/'out/result').read_bytes(),b'recover')
        # Kill the runner alone first. Its child has its own group and no
        # parent-death signal; an external owner must stop that exact fixture.
        process.kill(); self.assertEqual(process.wait(timeout=10),-signal.SIGKILL)
        self.assertTrue(mounted(),'SIGKILL unexpectedly removed the mount')
        self.assertEqual(os.getpgid(state['pid']),state['pid'])
        self.assertNotEqual(Path(f"/proc/{state['pid']}/stat").read_text().split()[2],'Z')
        inaccessible=subprocess.run([sys.executable,'-c',
            'import json,os,sys\ntry: os.listdir(sys.argv[1])\nexcept OSError as e: print(json.dumps({"errno":e.errno}))\nelse: raise AssertionError("disconnected mount remained readable")',str(mountpoint)],
            capture_output=True,timeout=5)
        self.assertEqual(inaccessible.returncode,0,inaccessible.stderr)
        self.assertEqual(json.loads(inaccessible.stdout)['errno'],errno.ENOTCONN)
        self.assertEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertFalse(select.select([master_fd],[],[],0)[0])
        self.assertTrue((directory/'remote-repository').exists())
        self.assertTrue(any(path.is_file() and path.read_bytes()==b'recover'
                            for path in (directory/'backing').iterdir()),'raw writable payload was lost')
        self.assertTrue((directory/'ssh-control-owner').is_file())
        self.assertEqual(f.git('rev-parse',pin,at=directory/'remote-repository'),recovery['tree_oid'])
        # Kernel flock ownership ends with the killed process. Active-run
        # objects and the recovery pin remain in private storage.
        for lockfile in (directory/'.lock',):
            with lockfile.open('rb') as held:
                fcntl.flock(held,fcntl.LOCK_EX|fcntl.LOCK_NB)
                fcntl.flock(held,fcntl.LOCK_UN)
        print('SIGKILL runtime evidence:',json.dumps(dict(mount_remaining=mounted(),
              mount_errno=errno.ENOTCONN,child_alive=True,ssh_master_alive=True,
              run_and_pin_retained=True,locks_released=True)),flush=True)
        os.killpg(state['pid'],signal.SIGKILL); process.communicate(timeout=10)
        stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.assertEqual(stale.returncode,0,stale.stderr); self.assertFalse(directory.exists())
        self.assertFalse(mounted())
        self.assertNotEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertTrue(select.select([master_fd],[],[],5)[0],'recorded owned master survived stale recovery')
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def test_real_mount_all_handled_signals_close_owned_ssh_and_remove_state(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.ssh(); ready=f.root/'child-ready'
        for sig in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGQUIT):
            with self.subTest(signal=sig):
                ready.unlink(missing_ok=True)
                process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--','sh','-c',
                    'printf "%s" "$$" > "$1.new"; mv "$1.new" "$1"; exec sleep 300','fixture',str(ready)],
                    cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                f.children.append(process)
                deadline=time.monotonic()+20
                while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
                self.assertTrue(ready.exists(),'mounted child never began')
                child=int(ready.read_text()); directory,=f.root.glob('git-on-*')
                control=['ssh','-F','/dev/null','-S',str(directory/'ssh-control'),'-O','check','git-on-control']
                self.assertEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
                try:
                    process.send_signal(sig); _,errors=process.communicate(timeout=10)
                    self.assertEqual(process.returncode,-sig,errors)
                    self.assertFalse(directory.exists())
                    self.assertFalse(any(line.split()[4]==str(directory/'mount') for line in Path('/proc/self/mountinfo').read_text().splitlines()))
                    self.assertNotEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
                    proc=Path(f'/proc/{child}/stat')
                    self.assertTrue(not proc.exists() or proc.read_text().split()[2]=='Z')
                finally:
                    if process.poll() is None:
                        with contextlib.suppress(ProcessLookupError): os.killpg(child,signal.SIGKILL)
                        process.kill(); process.communicate(timeout=5)
                    if directory.exists():
                        subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)

    def test_real_mount_sigkill_after_helper_mount_before_child_spawn(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.ssh(); helper=shutil.which('fusermount3')
        self.assertIsNotNone(helper)
        gates=f.root/'mount-helper'; gates.mkdir()
        ready=f.root/'mount-helper-ready'; child=f.root/'unexpected-child'
        wrapper=gates/'fusermount3'
        # Perform the real native mount and pass its FUSE descriptor as usual,
        # then hold the helper exit. This fixes the boundary deterministically:
        # a mounted kernel filesystem, before spawn_mount2 returns or child argv.
        wrapper.write_text(f'#!/bin/sh\nif [ -z "$_FUSE_COMMFD" ]; then exec "{helper}" "$@"; fi\n"{helper}" "$@" || exit $?\nfor point do :; done\nprintf "%s\\n%s\\n" "$$" "$point" > "{ready}.new"\nmv "{ready}.new" "{ready}"\nexec sleep 300\n')
        wrapper.chmod(0o700)
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--','sh','-c',
            'printf started > "$1"','fixture',str(child)],cwd=f.cwd,
            env=dict(f.env,PATH=str(gates)+os.pathsep+f.env['PATH']),
            stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        f.children.append(process); directory=None; helper_pid=None; helper_fd=None
        def dispose():
            nonlocal directory
            if process.poll() is None:
                with contextlib.suppress(ProcessLookupError): os.killpg(process.pid,signal.SIGKILL)
            if helper_fd is not None and not select.select([helper_fd],[],[],0)[0]:
                with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(helper_fd,signal.SIGKILL)
            process.communicate(timeout=10)
            if directory is None:
                candidates=list(f.root.glob('git-on-*'))
                if len(candidates)==1: directory=candidates[0]
            if directory is not None and directory.exists():
                subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
            if helper_fd is not None: os.close(helper_fd)
        self.addCleanup(dispose)
        deadline=time.monotonic()+20
        while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
        self.assertTrue(ready.exists(),'native fusermount helper did not reach held mount boundary')
        helper_text,point=ready.read_text().splitlines(); helper_pid=int(helper_text)
        helper_fd=os.pidfd_open(helper_pid)
        mountpoint=Path(point); directory=mountpoint.parent
        state=json.loads((directory/'.state').read_text())
        def mounted():
            return any(line.split()[4]==str(mountpoint) and 'git-on:'+state['id'] in line
                       for line in Path('/proc/self/mountinfo').read_text().splitlines())
        self.assertTrue(mounted()); self.assertFalse(child.exists())
        process.kill(); self.assertEqual(process.wait(timeout=10),-signal.SIGKILL)
        self.assertTrue(mounted()); self.assertFalse(child.exists())
        control=['ssh','-F','/dev/null','-S',str(directory/'ssh-control'),'-O','check','git-on-control']
        self.assertEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        # The held helper belongs only to this isolated fixture. It has no FUSE
        # descriptor after native fusermount exited and cannot serve the mount.
        signal.pidfd_send_signal(helper_fd,signal.SIGKILL)
        process.communicate(timeout=10)
        stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.assertEqual(stale.returncode,0,stale.stderr)
        self.assertFalse(mounted()); self.assertFalse(directory.exists())
        self.assertNotEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        print('SIGKILL mount boundary evidence:',json.dumps(dict(child_started=False,
              mount_remaining_after_kill=True,ssh_master_retained=True,recovery_removed_mount_and_state=True)),flush=True)

    def test_review_real_mount_failed_ssh_shutdown_retains_recovery_and_pin(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        if OPTIONS.before_lifecycle_binary:
            with self.assertRaisesRegex(AssertionError,'0 != 125'):
                self.failed_ssh_shutdown_retains_recovery_and_pin(OPTIONS.before_lifecycle_binary)
            print('RED-BEFORE mounted SSH shutdown: old runner returned0 and removed recovery for a live master',flush=True)
        self.failed_ssh_shutdown_retains_recovery_and_pin(OPTIONS.binary)

    def failed_ssh_shutdown_retains_recovery_and_pin(self,binary):
        f=Fixture(); self.addCleanup(f.close)
        url=f.ssh(); ssh=str(Path(shutil.which('ssh')).resolve())
        selected=f.root/'selected-client'; original=f.root/'original-client'
        command=shlex.quote(ssh)+f.env.pop('GIT_SSH_COMMAND')[3:]
        selected.write_text('#!/bin/sh\nexec '+command+' "$@"\n'); selected.chmod(0o700)
        f.env['GIT_SSH']=str(selected)
        programs=f.root/'git-only-path'; programs.mkdir()
        (programs/'git').symlink_to(Path(shutil.which('git')).resolve())
        f.env['PATH']=str(programs)
        self.assertIsNone(shutil.which('ssh',path=f.env['PATH']))
        ready=f.root/'child-ready'; release=f.root/'child-release'
        code='import os,time; from pathlib import Path; ready=Path(os.environ["READY"]); pending=ready.with_suffix(".new"); pending.write_text(str(Path.cwd().parent)); pending.replace(ready)\nwhile not Path(os.environ["RELEASE"]).exists(): time.sleep(.01)'
        # The mounted CLI also needs native fusermount on PATH. Keep SSH absent.
        fusermount=Path(shutil.which('fusermount3')).resolve()
        (programs/'fusermount3').symlink_to(fusermount)
        process=subprocess.Popen([str(binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],
            cwd=f.cwd,env=dict(f.env,READY=str(ready),RELEASE=str(release)),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process); directory=None; master_fd=None
        def dispose():
            if original.exists(): original.replace(selected)
            release.touch()
            if process.poll() is None: process.terminate()
            try: process.communicate(timeout=10)
            except subprocess.TimeoutExpired: process.kill(); process.communicate(timeout=5)
            if directory is not None and directory.exists():
                subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
            if master_fd is not None:
                if not select.select([master_fd],[],[],0)[0]:
                    with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(master_fd,signal.SIGTERM)
                os.close(master_fd)
        self.addCleanup(dispose)
        deadline=time.monotonic()+20
        while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
        self.assertTrue(ready.exists(),'mounted child never began')
        directory=Path(ready.read_text()); state=json.loads((directory/'.state').read_text())
        checked=subprocess.run([ssh,'-F','/dev/null','-S',str(directory/'ssh-control'),'-O','check','git-on-control'],capture_output=True,timeout=5)
        self.assertEqual(checked.returncode,0,checked.stderr)
        master_fd=os.pidfd_open(int(re.search(rb'pid=(\d+)',checked.stderr).group(1)))
        selected.rename(original)
        selected.write_text('#!/bin/sh\nexit 99\n'); selected.chmod(0o700)
        release.touch(); _,errors=process.communicate(timeout=15)
        if binary==OPTIONS.before_lifecycle_binary:
            self.assertFalse(select.select([master_fd],[],[],0)[0],'old runner closed master through a client unexpectedly on PATH')
            self.assertFalse(directory.exists(),'old runner unexpectedly retained recovery')
        self.assertEqual(process.returncode,125,errors)
        self.assertIn(b'recorded SSH shutdown executable changed',errors)
        self.assertIn(str(directory).encode(),errors)
        self.assertTrue((directory/'ssh-control-owner').is_file())
        self.assertFalse(select.select([master_fd],[],[],0)[0])
        self.assertEqual(f.git('rev-parse',state['pin_ref'],at=directory/'remote-repository'),state['tree_oid'])
        self.assertFalse(any(line.split()[4]==str(directory/'mount') for line in Path('/proc/self/mountinfo').read_text().splitlines()))
        original.replace(selected)
        stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.assertEqual(stale.returncode,0,stale.stderr); self.assertFalse(directory.exists())
        self.assertTrue(select.select([master_fd],[],[],5)[0])

    def test_real_mount_remote_busy_retention_and_cleanup(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        ready=f.root/'escaped'; runfile=f.root/'busy-run'
        child=f"import os,time; from pathlib import Path; Path({str(ready)!r}).write_text(str(os.getpid())); time.sleep(300)"
        code="import os,subprocess,sys,time; from pathlib import Path; Path(sys.argv[2]).write_text(str(Path.cwd().parent)); Path('out').mkdir(); Path('out/result').write_text('retained'); subprocess.Popen([sys.executable,'-c',sys.argv[1]],start_new_session=True,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); deadline=time.monotonic()+5\nwhile not Path(sys.argv[3]).exists() and time.monotonic()<deadline: time.sleep(.01)\nassert Path(sys.argv[3]).exists()"
        directory=None
        try:
            result=subprocess.run([str(OPTIONS.binary.resolve()),url,'HEAD','--',sys.executable,'-c',code,child,str(runfile),str(ready)],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
            self.assertEqual(result.returncode,125,result.stderr)
            directory=Path(runfile.read_text())
            self.assertTrue((directory/'remote-repository').exists())
            self.assertIn(str(directory/'mount'),[line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()])
            refused=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
            self.assertNotEqual(refused.returncode,0)
        finally:
            if ready.exists():
                with contextlib.suppress(ProcessLookupError): os.killpg(int(ready.read_text()),signal.SIGKILL)
            if directory is not None:
                stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
                self.assertEqual(stale.returncode,0,stale.stderr)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def stalled_mount_shutdown(self, binary, escaped, expect_old):
        f=self.f; url=self.stall_url
        shutil.rmtree(f.root/'cache',ignore_errors=True)
        f.stalled.clear(); f.resume.clear(); f.stalled_oids={f.blob}
        ready=f.root/'stall-ready'; escaped_pid=f.root/'stall-escaped'; exit_file=f.root/'leader-exit'
        for path in (ready,escaped_pid,exit_file): path.unlink(missing_ok=True)
        reader="import os,time; from pathlib import Path; Path(os.environ['ESCAPED_PID']).write_text(str(os.getpid()))\ntry:\n open('deep/file').read()\nexcept OSError:\n pass\ntime.sleep(300)"
        code="import json,os,subprocess,sys,time; from pathlib import Path; Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)}))\n"
        if escaped:
            code+="subprocess.Popen([sys.executable,'-c',sys.argv[1]],start_new_session=True,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\nwhile not Path(os.environ['LEADER_EXIT']).exists(): time.sleep(.01)\n"
        else: code+="open('deep/file').read()\n"
        process=subprocess.Popen([str(binary.resolve()),url,'HEAD','--',sys.executable,'-c',code,reader],cwd=f.cwd,env=dict(f.env,READY=str(ready),ESCAPED_PID=str(escaped_pid),LEADER_EXIT=str(exit_file)),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process); directory=None
        try:
            self.assertTrue(f.stalled.wait(timeout=20),'lazy fetch never reached controlled server stall')
            self.assertTrue(ready.exists()); state=json.loads(ready.read_text()); directory=Path(state['run'])
            if escaped: exit_file.write_text('exit successfully')
            else: os.killpg(state['pid'],signal.SIGTERM)
            try:
                _,errors=process.communicate(timeout=8)
                stayed_blocked=False
            except subprocess.TimeoutExpired:
                stayed_blocked=True
                process.kill(); f.resume.set(); process.communicate(timeout=10)
                errors=b'runner stayed blocked after child completion'
            if expect_old:
                self.assertTrue(stayed_blocked,'before-fix binary did not reproduce stalled shutdown: '+repr(errors))
            else:
                self.assertFalse(stayed_blocked,'runner stayed blocked after child completion')
                self.assertEqual(process.returncode,125 if escaped else -signal.SIGTERM,errors)
                if escaped: self.assertTrue(directory.exists(),'busy escaped cwd must preserve recovery state')
                else: self.assertFalse(directory.exists(),'signaled command must clean its run')
            print('shutdown regression evidence:',json.dumps(dict(binary=str(binary),escaped=escaped,before=expect_old,observed_stall=stayed_blocked)),flush=True)
        finally:
            f.resume.set(); f.stalled_oids.clear()
            if process.poll() is None: process.kill(); process.communicate(timeout=10)
            if ready.exists():
                with contextlib.suppress(ProcessLookupError): os.killpg(json.loads(ready.read_text())['pid'],signal.SIGKILL)
            if escaped_pid.exists():
                with contextlib.suppress(ProcessLookupError): os.killpg(int(escaped_pid.read_text()),signal.SIGKILL)
            if directory is not None and directory.exists():
                cleanup=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
                self.assertEqual(cleanup.returncode,0,cleanup.stderr)

    def test_review_real_mount_stalled_lazy_fetch_shutdown(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; self.stall_url=f.https(); self.addCleanup(f.server.shutdown)
        for escaped in (False,True):
            with self.subTest(escaped=escaped):
                if OPTIONS.before_binary: self.stalled_mount_shutdown(OPTIONS.before_binary,escaped,True)
                self.stalled_mount_shutdown(OPTIONS.binary,escaped,False)

    def test_review_real_mount_caught_run_odb_fault_forces_runner_failure(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        for fault in ('odb','raw'):
            with self.subTest(fault=fault):
                env=dict(f.env)
                if fault=='odb':
                    env.update(GIT_CONFIG_COUNT='1',GIT_CONFIG_KEY_0='fetch.fsckObjects',GIT_CONFIG_VALUE_0='false')
                code="import errno,os; from pathlib import Path\n"+private_storage_fault(fault)
                code+="try:\n open('deep/file').read()\nexcept OSError as e:\n assert e.errno == errno.EIO\nelse:\n raise AssertionError('private storage fault not observed')\n"
                if fault=='raw':code+="assert not target.exists()\n"
                result=subprocess.run([str(OPTIONS.binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=env,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,125,result.stderr)
                self.assertIn(b'remote object loading failed',result.stderr)
                self.assertIn(b'execution incomplete',result.stderr)
                self.assertEqual(list(f.root.glob('git-on-*')),[])

    def ignore_policy_fixture(self):
        f=self.f
        f.ignore=f.git('hash-object','-w','--stdin',data=b'created.tmp\ncreated-dir/\n')
        f.tree=f.git('mktree',data=f'100644 blob {f.ignore}\t.gitignore\n100644 blob {f.other}\tother\n'.encode())
        f.commit=f.git('commit-tree',f.tree,data=b'ignore acquisition fixture\n')
        f.git('update-ref','refs/heads/main',f.commit)
        url=f.https(); self.addCleanup(f.server.shutdown)
        return url

    def test_review_real_mount_ignore_acquisition_faults_report_eio(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=self.ignore_policy_fixture()
        cases=([(OPTIONS.before_policy_binary,errno.EROFS)] if OPTIONS.before_policy_binary else [])+[(OPTIONS.binary,errno.EIO)]
        for binary,expected in cases:
            faults=('transport','odb') if binary==OPTIONS.before_policy_binary else ('transport','odb','raw')
            for fault in faults:
                for operation,path in (('create','created.tmp'),('mkdir','created-dir')):
                    with self.subTest(binary=str(binary),fault=fault,operation=operation):
                        shutil.rmtree(f.root/'cache',ignore_errors=True)
                        f.requests.clear(); f.denied_oids={f.ignore} if fault=='transport' else set()
                        code="import json,os,sys; from pathlib import Path\n"
                        if fault!='transport':code+=private_storage_fault(fault)
                        code+="try:\n"
                        code+=(" fd=os.open(sys.argv[1],os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600); os.close(fd)\n" if operation=='create' else " os.mkdir(sys.argv[1])\n")
                        code+="except OSError as error:\n print(json.dumps({'errno':error.errno,'exists':os.path.lexists(sys.argv[1])}),flush=True)\nelse:\n raise AssertionError('ignore acquisition fault permitted mutation')\n"
                        if fault=='raw':code+="assert not target.exists()\n"
                        env=dict(f.env)
                        if fault=='odb':env.update(GIT_CONFIG_COUNT='1',GIT_CONFIG_KEY_0='fetch.fsckObjects',GIT_CONFIG_VALUE_0='false')
                        result=subprocess.run([str(binary),url,'HEAD','--',sys.executable,'-c',code,path],cwd=f.cwd,env=env,capture_output=True,timeout=30)
                        self.assertEqual(result.returncode,125,result.stderr)
                        self.assertEqual(json.loads(result.stdout),{'errno':expected,'exists':False},result.stderr)
                        self.assertIn(b'execution incomplete',result.stderr)
                        if fault=='transport':
                            self.assertTrue(any(f.ignore in request['wants'] and request.get('status')==403 for request in f.requests),f.requests)
                            self.assertIn(f.ignore.encode(),result.stderr)
                        else: self.assertIn(b'remote object loading failed',result.stderr)
                        self.assertEqual(list(f.root.glob('git-on-*')),[])
                        print('ignore acquisition errno regression evidence:',json.dumps(dict(binary=str(binary),before=expected==errno.EROFS,fault=fault,operation=operation,errno=expected)),flush=True)
        # With healthy rules, policy denial retains its distinct EROFS meaning.
        f.denied_oids.clear(); shutil.rmtree(f.root/'cache',ignore_errors=True)
        code="import errno,os\nfor operation in (lambda: os.open('denied',os.O_CREAT|os.O_WRONLY,0o600),lambda: os.mkdir('denied-dir')):\n try: operation()\n except OSError as error: assert error.errno==errno.EROFS,error\n else: raise AssertionError('unignored mutation allowed')\nfd=os.open('created.tmp',os.O_CREAT|os.O_WRONLY,0o600); os.close(fd)\nos.mkdir('created-dir')\n"
        result=subprocess.run([str(OPTIONS.binary),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def test_review_real_mount_interrupted_ignore_acquisition_preserves_eintr(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=self.ignore_policy_fixture()
        cases=([OPTIONS.before_policy_binary] if OPTIONS.before_policy_binary else [])+[OPTIONS.binary]
        for binary in cases:
            for operation,path in (('create','created.tmp'),('mkdir','created-dir')):
                with self.subTest(binary=str(binary),operation=operation):
                    shutil.rmtree(f.root/'cache',ignore_errors=True)
                    f.stalled.clear(); f.resume.clear(); f.stalled_oids={f.ignore}
                    ready=f.root/'ignore-request-ready'; handled=f.root/'ignore-request-handled'
                    observed=f.root/'ignore-request-errno'; release=f.root/'ignore-request-release'
                    for marker in (ready,handled,observed,release): marker.unlink(missing_ok=True)
                    # ctypes observes the original syscall errno; Python's open
                    # and mkdir wrappers otherwise retry EINTR automatically.
                    code="import ctypes,errno,json,os,signal,sys,time; from pathlib import Path\nlibc=ctypes.CDLL(None,use_errno=True)\ndef handler(*args): Path(os.environ['HANDLED']).touch()\nsignal.signal(signal.SIGTERM,handler)\nPath(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)}))\n"
                    code+=("result=libc.open(os.fsencode(sys.argv[1]),os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)\n" if operation=='create' else "result=libc.mkdir(os.fsencode(sys.argv[1]),0o700)\n")
                    code+="error=ctypes.get_errno()\nassert result==-1,(result,error)\nassert error==errno.EINTR,error\nassert not os.path.lexists(sys.argv[1])\nPath(os.environ['OBSERVED']).write_text(str(error))\nwhile not Path(os.environ['RELEASE']).exists(): time.sleep(.01)\n"
                    code+=("fd=os.open(sys.argv[1],os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600); os.close(fd)\n" if operation=='create' else "os.mkdir(sys.argv[1])\n")
                    env=dict(f.env,READY=str(ready),HANDLED=str(handled),OBSERVED=str(observed),RELEASE=str(release))
                    process=subprocess.Popen([str(binary),url,'HEAD','--',sys.executable,'-c',code,path],cwd=f.cwd,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                    f.children.append(process); state=None
                    try:
                        self.assertTrue(f.stalled.wait(timeout=20),'ignore request did not reach controlled acquisition stall')
                        self.assertTrue(ready.exists()); state=json.loads(ready.read_text())
                        os.kill(state['pid'],signal.SIGTERM)
                        deadline=time.monotonic()+5
                        while not observed.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.01)
                        self.assertTrue(observed.exists(),'ignore request failed to reply EINTR before server release')
                        self.assertEqual(int(observed.read_text()),errno.EINTR)
                        self.assertTrue(handled.exists(),'requester signal handler did not run')
                        f.resume.set(); release.touch()
                        _,errors=process.communicate(timeout=20)
                        self.assertEqual(process.returncode,0,errors)
                        self.assertNotIn(b'execution incomplete',errors)
                        self.assertEqual(list(f.root.glob('git-on-*')),[])
                        print('ignore interruption regression evidence:',json.dumps(dict(binary=str(binary),operation=operation,errno=errno.EINTR,retry_succeeded=True)),flush=True)
                    finally:
                        f.resume.set(); f.stalled_oids.clear(); release.touch()
                        if process.poll() is None:
                            if state is not None:
                                with contextlib.suppress(ProcessLookupError): os.killpg(state['pid'],signal.SIGKILL)
                            process.kill()
                        process.communicate(timeout=20)
                        if state is not None and Path(state['run']).exists():
                            cleanup=subprocess.run([str(OPTIONS.binary),'--cleanup',state['run']],capture_output=True,timeout=20)
                            self.assertEqual(cleanup.returncode,0,cleanup.stderr)

    def request_signal_behavior(self, binary, mode, expect_old):
        f=self.f
        shutil.rmtree(f.root/'cache',ignore_errors=True)
        f.stalled.clear(); f.resume.clear(); f.stalled_oids={f.blob}
        ready=f.root/'request-ready'; handled=f.root/'request-handled'
        ready.unlink(missing_ok=True); handled.unlink(missing_ok=True)
        code="import json,os,signal,sys; from pathlib import Path\n"
        if mode=='handled-exit':
            code+="def handle(*args):\n Path(os.environ['HANDLED']).touch()\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM,handle)\n"
        elif mode=='handled-retry':
            code+="def handle(*args):\n Path(os.environ['HANDLED']).touch()\nsignal.signal(signal.SIGTERM,handle)\n"
        code+="Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)}))\nassert open('deep/file').read()=='expected\\n'\nprint('read completed')\n"
        process=subprocess.Popen([str(binary.resolve()),self.signal_url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=dict(f.env,READY=str(ready),HANDLED=str(handled)),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process); directory=None; state=None
        try:
            self.assertTrue(f.stalled.wait(timeout=20),'request did not reach controlled transport stall')
            self.assertTrue(ready.exists()); state=json.loads(ready.read_text()); directory=Path(state['run'])
            os.killpg(state['pid'],signal.SIGINT if mode=='python-int' else signal.SIGTERM)
            if mode=='handled-retry':
                deadline=time.monotonic()+5
                while not handled.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.01)
                stayed_blocked=not handled.exists()
                if not stayed_blocked: f.resume.set()
            else:
                try:
                    output,errors=process.communicate(timeout=5)
                    stayed_blocked=False
                except subprocess.TimeoutExpired:
                    stayed_blocked=True
            if expect_old:
                self.assertTrue(stayed_blocked,'old binary did not reproduce pending requester signal')
            else:
                self.assertFalse(stayed_blocked,'pending requester signal did not interrupt native demand')
                if mode=='handled-retry': output,errors=process.communicate(timeout=15)
                self.assertEqual(process.returncode,-signal.SIGINT if mode=='python-int' else 0,errors)
                if mode=='handled-retry': self.assertEqual(output,b'read completed\n')
                if mode!='python-int': self.assertTrue(handled.exists(),'custom signal handler did not run')
                self.assertNotIn(b'execution incomplete',errors)
                self.assertFalse(directory.exists(),'completed command did not clean its run')
            print('request signal regression evidence:',json.dumps(dict(binary=str(binary),mode=mode,before=expect_old,observed_stall=stayed_blocked)),flush=True)
        finally:
            if process.poll() is None:
                if state is not None:
                    with contextlib.suppress(ProcessLookupError): os.killpg(state['pid'],signal.SIGKILL)
                process.kill()
            f.resume.set(); f.stalled_oids.clear()
            process.communicate(timeout=15)
            if directory is not None and directory.exists():
                cleanup=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
                self.assertEqual(cleanup.returncode,0,cleanup.stderr)

    def test_review_real_mount_pending_request_signals_preserve_handlers(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; self.signal_url=f.https(); self.addCleanup(f.server.shutdown)
        for mode in ('python-int','handled-exit','handled-retry'):
            with self.subTest(mode=mode):
                if OPTIONS.before_binary: self.request_signal_behavior(OPTIONS.before_binary,mode,True)
                self.request_signal_behavior(OPTIONS.binary,mode,False)

    def test_native_failed_verification_never_executes_command(self):
        f=self.f; signed,allowed=self.signing(); sentinel=f.root/'command-executed'
        allowed.write_text('')
        for revision in (signed,f.commit,f.tree):
            result=subprocess.run([str(OPTIONS.binary.resolve()),'--verify',revision,'--',sys.executable,'-c',f'from pathlib import Path; Path({str(sentinel)!r}).touch()'],cwd=f.repo,env=f.env,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,125,result.stderr)
            self.assertTrue(b'execution forbidden' in result.stderr or b'raw trees' in result.stderr,result.stderr)
            self.assertFalse(sentinel.exists())
            self.assertEqual(list(f.root.glob('git-on-*')),[])

if __name__=='__main__': unittest.main(argv=[__file__,*ARGS])
