#!/usr/bin/env python3
"""Deterministic native Git, HTTPS and OpenSSH acceptance. Keys are disposable.

The source probe runs the production loader without a mount. --require-fuse
and --require-nfs also execute the CLI through their native mounts.
"""
import argparse
import base64
import contextlib
import errno
import fcntl
import http.server
import json
import os
import pty
from pathlib import Path
import pwd
import queue
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
import termios
import threading
import time
import unittest
import unicodedata

from support.git_invocations import git_invocations, record_git_invocations

ROOT = Path(__file__).resolve().parents[1]
PARSER = argparse.ArgumentParser()
PARSER.add_argument('--probe', type=Path, default=ROOT / 'target/debug/examples/source-probe')
PARSER.add_argument('--binary', type=Path, default=ROOT / 'target/debug/git-on')
PARSER.add_argument('--require-fuse', action='store_true')
PARSER.add_argument('--require-nfs', action='store_true')
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

def run_directories(root):
    # Red-before comparisons also run legacy binaries with a flat layout.
    return [*root.glob('git-on/??????'), *root.glob(f'.go{os.geteuid():x}/??????'),
            *root.glob('git-on-*')]

def assert_no_run_state(test, root):
    groups = (root / 'git-on', root / f'.go{os.geteuid():x}')
    records = [record for group in groups for pattern in ('.git-on-run-*', '.git-on-remove-*')
               for record in group.glob(pattern)]
    test.assertEqual(run_directories(root) + records + list(root.glob('.git-on-service-*')), [])

def assert_run_unlocked(directory):
    record = directory.parent / f'.git-on-run-{os.geteuid():x}-{directory.name}'
    lock = next((candidate for candidate in (*record.glob('.git-on-lock-v2-*'),
                                            directory / '.git-on-lock-v2')
                 if candidate.is_file()), directory / '.lock')
    fd = os.open(lock, os.O_RDWR)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    finally:
        os.close(fd)

def fuse_mount_source(point):
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        if line.split()[4] == str(point):
            return line.partition(' - ')[2].split()[1]
    return ''

def run(argv, **kwargs):
    result=subprocess.run(list(map(str, argv)), capture_output=True, **kwargs)
    if result.returncode: raise RuntimeError('fixture command failed: '+result.stderr.decode())
    return result.stdout

def terminal_lines(transcript,columns=80):
    """Replay the cursor movement and line erasure used by the terminal UI."""
    rows=[[]];row=column=0
    for token in re.finditer(r'\x1b\[([0-?]*)[ -/]*([@-~])|([^\x1b])',transcript.decode('utf-8',errors='replace')):
        parameters,control,character=token.groups()
        if control:
            number=int(parameters or '0') if (parameters or '0').isdigit() else 0
            if control=='A':row=max(0,row-(number or 1))
            elif control=='B':row+=number or 1
            elif control=='K':
                if number==2:rows[row]=[]
                elif number==0:rows[row]=rows[row][:column]
            while row>=len(rows):rows.append([])
        elif character=='\r':column=0
        elif character=='\n':row+=1
        elif character=='\b':column=max(0,column-1)
        elif ord(character)>=32:
            if column>=columns:row+=1;column=0
            while row>=len(rows):rows.append([])
            while len(rows[row])<=column:rows[row].append(' ')
            rows[row][column]=character;column+=1
        while row>=len(rows):rows.append([])
    return '\n'.join(''.join(line).rstrip() for line in rows)

def git_on_environment(binary, environment, tools):
    # Git dispatches by the git-on basename even when --binary names an artifact.
    tools.mkdir()
    (tools / 'git-on').symlink_to(binary.resolve())
    return dict(environment, PATH=str(tools)+os.pathsep+environment['PATH'])

def private_odb_fault(root="Path.cwd().parent"):
    """Block the pack directory before a native object demand."""
    return (f"repository={root}/'remote-repository'; packs=repository/'pack-staging'; "
            "packs=packs if packs.is_symlink() else repository/'objects/pack'; "
            "packs.rename(packs.with_name('blocked-packs')); packs.write_bytes(b'blocked')\n")

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
        self.git('config', 'uploadpackfilter.tree.maxDepth', '0')
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
        self.stalled_requests = queue.Queue()
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
    def https(self, authorization=None, transfer_delay=0):
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
                if authorization is not None and self.headers.get('Authorization') != authorization:
                    self.send_response(401)
                    self.send_header('WWW-Authenticate','Basic realm="git-on-fixture"')
                    self.send_header('Content-Length','0'); self.end_headers()
                    return
                wants = [x.decode() for x in re.findall(rb'want ([0-9a-f]{40})', data)]
                stalled_request = bool(fixture.stalled_oids.intersection(wants))
                if stalled_request:
                    fixture.stalled_requests.put(wants)
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
                    elif name.lower() != b'content-length':
                        fields.append((name.decode(), value.strip().decode()))
                try:
                    self.send_response(status)
                    for name, value in fields: self.send_header(name, value)
                    self.send_header('Content-Length', str(len(body))); self.end_headers()
                    if transfer_delay and wants:
                        for offset in range(0,len(body),8192):
                            self.wfile.write(body[offset:offset+8192]);self.wfile.flush();time.sleep(transfer_delay)
                    else:self.wfile.write(body)
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
    def ssh(self, encrypted=False, protocol_v2=False, max_sessions=None, accept_env=()):
        sshd = shutil.which('sshd') or '/usr/sbin/sshd'
        if not Path(sshd).exists(): raise RuntimeError('OpenSSH server missing; install openssh-server')
        host, identity = self.root/'host', self.root/'identity'
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', host])
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', 'fixture-password' if encrypted else '', '-f', identity])
        authorized = self.root/'authorized'; authorized.write_bytes((self.root/'identity.pub').read_bytes())
        with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port = sock.getsockname()[1]
        config = self.root/'sshd_config'
        config.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nPidFile {self.root}/sshd.pid\nAuthorizedKeysFile {authorized}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nLogLevel VERBOSE\n')
        accepted = ('GIT_PROTOCOL', *accept_env) if protocol_v2 else accept_env
        if accepted:
            with config.open('a') as output:output.write('AcceptEnv ' + ' '.join(accepted) + '\n')
        if max_sessions is not None:
            with config.open('a') as output:output.write(f'MaxSessions {max_sessions}\n')
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
    def force_upload_pack(self, commands):
        wrapper = self.root / 'fixture-upload-pack'
        wrapper.write_text('#!/bin/sh\n' + commands)
        wrapper.chmod(0o700)
        authorized = self.root / 'authorized'
        authorized.write_text(f'command="{wrapper}" ' + authorized.read_text())
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

    def test_git_on_uses_selected_nonstandard_binary_over_path_fallback(self):
        selected = self.root / 'chosen-runner'
        selected.write_text('#!/bin/sh\nprintf selected\n')
        selected.chmod(0o755)
        fallback = self.tools / 'git-on'
        fallback.write_text('#!/bin/sh\nprintf wrong-runner\nexit 77\n')
        fallback.chmod(0o755)
        environment = dict(self.env, PATH=str(self.tools)+os.pathsep+self.env['PATH'])
        environment = git_on_environment(selected, environment, self.root / 'selected-tools')
        result = subprocess.run([self.native_git, 'on'], cwd=self.root,
                                env=environment, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'selected')

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
        assert_no_run_state(self, self.root)
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
        assert_no_run_state(self, self.root)

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
    def fetched_oid(self, source, revision, *, filtered=True):
        """Native fetch is the oracle; the caller repository is never its target."""
        f=self.f
        with tempfile.TemporaryDirectory(dir=f.root,prefix='fetch-oracle-') as directory:
            f.git('init','--bare','--quiet',directory,at=f.root)
            result=subprocess.run(['git','-C',directory,'fetch','--no-tags',
                                   '--filter=tree:0' if filtered else '--no-filter',
                                   '--refmap=','--',source,revision],
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

    def test_eager_copy_rejects_configuration_rejected_by_native_fetch(self):
        f = self.f
        source = f.repo.as_uri()
        oracle = f.root / 'native-fetch.git'
        f.git('init', '--bare', '--quiet', oracle, at=f.root)
        invalid_config = {
            'core.abbrev': 'invalid',
            'fetch.parallel': '-1',
        }
        for key, value in invalid_config.items():
            with self.subTest(key=key):
                env = dict(f.env, GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0=key,
                           GIT_CONFIG_VALUE_0=value)
                native = subprocess.run(
                    ['git', '-C', oracle, 'fetch', '--no-tags', '--depth=1', '--no-filter', '--keep', '--',
                     source, f.commit],
                    env=env, capture_output=True, timeout=15,
                )
                self.assertNotEqual(native.returncode, 0, native.stdout)
                self.assertIn(key.encode(), native.stderr)
                result = subprocess.run(
                    [OPTIONS.binary, source, f.commit, '--backend=copy', '--write-policy=all',
                     '--', 'printf', 'child-ran'],
                    cwd=f.cwd, env=env, capture_output=True, timeout=15,
                )
                self.assertEqual(result.returncode, 125, (result.stdout, result.stderr))
                self.assertEqual(result.stdout, b'')
                self.assertIn(key.encode(), result.stderr)

    def test_eager_file_copy_streams_from_an_ordinary_clone(self):
        f = self.f
        source = f.repo.as_uri()
        f.git('clone', '--quiet', source, f.cwd, at=f.root)
        f.git('config', 'core.quotePath', 'off', at=f.cwd)
        alias = f.root / 'répo with space.git'
        alias.symlink_to(f.repo, target_is_directory=True)
        environment, log = record_git_invocations(f.root, f.env)
        code = r"""
from pathlib import Path
import sys
assert Path('deep/file').read_bytes() == b'expected\n'
repo = Path.cwd().parent / 'remote-repository'
assert sys.argv[1] != 'plain' or not repo.exists()
assert not list((repo / 'objects/pack').glob('*'))
"""
        for source, revision, spelling in (
            (source, f.commit, 'plain'), (source, '+main', 'plain'),
            ('../server.git', f.commit, 'relative'), (alias.as_uri(), f.commit, 'escaped'),
            ('../' + alias.name, f.commit, 'unicode'),
        ):
            with self.subTest(source=source, revision=revision):
                log.write_text('')
                result = subprocess.run(
                    [OPTIONS.binary, source, revision, '--backend=copy', '--write-policy=all',
                     '--', sys.executable, '-I', '-S', '-c', code, spelling],
                    cwd=f.cwd, env=environment,
                    capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                starts = git_invocations(log)
                self.assertTrue(starts)
                self.assertFalse([argv for argv in starts if 'fetch' in argv], starts)

    def test_fetch_uses_native_configuration_when_git_config_is_set(self):
        f = self.f
        f.git('config', 'protocol.file.allow', 'never')
        config = f.repo / 'config'
        original = config.read_bytes()
        alternate = f.root / 'config-builtin-only'
        alternate.touch()
        env = dict(f.env, GIT_CONFIG=str(alternate))
        source = f.repo.as_uri()
        native = subprocess.run(['git', '-C', f.repo, 'ls-remote', '--', source],
                                env=env, capture_output=True, timeout=15)
        self.assertNotEqual(native.returncode, 0, native.stdout)
        self.assertIn(b"transport 'file' not allowed", native.stderr)
        result = subprocess.run(
            [OPTIONS.binary, source, 'main', '--backend=copy', '--write-policy=all',
             '--', 'printf', 'child-ran'],
            cwd=f.repo, env=env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 125, (result.stdout, result.stderr))
        self.assertIn(b"transport 'file' not allowed", result.stderr)
        self.assertEqual(result.stdout, b'')
        self.assertEqual(config.read_bytes(), original)
        self.assertEqual(alternate.read_bytes(), b'')

    def test_local_remote_uses_the_same_namespace_as_native_fetch(self):
        f = self.f
        blob = f.git('hash-object', '-w', '--stdin', data=b'namespaced\n')
        subtree = f.git('mktree', data=f'100644 blob {blob}\tfile\n'.encode())
        tree = f.git('mktree', data=f'040000 tree {subtree}\tdeep\n'.encode())
        commit = f.git('commit-tree', tree, data=b'namespace fixture\n')
        f.git('update-ref', 'refs/namespaces/fixture/refs/heads/main', commit)
        f.git('symbolic-ref', 'refs/namespaces/fixture/HEAD',
              'refs/namespaces/fixture/refs/heads/main')
        f.env['GIT_NAMESPACE'] = 'fixture'
        for source in (str(f.repo), f.repo.as_uri()):
            self.assertEqual(self.fetched_oid(source, 'main', filtered=False), commit)
            for native in (False, True):
                with self.subTest(source=source, native=native):
                    env = dict(f.env)
                    if native:
                        env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='fetch.parallel',
                                   GIT_CONFIG_VALUE_0='1')
                    result = subprocess.run(
                        [OPTIONS.binary, source, 'main', '--backend=copy', '--write-policy=all',
                         '--', 'sh', '-c', 'printf "%s\\n" "$GIT_ON_TREE_OID"; cat deep/file'],
                        cwd=f.cwd, env=env, capture_output=True, timeout=20,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, tree.encode() + b'\nnamespaced\n')

    @unittest.skipUnless(sys.platform == 'darwin', 'native Git precomposes arguments on macOS')
    def test_eager_file_copy_selects_decomposed_ref_as_native_git(self):
        f = self.f
        composed = 'refs/heads/pr\N{LATIN SMALL LETTER U WITH DIAERESIS}f'
        decomposed = 'refs/heads/pru\N{COMBINING DIAERESIS}f'
        different_subtree = f.git('mktree', data=f'100644 blob {f.other}\tfile\n'.encode())
        different_tree = f.git('mktree', data=f'040000 tree {different_subtree}\tdeep\n'.encode())
        different = f.git('commit-tree', different_tree, data=b'different snapshot\n')
        (f.repo / 'packed-refs').write_bytes(
            f'{f.commit} {composed}\n{different} {decomposed}\n'.encode(),
        )
        source = f.repo.as_uri()
        oracle = f.root / 'oracle.git'
        f.git('init', '--bare', str(oracle), at=f.root)
        ssh = f.ssh(protocol_v2=True)
        # Leave room for OpenSSH's control-socket suffix on Darwin.
        scratch = tempfile.TemporaryDirectory(prefix='go-ssh-', dir='/tmp')
        self.addCleanup(scratch.cleanup)
        base = dict(f.env, TMPDIR=scratch.name)
        for setting in ('default', 'true'):
            with self.subTest(precompose=setting):
                f.env = base.copy()
                if setting == 'true':
                    f.env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='core.precomposeUnicode',
                                 GIT_CONFIG_VALUE_0=setting)
                native = subprocess.run(
                    ['git', '-C', oracle, 'fetch', '--no-tags', '--depth=1', '--', source, decomposed],
                    cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
                )
                self.assertEqual(native.returncode, 0, native.stderr)
                selected = f.git('-C', oracle, 'rev-parse', 'FETCH_HEAD')
                if setting == 'true':
                    self.assertEqual(selected, f.commit)
                expected = f.git('-C', oracle, 'show', 'FETCH_HEAD:deep/file') + '\n'
                process = f.probe(ssh, decomposed)
                self.assertEqual(f.line(process)['tree'], f.git('-C', oracle, 'rev-parse', 'FETCH_HEAD^{tree}'))
                value, _ = f.operation(process, 'read deep/file')
                self.assertEqual(value['data'], expected)
                f.finish(process)
                result = subprocess.run(
                    [OPTIONS.binary, source, decomposed, '--backend=copy', '--write-policy=all',
                     '--', 'cat', 'deep/file'],
                    cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
                )
                self.assertEqual((result.returncode, result.stdout), (0, expected.encode()), result.stderr)
                self.assertEqual(list(Path(scratch.name).iterdir()), [])

    def test_disabled_packet_trace_keeps_reusable_file_transport(self):
        f = self.f
        for disabled in ('', '0', 'FaLsE'):
            with self.subTest(value=disabled):
                f.env['GIT_TRACE_PACKET'] = disabled
                process = f.probe(f.repo.as_uri(), 'main')
                f.line(process)
                value, report = f.operation(process, 'read deep/file')
                self.assertEqual(value['data'], 'expected\n')
                f.finish(process)
                self.assertGreater(report['metrics']['remote_session_fetch_operations'], 0)
                self.assertEqual(report['metrics']['remote_fetch_operations'], 0)

    def test_fetch_accepts_relative_absolute_and_non_utf8_repository_paths(self):
        f=self.f
        weird=f.root/os.fsdecode(b'server-\xff.git')
        sources = [str(f.repo), '../server.git']
        try:
            weird.symlink_to(f.repo, target_is_directory=True)
        except OSError as error:
            if sys.platform != 'darwin' or error.errno != errno.EILSEQ:
                raise
        else:
            sources.append(str(weird))
        for source in sources:
            with self.subTest(source=source):
                process=f.probe(source,'main')
                self.assertEqual(f.line(process)['tree'],f.tree)
                value,report=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n')
                self.assertEqual(report['metrics']['remote_fetch_operations'],0)
                self.assertEqual(report['metrics']['remote_native_pack_bytes'],0)
                self.assertFalse(any(path.is_file() for path in
                                     f.root.glob('*/remote-repository/objects/**/*')))
                f.finish(process)

    def test_file_selection_preserves_native_fetch_for_ref_in_want(self):
        f = self.f
        f.git('config', 'uploadpack.allowRefInWant', 'true')
        source = f.repo.as_uri()
        with tempfile.TemporaryDirectory(dir=f.root) as oracle:
            f.git('init', '--bare', '--quiet', oracle, at=f.root)
            native = subprocess.run(
                ['git', '-C', oracle, 'fetch', '--quiet', '--no-tags', '--depth=1',
                 '--filter=tree:0', '--', source, 'main'],
                env=f.env, capture_output=True, timeout=20,
            )
        process = f.probe(source, 'main')
        stdout, stderr = process.communicate(timeout=20)
        if native.returncode:
            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertIn(native.stderr.strip(), stderr)
        else:
            self.assertEqual(process.returncode, 0, stderr)
            report = json.loads(stdout)
            self.assertEqual(report['tree'], f.tree)
            self.assertEqual(report['metrics']['remote_fetch_operations'], 1)
        process = f.probe(source, f.commit)
        report = f.line(process)
        self.assertEqual(report['tree'], f.tree)
        self.assertEqual(report['metrics']['remote_fetch_operations'], 0)
        f.finish(process)

    @unittest.skipUnless(sys.platform == 'darwin', 'native Git precomposes remote names on macOS')
    def test_decomposed_file_remote_uses_configured_fetch_mapping(self):
        f = self.f
        composed = 'caf\N{LATIN SMALL LETTER E WITH ACUTE}'
        decomposed = 'cafe\N{COMBINING ACUTE ACCENT}'
        f.git('init', '--quiet', at=f.cwd)
        f.git('config', 'core.precomposeUnicode', 'true', at=f.cwd)
        f.git('config', f'remote.{composed}.url', f.repo.as_uri(), at=f.cwd)
        included = f.root / 'fetch-mappings'
        f.git('config', 'include.path', str(included), at=f.cwd)
        f.env['GIT_CONFIG'] = str(f.cwd / '.git' / 'config')
        for configured, expected_fetches in ((composed, 1), (decomposed, 0)):
            with self.subTest(configured=configured):
                included.write_text(
                    f'[remote "{configured}"]\n'
                    '    fetch = +refs/heads/*:refs/remotes/selected/*\n',
                )
                process = f.probe(decomposed, 'main')
                report = f.line(process)
                try:
                    self.assertEqual(report['tree'], f.tree)
                    self.assertEqual(report['metrics']['remote_fetch_operations'], expected_fetches)
                    if expected_fetches:
                        private = next(f.root.glob('*/remote-repository'))
                        self.assertEqual(
                            f.git('rev-parse', 'refs/remotes/selected/main', at=private), f.commit,
                        )
                finally:
                    f.finish(process)
    def test_force_prefixed_refspec_follows_native_ssh_selection(self):
        f = self.f
        other_tree = f.git('mktree', data=f'100644 blob {f.other}\tfile\n'.encode())
        other = f.git('commit-tree', other_tree, data=b'force refspec fixture\n')
        f.git('update-ref', 'refs/heads/+main', other)
        source = f.ssh(protocol_v2=True)
        for revision, commit, tree in (
            ('+main', f.commit, f.tree),
            ('refs/heads/+main', other, other_tree),
        ):
            with self.subTest(revision=revision):
                self.assertEqual(self.fetched_oid(source, revision), commit)
                process = f.probe(source, revision)
                self.assertEqual(f.line(process)['tree'], tree)
                f.finish(process)
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
    def test_native_packet_diagnostics_leave_external_trace_targets_untouched(self):
        f = self.f
        f.git('config', 'uploadpack.allowFilter', 'false')
        trace = f.root / 'persistent-trace'
        previous = b'earlier trace: fetch=shallow filter\n'
        trace.write_bytes(previous)
        fifo = f.root / 'external-pipe'
        os.mkfifo(fifo, 0o600)
        command = [OPTIONS.binary, f.repo.as_uri(), f.commit, '--write-policy=none',
                   '--', 'printf', 'child-ran']
        for target in (str(trace), str(fifo), '/dev/stdout', '2'):
            with self.subTest(target=target):
                result = subprocess.run(
                    command, cwd=f.cwd,
                    env=dict(f.env, GIT_TRACE_PACKET=target, GIT_TRACE=str(trace),
                             GIT_TRACE2_EVENT=str(trace)),
                    capture_output=True, timeout=15,
                )
                self.assertEqual(result.returncode, 125, result.stderr)
                self.assertNotIn(b'child-ran', result.stdout)
                self.assertIn(b'does not advertise object filtering', result.stderr)
                self.assertEqual(trace.read_bytes(), previous)
                if target in ('/dev/stdout', '2'):
                    packets = result.stdout if target == '/dev/stdout' else result.stderr
                    self.assertIn(b'version 2', packets)
                    self.assertNotIn(b'want ', packets)
                else:
                    self.assertEqual(result.stdout, b'')

    def test_native_packet_trace_to_a_full_pipe_can_be_cancelled(self):
        f = self.f
        f.git('config', 'uploadpack.allowFilter', 'false')
        reader, writer = os.pipe()
        try:
            # Some Linux cat implementations grow the pipe, and splice can
            # queue more bytes than the nominal capacity. Leave ample trace.
            trace_size = 128 * 1024
            if sys.platform == 'linux':
                max_pipe_size = 1024 * 1024
                try:
                    configured = int(Path('/proc/sys/fs/pipe-max-size').read_text())
                    max_pipe_size = max(max_pipe_size, configured)
                except OSError:
                    pass
                trace_size = 4 * max_pipe_size
            native_git = shutil.which('git', path=f.env['PATH'])
            tools = f.root / 'trace-replay-tools'
            tools.mkdir()
            wrapper = tools / 'git'
            wrapper.write_text(f"""#!{sys.executable}
import os, sys
if 'ls-remote' in sys.argv and '--symref' in sys.argv:
    with open(os.environ['GIT_TRACE_PACKET'], 'ab') as output:
        output.truncate({trace_size})
os.execv({native_git!r}, [{native_git!r}, *sys.argv[1:]])
""")
            wrapper.chmod(0o700)
            env = dict(f.env, GIT_TRACE_PACKET='/dev/stdout',
                       PATH=str(tools) + os.pathsep + f.env['PATH'])
            command = [OPTIONS.binary, f.repo.as_uri(), f.commit, '--write-policy=none',
                       '--', 'printf', 'child-ran']
            runner = subprocess.Popen(command, cwd=f.cwd, env=env, stdout=writer,
                                      stderr=subprocess.PIPE)
            with runner:
                try:
                    deadline = time.monotonic() + 10
                    while True:
                        if runner.poll() is not None:
                            _, stderr = runner.communicate(timeout=2)
                            self.fail(f'runner exited {runner.returncode} before trace blocked: {stderr!r}')
                        readable, writable, _ = select.select([reader], [writer], [], 0)
                        if readable and not writable:
                            break
                        self.assertLess(time.monotonic(), deadline, 'trace replay did not block')
                        time.sleep(.01)
                    runner.send_signal(signal.SIGTERM)
                    _, stderr = runner.communicate(timeout=4)
                    self.assertEqual(runner.returncode, -signal.SIGTERM, stderr)
                    assert_no_run_state(self, f.root)
                finally:
                    if runner.poll() is None:
                        runner.kill()
        finally:
            os.close(reader)
            os.close(writer)

    def test_native_filter_advertisement_discards_listing_and_preserves_detached_helper(self):
        f=self.f; native=shutil.which('git'); tools=f.root/'trace-tools'; tools.mkdir()
        source='file://'+str(f.repo)
        sink=f.root/'listing-sink'; ready=f.root/'trace-ready'; result=f.root/'trace-result'
        detached='''import os,subprocess,sys,time
from pathlib import Path
git,source,ready,result,descriptor=sys.argv[1:]; ready,result=Path(ready),Path(result)
deadline=time.monotonic()+10
while not ready.exists() and time.monotonic()<deadline: time.sleep(.01)
if not ready.exists():
    result.write_bytes(b'timed out waiting for native ref advertisement')
    raise SystemExit(1)
os.write(2,b'late diagnostic\\n')
try:
    os.write(int(descriptor),b'late packet trace\\n')
except BrokenPipeError:
    result.write_bytes(b'already-open trace lost its reader')
    raise SystemExit(1)
trace=Path(os.environ['GIT_TRACE_PACKET'])
discarded=trace.exists() and trace.samefile(os.devnull)
run=subprocess.run([git,'ls-remote',source,'HEAD'],capture_output=True,close_fds=False,timeout=5)
pending=result.with_suffix('.new')
pending.write_bytes(f'{discarded} {run.returncode}\\n'.encode()+run.stdout+run.stderr)
pending.rename(result)
'''
        wrapper=tools/'git'
        wrapper.write_text(f'''#!{sys.executable}
import os,subprocess,sys
from pathlib import Path
args=sys.argv[1:]
if 'ls-remote' in args and '--symref' in args:
    trace=Path(os.environ['GIT_TRACE_PACKET'])
    with open(os.devnull,'wb') as null:
        stdout_discarded=os.path.sameopenfile(1,null.fileno())
        Path({str(sink)!r}).write_text(str(stdout_discarded))
    with trace.open('ab') as writer:
        writer.write(b'irrelevant packet trace\\n' * 4096)
        writer.flush()
        subprocess.Popen(
            [sys.executable,'-c',{detached!r},{native!r},{source!r},{str(ready)!r},
             {str(result)!r},str(writer.fileno())],
            stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,pass_fds=(writer.fileno(),),
            start_new_session=True)
os.execv({native!r},[{native!r},*args])
''')
        wrapper.chmod(0o700)
        f.env.update(PATH=str(tools)+os.pathsep+f.env['PATH'],GIT_TRACE_BARE='1')
        process=f.probe(source, 'HEAD:')
        self.assertEqual(f.line(process)['tree'],f.tree)
        ready.touch()
        deadline=time.monotonic()+10
        while not result.exists() and time.monotonic()<deadline: time.sleep(.01)
        self.assertTrue(result.exists(),'detached helper did not finish after ref advertisement')
        detached_status=result.read_bytes()
        f.finish(process)
        self.assertEqual(sink.read_text(),'True')
        self.assertEqual(detached_status,b'True 0\n'+f.commit.encode()+b'\tHEAD\n')
    def test_native_fetch_does_not_run_the_source_reference_hook(self):
        f = self.f
        f.cwd = f.repo
        hooks = f.root / 'hooks'
        hooks.mkdir()
        audit = f.root / 'caller-audit'
        audit.write_bytes(b'original')
        hook = hooks / 'reference-transaction'
        hook.write_text('#!/bin/sh\nprintf called >> ' + shlex.quote(str(audit)) + '\n')
        hook.chmod(0o700)
        f.git('config', 'core.hooksPath', str(hooks))
        alternate_audit = f.root / 'alternate-audit'
        alternate = f.root / 'alternate-refs'
        alternate.write_text('#!/bin/sh\nprintf called > ' + shlex.quote(str(alternate_audit)) + '\n')
        alternate.chmod(0o700)
        f.git('config', 'core.alternateRefsCommand', str(alternate))
        refs = f.git('for-each-ref', '--format=%(refname) %(objectname)')
        config = (f.repo / 'config').read_bytes()
        def objects():
            root = f.repo / 'objects'
            return {path.relative_to(root): path.read_bytes() for path in root.rglob('*') if path.is_file()}
        before = objects()

        process = f.probe(f.repo.as_uri(), 'main:refs/heads/private-copy')
        self.assertEqual(f.line(process)['tree'], f.tree)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        f.finish(process)

        self.assertEqual(audit.read_bytes(), b'original')
        self.assertFalse(alternate_audit.exists())
        self.assertEqual((f.repo / 'config').read_bytes(), config)
        self.assertEqual(f.git('for-each-ref', '--format=%(refname) %(objectname)'), refs)
        self.assertEqual(objects(), before)

    def test_native_fetch_uses_private_storage_and_releases_it_after_runner_death(self):
        f = self.f
        if sys.platform == 'darwin':
            f.env['XDG_RUNTIME_DIR'] = str(f.root)
        process = f.probe(f.repo.as_uri(), 'main:refs/heads/private-copy')
        self.assertEqual(f.line(process)['tree'], f.tree)
        (link,) = f.root.glob('*/remote-repository')
        repository = link.resolve(strict=True)
        workspace = (link.parent / '.git-on-memory').resolve(strict=True)
        staging = (repository / 'pack-staging').resolve(strict=True)
        self.assertTrue(repository.is_relative_to(link.parent.resolve()))
        self.assertTrue(staging.is_relative_to(workspace))
        self.assertEqual(repository.stat().st_dev, link.parent.stat().st_dev)
        self.assertEqual(workspace.stat().st_mode & 0o777, 0o700)
        if sys.platform == 'darwin':
            self.assertEqual(workspace.parent, f.root.resolve())
            self.assertEqual(workspace.stat().st_dev, f.root.stat().st_dev)
        else:
            self.assertEqual(run(['stat', '-f', '-c', '%T', staging]).strip(), b'tmpfs')

        # An unlinked diagnostic can outlive the runner in a detached Git
        # helper. Keep the descriptor open across runner exit and use it.
        with tempfile.TemporaryFile(dir=workspace) as diagnostic:
            process.kill()
            process.communicate(timeout=20)
            diagnostic.write(b'late diagnostic')
            diagnostic.seek(0)
            self.assertEqual(diagnostic.read(), b'late diagnostic')
        deadline = time.monotonic() + 10
        while workspace.exists() and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertFalse(workspace.exists(), 'native repository outlived the runner')

    @contextlib.contextmanager
    def native_copy_workspace(self):
        f = self.f
        child = 'import sys; from pathlib import Path; print(Path.cwd().parent, flush=True); sys.stdin.read()'
        process = subprocess.Popen(
            [OPTIONS.binary, f.repo.as_uri(), 'main:refs/heads/selected',
             '--backend=copy', '--write-policy=all', '--', sys.executable, '-c', child],
            cwd=f.cwd, env=f.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            self.assertTrue(select.select([process.stdout], [], [], 20)[0], 'command did not start')
            line = process.stdout.readline()
            self.assertTrue(line, 'command did not report its run directory')
            directory = Path(os.fsdecode(line.rstrip(b'\n')))
            workspace = (directory / '.git-on-memory').resolve(strict=True)
            self.addCleanup(shutil.rmtree, workspace, True)
            yield process, directory, workspace
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=20)

    @contextlib.contextmanager
    def replace_service_directory(self, workspace):
        saved = workspace.with_name(workspace.name + '-saved')
        workspace.rename(saved)
        try:
            workspace.mkdir(mode=0o700)
            sentinel = workspace / 'untouched'
            sentinel.write_bytes(b'keep this directory')
            yield sentinel
        finally:
            shutil.rmtree(workspace, ignore_errors=True)
            saved.rename(workspace)

    def test_checked_cleanup_removes_service_files_after_both_processes_die(self):
        f = self.f
        with self.native_copy_workspace() as (process, directory, workspace):
            processes = run(['ps', '-e', '-o', 'pid=', '-o', 'ppid=', '-o', 'args='])
            owner_args = [str(process.pid).encode(), b'--internal-service-storage']
            owners = [int(fields[0]) for row in processes.splitlines()
                      if (fields := row.split(maxsplit=2))[1:] == owner_args]
            self.assertEqual(len(owners), 1)
            os.kill(owners[0], signal.SIGKILL)
            process.kill()
        self.assertTrue(workspace.is_dir())

        cleanup = [OPTIONS.binary, '--cleanup', directory]
        with self.replace_service_directory(workspace) as sentinel:
            rejected = subprocess.run(cleanup, cwd=f.cwd, env=f.env, capture_output=True, timeout=15)
            self.assertEqual(rejected.returncode, 125, rejected.stderr)
            self.assertIn(b'service storage changed', rejected.stderr)
            self.assertEqual(sentinel.read_bytes(), b'keep this directory')
            self.assertTrue(directory.exists(), 'failed cleanup discarded its recovery record')
        run(cleanup, cwd=f.cwd, env=f.env, timeout=15)
        self.assertFalse(workspace.exists())
        self.assertFalse(directory.exists())

    def test_automatic_cleanup_preserves_a_replacement_service_directory(self):
        f = self.f
        with self.native_copy_workspace() as (process, directory, workspace):
            with self.replace_service_directory(workspace) as sentinel:
                _, errors = process.communicate(timeout=20)
                self.assertTrue(sentinel.is_file(), 'automatic cleanup removed the replacement')
                self.assertEqual(sentinel.read_bytes(), b'keep this directory')
                self.assertEqual(process.returncode, 125, errors)
                self.assertTrue(directory.exists())
        run([OPTIONS.binary, '--cleanup', directory], cwd=f.cwd, env=f.env, timeout=15)
        self.assertFalse(workspace.exists())

    def test_native_fetch_honors_empty_tmpdir(self):
        f = self.f
        f.env.update(TMPDIR='', XDG_RUNTIME_DIR=str(f.root))
        output = run([OPTIONS.binary, f.repo.as_uri(), 'main:refs/heads/selected',
                      '--backend=copy', '--write-policy=all', '--', 'cat', 'deep/file'],
                     cwd=f.cwd, env=f.env, timeout=20)
        self.assertEqual(output, b'expected\n')
        assert_no_run_state(self, f.cwd)

    def test_native_fetch_retries_only_when_pack_writes_exhaust_tmpfs(self):
        if sys.platform != 'linux' or not Path('/dev/shm').is_dir():
            self.skipTest('requires the automatic Linux tmpfs')
        f = self.f
        tmpfs = Path('/dev/shm').resolve()
        if f.root.stat().st_dev == tmpfs.stat().st_dev:
            self.skipTest('requires a separate system temporary filesystem')
        attempts = f.root / 'fetch-attempts'
        tools = f.root / 'git-tools'
        tools.mkdir()
        native = shutil.which('git', path=f.env['PATH'])
        git = tools / 'git'
        git.write_text(f"""#!{sys.executable}
import os, subprocess, sys
from pathlib import Path

args = sys.argv[1:]
if 'fetch' in args:
    root = Path(os.environ['TMPDIR']).resolve()
    repository = Path(args[args.index('--git-dir') + 1]).resolve()
    objects = Path(os.environ['GIT_OBJECT_DIRECTORY']).resolve()
    assert repository.is_relative_to({str(f.root.resolve())!r})
    assert objects.is_relative_to(root) and (objects / 'pack').is_dir()
    with open({str(attempts)!r}, 'a') as log:
        print(root, file=log)
    if os.environ['FIXTURE_STORAGE_PHASE'] == 'reference':
        subprocess.run([{native!r}, *args], check=True)
        sys.exit("error: couldn't write destination reference: No space left on device")
    if '--no-write-fetch-head' in args and root.is_relative_to({str(tmpfs)!r}):
        if os.environ['FIXTURE_STORAGE_PHASE'] == 'index':
            (objects / 'pack' / 'tmp_pack_fixture').write_bytes(b'x' * 4096)
            sys.exit("fatal: sha1 file 'tmp_idx_fixture' write error. Out of diskspace")
        sys.exit('fatal: write error: No space left on device')
os.execv({native!r}, [{native!r}, *args])
""")
        git.chmod(0o700)
        f.env.update(PATH=str(tools) + os.pathsep + f.env['PATH'],
                     GIT_CONFIG_COUNT='2', GIT_CONFIG_KEY_0='fetch.fsckObjects',
                     GIT_CONFIG_VALUE_0='false', GIT_CONFIG_KEY_1='fetch.unpackLimit',
                     GIT_CONFIG_VALUE_1='1', XDG_RUNTIME_DIR=str(f.root))
        source = f.repo.as_uri()
        refspec = 'main:refs/heads/selected'
        for phase in ('pack', 'index', 'reference'):
            with self.subTest(phase=phase):
                f.env['FIXTURE_STORAGE_PHASE'] = phase
                f.env['GIT_TRACE'] = '1' if phase == 'reference' else '0'
                attempts.write_text('')
                process = f.probe(source, refspec)
                if phase == 'reference':
                    _, errors = process.communicate(timeout=20)
                    self.assertNotEqual(process.returncode, 0)
                    self.assertIn(b"couldn't write", errors)
                    self.assertIn(b'index-pack', errors)
                    self.assertEqual(len(attempts.read_text().splitlines()), 1)
                else:
                    self.assertEqual(f.line(process)['tree'], f.tree)
                    value, report = f.operation(process, 'read deep/file')
                    self.assertEqual(value['data'], 'expected\n')
                    f.finish(process)
                    paths = list(map(Path, attempts.read_text().splitlines()))
                    self.assertEqual(report['metrics']['remote_fetch_operations'], len(paths))
                    if phase == 'index':
                        self.assertGreaterEqual(report['metrics']['remote_native_pack_bytes'], 4096)
                    self.assertTrue(paths[0].is_relative_to(tmpfs))
                    self.assertTrue(paths[1].is_relative_to(tmpfs))
                    self.assertTrue(paths[2].is_relative_to(f.root.resolve()))
                    self.assertTrue(all(path == paths[2] for path in paths[2:]))
                assert_no_run_state(self, f.root)

    def test_native_fetch_keeps_case_distinct_destination_names(self):
        f = self.f
        source = f.root / 'reftable.git'
        f.git('init', '--bare', '--quiet', '--ref-format=reftable', str(source), at=f.root)
        (source / 'objects/info/alternates').write_text(str(f.repo / 'objects') + '\n')
        refs = ('refs/heads/release', 'refs/heads/RELEASE/next')
        for ref in refs:
            f.git('update-ref', ref, f.commit, at=source)
        f.env['XDG_RUNTIME_DIR'] = str(f.root)

        inspect = '''
import subprocess, sys
from pathlib import Path

destination = Path.cwd().parent / 'remote-repository'
subprocess.run(['git', '-C', str(destination), 'rev-parse', *sys.argv[1:]], check=True)
print(Path('deep/file').read_text(), end='')
'''
        output = run([OPTIONS.binary, source.as_uri(), 'refs/heads/*:refs/heads/*',
                      '--backend=copy', '--write-policy=all', '--',
                      sys.executable, '-c', inspect, *refs], cwd=f.cwd, env=f.env, timeout=20)
        self.assertEqual(output.decode().splitlines(), [f.commit, f.commit, 'expected'])

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
        f = self.f
        f.env, log = record_git_invocations(f.root, f.env)
        config=f.root/'rewrite-config'
        config.write_text(f'[url "file://{f.root}/"]\ninsteadOf = fixture:\n')
        shallow = f.root / 'source-shallow'
        boundary = f.commit.encode() + b'\n'
        shallow.write_bytes(boundary)
        f.env.update(GIT_CONFIG_GLOBAL=str(config), GIT_SHALLOW_FILE=str(shallow))
        source='fixture:server.git'; revision='+heads/main:refs/heads/private-copy'
        process=f.probe(source,revision)
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n'); f.finish(process)
        calls = git_invocations(log)
        self.assertTrue(any('fetch' in call and call[-3:]==['--',source,revision]
                            for call in calls),calls)
        self.assertEqual(shallow.read_bytes(), boundary)
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
                assert_no_run_state(self, f.root)
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
                    assert_no_run_state(self, f.root)
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
            assert_no_run_state(self, f.root)
    def test_local_common_directory_reads_without_changing_native_repository_refs(self):
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
        for repository in before: self.assertEqual(refs(repository),before[repository])
        value,_=f.operation(process,'read file')
        self.assertEqual(value['data'],'common repository\n'); f.finish(process)
        for repository in before: self.assertEqual(refs(repository),before[repository])
        if OPTIONS.require_fuse or OPTIONS.require_nfs:
            ready=f.root/'common-ready'
            code=(f'import os,time; from pathlib import Path; '
                  f'assert Path("file").read_text() == "common repository\\n"; '
                  f'ready=Path({str(ready)!r}); pending=ready.with_suffix(".new"); '
                  f'pending.write_text(str(os.getpid())); pending.replace(ready); time.sleep(30)')
            process=subprocess.Popen([str(OPTIONS.binary),commit,'--',sys.executable,'-c',code],
                                     cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            f.children.append(process)
            deadline=time.monotonic()+10
            while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
            self.assertTrue(ready.exists(),'common-directory snapshot did not execute its command')
            child=int(ready.read_text()); runs=run_directories(f.root)
            self.assertEqual(len(runs),1)
            live_refs={repository:refs(repository) for repository in before}
            process.kill(); process.wait(timeout=5)
            with contextlib.suppress(ProcessLookupError): os.kill(child,signal.SIGTERM)
            process.communicate(timeout=5)
            stale_refs={repository:refs(repository) for repository in before}
            cleanup=subprocess.run([str(OPTIONS.binary),'--cleanup',str(runs[0])],
                                   cwd=f.cwd,env=unscoped,capture_output=True,timeout=15)
            self.assertEqual(cleanup.returncode,0,cleanup.stderr)
            self.assertFalse(runs[0].exists())
            self.assertEqual(live_refs,before)
            self.assertEqual(stale_refs,before)
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
            assert_no_run_state(self, f.root)

    def test_local_absolute_object_directory_with_an_alternate_needs_no_private_odb(self):
        f = self.f
        f.cwd = f.repo
        name = b'primary:objects-' + (b'utf8' if sys.platform == 'darwin' else b'\xff')
        primary = f.root.resolve() / os.fsdecode(name)
        (primary / 'info').mkdir(parents=True)
        (primary / 'info' / 'alternates').write_text('../server.git/objects\n')
        f.env['GIT_OBJECT_DIRECTORY'] = str(primary)
        expected = f.git('show', f.commit + ':deep/file')
        self.assertEqual(expected, 'expected')
        process = f.probe('local', f.commit)
        self.assertEqual(f.line(process)['tree'], f.tree)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], expected + '\n')
        bridges = list(f.root.glob('*/local-object-bridge-*'))
        f.finish(process)
        self.assertEqual(bridges, [])

    def test_local_absolute_object_directory_accepts_native_relative_alternates(self):
        f = self.f
        f.cwd = f.repo
        link = f.git('hash-object', '-w', '--stdin', data=b'deep/file')
        entries = f.git('ls-tree', f.tree).encode() + f'\n120000 blob {link}\tlink\n'.encode()
        tree = f.git('mktree', data=entries)
        commit = f.git('commit-tree', tree, data=b'relative alternates fixture\n')
        primary = f.root.resolve() / 'primary-objects'
        (primary / 'info').mkdir(parents=True)
        shutil.copytree(f.repo / 'objects', primary / 'sub')
        f.env['GIT_OBJECT_DIRECTORY'] = str(primary)
        f.env['GIT_ON_SERVICE_TMPDIR'] = str(f.root / 'missing-service-dir')
        entry = primary / 'info/alternates'
        for line in ('sub', '"sub"'):
            with self.subTest(alternate=line):
                entry.write_text(line + '\n')
                expected = f.git('show', commit + ':deep/file')
                self.assertEqual(expected, 'expected')
                process = f.probe('local', commit)
                self.assertEqual(f.line(process)['tree'], tree)
                value, _ = f.operation(process, 'read deep/file')
                self.assertEqual(value['data'], expected + '\n')
                self.assertEqual(list(f.root.glob('*/local-object-bridge-*')), [])
                f.finish(process)
                copy = subprocess.run(
                    [OPTIONS.binary, commit, '--backend=copy', '--write-policy=all', '--',
                     'cat', 'link'],
                    cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
                )
                self.assertEqual((copy.returncode, copy.stdout), (0, b'expected\n'), copy.stderr)
                assert_no_run_state(self, f.root)

        # An unrelated object directory with the same relative spelling in
        # the caller's cwd is outside the source Git selected.
        shutil.copytree(primary / 'sub', f.cwd / 'elsewhere')
        entry.write_text('elsewhere\n')
        native = subprocess.run(
            ['git', 'cat-file', '-e', commit], cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
        )
        self.assertNotEqual(native.returncode, 0, native.stderr)
        failed = subprocess.run(
            [OPTIONS.binary, commit, '--backend=copy', '--write-policy=all', '--', 'cat', 'link'],
            cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
        )
        self.assertEqual((failed.returncode, failed.stdout), (125, b''), failed.stderr)
        assert_no_run_state(self, f.root)

    def test_native_local_alternates_do_not_fetch_missing_promisor_objects(self):
        f = self.f
        f.cwd = f.repo
        f.git('config', 'remote.provider.url', f.repo.as_uri())
        f.git('config', 'remote.provider.promisor', 'true')
        programs = f.root / 'git-without-no-lazy-fetch'
        programs.mkdir()
        git = shutil.which('git')
        launcher = programs / 'git'
        intercepted = programs / 'intercepted'
        launcher.write_text(
            '#!/bin/sh\n'
            'for argument do case "$argument" in cat-file|rev-parse) '
            f'unset GIT_NO_LAZY_FETCH; printf "%s\\n" "$argument" >> {shlex.quote(str(intercepted))}; '
            'break ;; esac; done\n'
            f'exec {shlex.quote(git)} "$@"\n'
        )
        launcher.chmod(0o700)
        for missing, revision in ((f.blob, f.commit), (f.subtree, f.commit + ':deep/file')):
            with self.subTest(revision=revision):
                intercepted.unlink(missing_ok=True)
                primary = f.root / ('objects-' + missing)
                (primary / 'info').mkdir(parents=True)
                (primary / 'info/alternates').write_text('sub\n')
                storage = primary / 'sub'
                shutil.copytree(f.repo / 'objects', storage)
                (storage / missing[:2] / missing[2:]).unlink()
                env = dict(f.env, PATH=str(programs) + os.pathsep + f.env['PATH'],
                           GIT_OBJECT_DIRECTORY=str(primary), GIT_ALLOW_PROTOCOL='file')
                before = sorted(path.relative_to(primary) for path in primary.rglob('*'))
                result = subprocess.run(
                    [OPTIONS.binary, revision, '--backend=copy', '--write-policy=all', '--',
                     'cat', 'deep/file'],
                    cwd=f.cwd, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual((result.returncode, result.stdout), (125, b''), result.stderr)
                expected_reader = 'cat-file' if missing == f.blob else 'rev-parse'
                self.assertIn(expected_reader, intercepted.read_text().splitlines())
                self.assertEqual(sorted(path.relative_to(primary) for path in primary.rglob('*')), before)
                assert_no_run_state(self, f.root)

    def test_cr_inside_an_alternates_comment_does_not_add_an_object_store(self):
        f = self.f
        f.git('init', '--quiet', at=f.cwd)
        alternates = f.cwd / '.git' / 'objects' / 'info' / 'alternates'
        contents = b'# ignored\r' + os.fsencode(f.repo / 'objects') + b'\n'
        alternates.write_bytes(contents)
        native = subprocess.run(['git', 'cat-file', '-t', f.tree], cwd=f.cwd, env=f.env,
                                capture_output=True, timeout=10)
        self.assertNotEqual(native.returncode, 0)
        result = subprocess.run(
            [OPTIONS.binary, f.tree, '--backend=copy', '--write-policy=all', '--', 'cat', 'deep/file'],
            cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
        )
        self.assertEqual((result.returncode, result.stdout), (125, b''), result.stderr)
        self.assertEqual(alternates.read_bytes(), contents)
        assert_no_run_state(self, f.root)

    def test_local_alternates_keep_the_primary_depth_alongside_env(self):
        f = self.f
        f.cwd = f.repo
        stores = [f.root / f'objects-{index}' for index in range(6)]
        for store, following in zip(stores, [*stores[1:], f.repo / 'objects']):
            (store / 'info').mkdir(parents=True)
            (store / 'info/alternates').write_text(os.path.relpath(following, store) + '\n')
        elsewhere = f.root / 'environment-objects'
        elsewhere.mkdir()
        f.env.update(GIT_OBJECT_DIRECTORY=str(stores[0]),
                     GIT_ALTERNATE_OBJECT_DIRECTORIES=str(elsewhere))
        self.assertEqual(f.git('show', f.commit + ':deep/file'), 'expected')
        process = f.probe('local', f.commit)
        self.assertEqual(f.line(process)['tree'], f.tree)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        self.assertEqual(list(f.root.glob('*/local-object-bridge-*')), [])
        f.finish(process)

    def test_native_local_alternates_keep_the_resolved_environment_store(self):
        f = self.f
        f.cwd = f.repo
        primary = f.root / 'primary-objects'
        (primary / 'info').mkdir(parents=True)
        original = f.root / 'objects-with-":quote'
        shutil.copytree(f.repo / 'objects', original)
        empty = f.root / 'empty-objects'
        empty.mkdir()
        (primary / 'info/alternates').write_text('"../empty-objects"\n')
        alternate = f.root / 'environment-objects'
        f.env['GIT_OBJECT_DIRECTORY'] = str(primary)
        for value in (str(alternate), '"missing:' + str(alternate), f'"{alternate}\\000suffix"'):
            with self.subTest(value=value):
                alternate.unlink(missing_ok=True)
                alternate.symlink_to(original, target_is_directory=True)
                f.env['GIT_ALTERNATE_OBJECT_DIRECTORIES'] = value
                self.assertEqual(f.git('show', f.commit + ':deep/file'), 'expected')
                process = f.probe('local', f.commit)
                self.assertEqual(f.line(process)['tree'], f.tree)
                alternate.unlink()
                alternate.symlink_to(empty, target_is_directory=True)
                value, _ = f.operation(process, 'read deep/file')
                self.assertEqual(value['data'], 'expected\n')
                f.finish(process)
                alternate.unlink()

        physical = f.root / 'physical'
        (physical / 'child').mkdir(parents=True)
        (physical / 'objects').symlink_to(original, target_is_directory=True)
        alternate.symlink_to(physical / 'child', target_is_directory=True)
        with self.subTest(value='symlink/../objects with NUL suffix'):
            store = alternate / '..' / 'objects'
            f.env['GIT_ALTERNATE_OBJECT_DIRECTORIES'] = f'"{store}\\000suffix"'
            self.assertEqual(f.git('show', f.commit + ':deep/file'), 'expected')
            process = f.probe('local', f.commit)
            self.assertEqual(f.line(process)['tree'], f.tree)
            value, _ = f.operation(process, 'read deep/file')
            self.assertEqual(value['data'], 'expected\n')
            f.finish(process)

    def test_native_local_alternates_read_from_a_long_resolved_environment(self):
        f = self.f
        f.cwd = f.repo
        primary = f.root / 'primary-objects'
        (primary / 'info').mkdir(parents=True)
        (f.root / 'empty-objects').mkdir()
        (primary / 'info/alternates').write_text('../empty-objects\n')
        long_path = f.root
        for index in range(8):
            long_path /= f'objects-{index}-' + 'x' * 68
        long_path.mkdir(parents=True)
        entries, stores = [], []
        for index in range(220):
            store = long_path / f'objects-{index}'
            store.mkdir()
            alias = f.root / f'alias-{index}'
            alias.symlink_to(store, target_is_directory=True)
            entries.append(str(alias))
            stores.append(store)

        def chain(*paths):
            for here, following in zip(paths, paths[1:]):
                (here / 'info').mkdir(parents=True)
                (here / 'info/alternates').write_text(os.path.relpath(following, here) + '\n')

        # The first root reaches the shared directory at the nesting limit.
        # The later root skips that already visited store, so the last direct
        # entry is still traversed at depth zero in Git's single graph.
        shared = long_path / 'shared'
        chain(stores[0], *(long_path / f'early-{i}' for i in range(4)), shared)
        chain(stores[-2], *(long_path / f'late-{i}' for i in range(3)), shared)
        chain(shared, stores[-1], f.repo / 'objects')
        f.env.update(GIT_OBJECT_DIRECTORY=str(primary),
                     GIT_ALTERNATE_OBJECT_DIRECTORIES=':'.join(entries))
        self.assertEqual(f.git('show', f.commit + ':deep/file'), 'expected')
        f.env, log = record_git_invocations(f.root, f.env)
        process = f.probe('local', f.commit)
        self.assertEqual(f.line(process)['tree'], f.tree)
        alias = f.root / 'alias-219'
        alias.unlink()
        alias.symlink_to(f.root / 'empty-objects', target_is_directory=True)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        f.finish(process)
        starts = git_invocations(log)
        readers = [argv for argv in starts if 'cat-file' in argv
                   and any(mode in argv for mode in ('--batch-command', '--batch', '--batch-check'))]
        self.assertIn(len(readers), (1, 2), readers)

    def test_native_local_alternates_report_an_early_git_error(self):
        f = self.f
        f.cwd = f.repo
        primary = f.root / 'primary-objects'
        (primary / 'info').mkdir(parents=True)
        (primary / 'info/alternates').write_text('"../server.git/objects"\n')
        programs = f.root / 'programs'
        programs.mkdir()
        git = shutil.which('git')
        launcher = programs / 'git'
        launcher.write_text(
            '#!/bin/sh\n'
            'for argument do if [ "$argument" = cat-file ]; then echo "fixture git failure" >&2; exit 17; fi; done\n'
            f'exec {shlex.quote(git)} "$@"\n'
        )
        launcher.chmod(0o700)
        env = dict(f.env, PATH=str(programs) + os.pathsep + f.env['PATH'],
                   GIT_OBJECT_DIRECTORY=str(primary))
        result = subprocess.run(
            [OPTIONS.binary, f.commit, '--backend=copy', '--write-policy=all', '--', 'true'],
            cwd=f.cwd, env=env, capture_output=True, timeout=20,
        )
        self.assertEqual((result.returncode, result.stdout), (125, b''), result.stderr)
        self.assertIn(b'fixture git failure', result.stderr)

    def test_copy_rejects_a_native_decoder_failure_at_eof(self):
        f = self.f
        f.cwd = f.repo
        data = random.Random(9).randbytes(80 * 1024)
        blob = f.git('hash-object', '-w', '--stdin', data=data)
        tree = f.git('mktree', data=f'100644 blob {blob}\tpayload\n'.encode())
        commit = f.git('commit-tree', tree, data=b'batch exit fixture\n')
        f.git('update-ref', 'refs/heads/main', commit)
        f.git('config', 'fetch.prune', 'false')
        primary = f.root / 'primary-objects'
        (primary / 'info').mkdir(parents=True)
        (primary / 'info/alternates').write_text('"../server.git/objects"\n')
        tools = f.root / 'programs'
        tools.mkdir()
        native = shlex.quote(shutil.which('git'))
        launcher = tools / 'git'
        launcher.write_text(
            '#!/bin/sh\nfor argument do\n'
            '    case "$argument" in --batch-command|--batch)\n'
            f'        {native} "$@" || exit $?\n'
            '        exit 13 ;;\n'
            '    esac\ndone\n'
            f'exec {native} "$@"\n'
        )
        launcher.chmod(0o700)
        env = dict(f.env, PATH=str(tools) + os.pathsep + f.env['PATH'])
        for operands, environment in (
            ([commit], dict(env, GIT_OBJECT_DIRECTORY=str(primary))),
            ([f.repo.as_uri(), commit], env),
        ):
            with self.subTest(operands=operands):
                result = subprocess.run(
                    [OPTIONS.binary, *operands, '--backend=copy', '--write-policy=all',
                     '--', 'printf', 'child ran'],
                    cwd=f.cwd, env=environment, capture_output=True, timeout=20,
                )
                self.assertEqual((result.returncode, result.stdout), (125, b''), result.stderr)
                self.assertIn(b'exit status: 13', result.stderr)
                assert_no_run_state(self, f.root)

        if OPTIONS.require_fuse or OPTIONS.require_nfs:
            backend = 'fuse' if OPTIONS.require_fuse else 'nfs'
            result = subprocess.run(
                [OPTIONS.binary, commit, f'--backend={backend}', '--', 'cat', 'payload'],
                cwd=f.cwd, env=dict(env, GIT_OBJECT_DIRECTORY=str(primary)),
                capture_output=True, timeout=20,
            )
            self.assertEqual(result.stdout, data)
            self.assertEqual(result.returncode, 125, result.stderr)
            self.assertIn(b'exit status: 13', result.stderr)
            assert_no_run_state(self, f.root)

    def test_local_absolute_object_directory_keeps_its_resolved_store(self):
        f = self.f
        f.cwd = f.repo
        empty = f.root / 'empty-objects'
        empty.mkdir()
        primary = f.root / 'selected-objects'
        primary.symlink_to(f.repo / 'objects')
        f.env['GIT_OBJECT_DIRECTORY'] = str(primary)
        self.assertEqual(f.git('show', f.commit + ':deep/file'), 'expected')
        process = f.probe('local', f.commit)
        self.assertEqual(f.line(process)['tree'], f.tree)
        replacement = f.root / 'replacement'
        replacement.symlink_to(empty)
        replacement.replace(primary)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        f.finish(process)

    @unittest.skipUnless(sys.platform == 'linux', 'byte paths require Linux in this fixture')
    def test_local_indirect_primary_byte_path_keeps_full_alternate_depth(self):
        f = self.f
        f.cwd = f.repo
        stores = [f.root / os.fsdecode(b'byte-objects-\xff')]
        stores.extend(f.root / f'objects-{index}' for index in range(1, 6))
        for store, following in zip(stores, [*stores[1:], f.repo / 'objects']):
            (store / 'info').mkdir(parents=True)
            relative = os.fsencode(os.path.relpath(following, store))
            (store / 'info' / 'alternates').write_bytes(relative + b'\n')
        primary = f.root / 'primary'
        primary.symlink_to(stores[0])
        f.env['GIT_OBJECT_DIRECTORY'] = str(primary)
        self.assertEqual(f.git('show', f.commit + ':deep/file'), 'expected')
        process = f.probe('local', f.commit)
        self.assertEqual(f.line(process)['tree'], f.tree)
        empty = f.root / 'empty-objects'
        empty.mkdir()
        alternate = f.root / 'replacement'
        alternate.symlink_to(empty)
        alternate.replace(primary)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        bridges = list(f.root.glob('*/local-object-bridge-*'))
        descriptor = os.open(stores[0], os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            descriptor_path_works = os.path.samefile(stores[0], f'/proc/self/fd/{descriptor}')
        except OSError:
            descriptor_path_works = False
        finally:
            os.close(descriptor)
        if descriptor_path_works:
            self.assertEqual(bridges, [])
        else:
            self.assertEqual(len(bridges), 1)
            self.assertEqual(sorted(p.name for p in bridges[0].iterdir()),
                             ['HEAD', 'objects', 'refs'])
            self.assertTrue((bridges[0] / 'objects').is_symlink())
        f.finish(process)
        self.assertTrue(all(not bridge.exists() for bridge in bridges))

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
                assert_no_run_state(self, f.root)
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
                    bridges=list(f.root.glob('*/local-object-bridge-*'))
                    value,_=f.operation(process,'read file')
                    self.assertEqual(value['data'].encode(),payload); f.finish(process)
                    self.assertEqual(bridges,[])
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
    def test_local_nested_environment_alternates_match_native_git(self):
        f=self.f; stores=[f.root/f'objects-{index}' for index in range(7)]
        for store in stores: (store/'info').mkdir(parents=True)
        for store,following in zip(stores,stores[1:]):
            (store/'info'/'alternates').write_text(str(following)+'\n')
        f.env['GIT_OBJECT_DIRECTORY']=str(stores[-1])
        blob=f.git('hash-object','-w','--stdin',data=b'nested alternate\n')
        del f.env['GIT_OBJECT_DIRECTORY']
        tree=f.git('mktree','--missing',data=f'100644 blob {blob}\tleaf\n'.encode())
        for alternate,accepted in ((stores[0],False),(stores[1],True)):
            with self.subTest(alternate=alternate.name):
                env=dict(f.env,GIT_ALTERNATE_OBJECT_DIRECTORIES=str(alternate))
                native=subprocess.run(['git','cat-file','-p',tree+':leaf'],cwd=f.repo,env=env,
                                      capture_output=True,timeout=10)
                self.assertEqual(native.returncode==0,accepted,native.stderr)
                probe=subprocess.run([OPTIONS.probe,'local',tree,'--metrics-only'],cwd=f.repo,
                                     env=env,input=b'read leaf\nexit\n',capture_output=True,timeout=10)
                self.assertEqual(probe.returncode==0,accepted,probe.stderr)
                if accepted:
                    self.assertEqual(json.loads(probe.stdout.splitlines()[1])['data'].encode(),native.stdout)
                else:
                    self.assertEqual(len(probe.stdout.splitlines()),1)
                    self.assertIn(b'read blob header',probe.stderr)
    def test_https_fresh_runs_lazy_objects_ref_race_and_local_equivalence(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        wanted={f.commit,f.tree,f.subtree,f.blob}
        for invocation,revision in enumerate(('HEAD','HEAD',f.commit)):
            with self.subTest(invocation=invocation,revision=revision):
                requests=len(f.requests)
                process=f.probe(url,revision); start=f.line(process)
                self.assertEqual(set(start['objects']),{f.commit,f.tree})
                if invocation==0:
                    self.assertFalse((f.cwd/'.git').exists())
                    value,listed=f.operation(process,'list ')
                    self.assertEqual(set(value['names']),{'.gitignore','deep','other'})
                    self.assertEqual(set(listed['objects']),{f.commit,f.tree})
                if invocation==1:
                    # Move HEAD after resolution. This invocation stays pinned;
                    # the next explicitly requests the original commit.
                    moved_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
                    f.git('update-ref','refs/heads/main',f.git('commit-tree',moved_tree,data=b'moved\n'))
                value,read=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n')
                self.assertEqual(set(read['objects']),wanted)
                f.finish(process)
                self.assertEqual({oid for request in f.requests[requests:] for oid in request['wants']},wanted)
        self.assertFalse((f.root/'cache').exists(),'remote run created a global cache')
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

    def test_remote_verifier_keeps_the_callers_git_configuration(self):
        f = self.f
        _, allowed = self.signing()
        f.env.pop('GIT_CONFIG_COUNT')
        f.env['GIT_CONFIG_GLOBAL'] = str(f.root / 'gitconfig')
        f.git('config', '--global', 'fixture.verify', 'enabled')
        policy = f.root / 'policy'
        policy.mkdir()
        f.git('init', '--quiet', at=policy)
        f.git('config', 'fixture.verify', 'permit', at=policy)
        verifier = f.root / 'caller-verify'
        journal = f.root / 'verifier-used'
        verifier.write_text(
            '#!/bin/sh\n'
            'test "$(git config --global fixture.verify)" = enabled || exit 1\n'
            'if test -n "$FIXTURE_WORKTREE"; then\n'
            '    test "$(git rev-parse --show-toplevel)" = "$FIXTURE_WORKTREE" || exit 1\n'
            '    test "$(git -C "$FIXTURE_POLICY" config --local fixture.verify)" = permit || exit 1\n'
            'fi\n'
            'if test "$FIXTURE_NO_WORKTREE" = 1; then\n'
            '    test "$(git rev-parse --is-inside-work-tree)" = false || exit 1\n'
            '    test "$(git rev-parse --is-bare-repository)" = false || exit 1\n'
            'fi\n'
            f'printf invoked > {shlex.quote(str(journal))}\n'
            f'exec {shlex.quote(shutil.which("ssh-keygen"))} "$@"\n'
        )
        verifier.chmod(0o700)
        for object_format in ('sha1', 'sha256'):
            with self.subTest(object_format=object_format):
                caller = f.root / object_format
                caller.mkdir()
                f.git('init', '--quiet', '--object-format=' + object_format, at=caller)
                f.git('config', 'fixture.verify', 'deny', at=caller)
                f.git('config', 'gpg.format', 'ssh', at=caller)
                allowed_path = allowed
                if object_format == 'sha256':
                    allowed_path = Path('allowed-signers')
                    shutil.copyfile(allowed, caller / allowed_path)
                f.git('config', 'gpg.ssh.allowedSignersFile', str(allowed_path), at=caller)
                f.git('config', 'gpg.ssh.program', str(verifier), at=caller)
                cwd = caller / 'nested'
                cwd.mkdir()
                environment = dict(f.env)
                if object_format == 'sha1':
                    f.git('init', '--quiet', at=cwd)
                    shutil.rmtree(cwd / '.git/objects')
                    self.assertEqual(f.git('rev-parse', '--absolute-git-dir', at=cwd),
                                     str(caller.resolve() / '.git'))
                    environment['FIXTURE_WORKTREE'] = str(caller.resolve())
                    environment['FIXTURE_POLICY'] = str(policy.resolve())
                contexts = [(cwd, environment, None)]
                if object_format == 'sha1':
                    contexts.append((caller / '.git', dict(f.env, FIXTURE_NO_WORKTREE='1'), None))
                    worktree = f.root / 'separate-worktree'
                    worktree.mkdir()
                    contexts.append((caller, f.env, worktree))
                for directory, env, worktree in contexts:
                    with self.subTest(directory=directory.name):
                        if worktree is not None:
                            f.git('config', 'core.worktree', str(worktree), at=caller)
                            self.assertEqual(f.git('rev-parse', '--show-toplevel', at=caller),
                                             str(worktree.resolve()))
                        journal.unlink(missing_ok=True)
                        output = run([OPTIONS.binary, f.repo.as_uri(), 'refs/tags/signed', '--verify',
                                      '--backend=copy', '--write-policy=all', '--', 'cat', 'deep/file'],
                                     cwd=directory, env=env, timeout=20)
                        self.assertEqual(output, b'expected\n')
                        self.assertEqual(journal.read_text(), 'invoked')

    def test_native_openpgp_verification_uses_git_configuration(self):
        f = self.f
        signing_home = f.root / 'gnupg'
        empty_home = f.root / 'unknown-gnupg'
        for home in (signing_home, empty_home):
            home.mkdir(mode=0o700)
            self.addCleanup(run, ['gpgconf', '--kill', 'all'],
                            env=dict(f.env, GNUPGHOME=str(home)))
        f.env['GNUPGHOME'] = str(signing_home)
        uid = 'OpenPGP fixture <fixture@example.invalid>'
        run(['gpg', '--batch', '--pinentry-mode', 'loopback', '--passphrase', '',
             '--quick-generate-key', uid, 'ed25519', 'sign', '0'], env=f.env)
        signed = f.git('-c', 'user.signingkey=fixture@example.invalid', 'commit-tree',
                       '-S', f.tree, data=b'OpenPGP fixture\n')

        journal = f.root / 'verifier-journal'
        trace = f.root / 'git-trace'
        verifier = f.root / 'caller-gpg'
        gpg = shlex.quote(shutil.which('gpg', path=f.env['PATH']))
        verifier.write_text(
            '#!/bin/sh\n'
            f'printf "%s\\n%s\\n" "$GNUPGHOME" "$TMPDIR" >> {shlex.quote(str(journal))}\n'
            f'exec {gpg} "$@"\n'
        )
        verifier.chmod(0o700)
        f.git('config', 'gpg.program', str(verifier))

        for home, status, stdout in ((signing_home, 0, b'expected\n'), (empty_home, 125, b'')):
            with self.subTest(gnupg_home=home.name):
                env = dict(f.env, GNUPGHOME=str(home), GIT_TRACE=str(trace),
                           GIT_ON_SERVICE_TMPDIR=str(f.root / 'no-service-storage'))
                journal.unlink(missing_ok=True)
                trace.unlink(missing_ok=True)
                result = subprocess.run(
                    [OPTIONS.binary, '--verify', signed, '--backend=copy', '--write-policy=all',
                     '--', 'cat', 'deep/file'],
                    cwd=f.repo, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual(journal.read_text().splitlines(), [str(home), str(f.root)])
                self.assertIn(f'verify-commit {signed}', trace.read_text())
                self.assertEqual((result.returncode, result.stdout), (status, stdout), result.stderr)
                if status:
                    self.assertIn(b'execution forbidden', result.stderr)
                assert_no_run_state(self, f.root)

    @unittest.skipUnless(shutil.which('gpgsm'), 'requires the native X.509 verifier')
    def test_x509_verification_uses_the_callers_certificate_policy(self):
        f = self.f
        home = f.root / 'x509'
        home.mkdir(mode=0o700)
        f.env['GNUPGHOME'] = str(home)
        self.addCleanup(run, ['gpgconf', '--kill', 'all'], env=f.env)
        certificate, key = f.root / 'fixture.crt', f.root / 'fixture.key'
        policy = '1.2.3.4'
        run(['openssl', 'req', '-x509', '-nodes', '-newkey', 'rsa:2048', '-days', '1',
             '-subj', '/CN=Policy fixture', '-addext', 'basicConstraints=critical,CA:TRUE',
             '-addext', 'keyUsage=critical,digitalSignature,keyCertSign',
             '-addext', 'certificatePolicies=critical,' + policy,
             '-keyout', key, '-out', certificate], env=f.env)
        fingerprint = run(['openssl', 'x509', '-in', certificate, '-noout', '-fingerprint', '-sha1'],
                          env=f.env).strip().split(b'=', 1)[1]
        (home / 'trustlist.txt').write_bytes(fingerprint + b' S\n')
        policies = f.repo / 'fixture policy.txt'
        policies.write_text(policy + '\n')
        (home / 'gpgsm.conf').write_text(f'disable-crl-checks\npolicy-file "{policies.name}"\n')
        run(['gpgsm', '--batch', '--import', certificate], cwd=f.repo, env=f.env)
        # Some gpgsm builds require /dev/stdin for Git's detached input.
        wrapper = f.root / 'gpgsm-stdin'
        verifier = shutil.which('gpgsm')
        wrapper.write_text(f'''#!{sys.executable}
import os, sys
os.execv({verifier!r}, [{verifier!r}, *('/dev/stdin' if arg == '-' else arg for arg in sys.argv[1:])])
''')
        wrapper.chmod(0o700)
        f.git('config', 'gpg.x509.program', str(wrapper))
        unsigned = run(['git', '-C', f.repo, 'cat-file', 'commit', f.commit], env=f.env)
        cms = run(['openssl', 'cms', '-sign', '-binary', '-outform', 'DER', '-md', 'sha256',
                   '-signer', certificate, '-inkey', key], input=unsigned, env=f.env)
        signature = b'-----BEGIN SIGNED MESSAGE-----\n' + base64.encodebytes(cms) + b'-----END SIGNED MESSAGE-----'
        headers, message = unsigned.split(b'\n\n', 1)
        signed = headers + b'\ngpgsig ' + signature.replace(b'\n', b'\n ') + b'\n\n' + message
        commit = f.git('hash-object', '-w', '-t', 'commit', '--stdin', data=signed)
        native = ['git', '-C', str(f.repo), 'verify-commit', commit]
        run(native, env=f.env)
        agent_config = home / 'gpg-agent.conf'
        command = [OPTIONS.binary, commit, '--verify', '--backend=copy', '--write-policy=all',
                   '--', 'cat', 'deep/file']
        result = subprocess.run(command, cwd=f.repo, env=f.env, capture_output=True, timeout=20)
        self.assertEqual((result.returncode, result.stdout), (0, b'expected\n'), result.stderr)

        agent_config.write_text('no-user-trustlist\n')
        run(['gpgconf', '--kill', 'gpg-agent'], env=f.env)
        untrusted = subprocess.run(native, env=f.env, capture_output=True, timeout=20)
        self.assertNotEqual(untrusted.returncode, 0)
        executed = f.root / 'untrusted-command-executed'
        rejected = subprocess.run(
            [OPTIONS.binary, commit, '--verify', '--backend=copy', '--write-policy=all', '--',
             sys.executable, '-c', 'import pathlib, sys; pathlib.Path(sys.argv[1]).touch()', executed],
            cwd=f.repo, env=f.env, capture_output=True, timeout=20,
        )
        self.assertEqual(rejected.returncode, 125, rejected.stderr)
        self.assertIn(b'execution forbidden', rejected.stderr)
        self.assertFalse(executed.exists())
        agent_config.unlink()
        run(['gpgconf', '--kill', 'gpg-agent'], env=f.env)
        self.assertEqual(policies.read_text(), policy + '\n')
        policies.write_text('1.2.3.5\n')
        denied = subprocess.run(native, env=f.env, capture_output=True, timeout=20)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn(b'No policy match', denied.stderr)
        rejected = subprocess.run(
            [OPTIONS.binary, commit, '--verify', '--backend=copy', '--write-policy=all', '--', 'cat', 'deep/file'],
            cwd=f.repo, env=f.env, capture_output=True, timeout=20,
        )
        self.assertEqual((rejected.returncode, rejected.stdout), (125, b''), rejected.stderr)
        self.assertIn(b'No policy match', rejected.stderr)
        self.assertEqual(policies.read_text(), '1.2.3.5\n')
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
        assert_no_run_state(self, f.root)
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
                    assert_no_run_state(self, f.root)
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
        directory,=run_directories(f.root)
        process.kill(); self.assertEqual(process.wait(timeout=5),-signal.SIGKILL)
        self.assertFalse(select.select([descriptor],[],[],.1)[0],'startup native operation unexpectedly stopped with runner death')
        self.assertFalse((directory/'.state').exists())
        self.assertFalse((directory/'ssh-control-owner').exists())
        self.assertFalse(any(line.split()[4]==str(directory/'mount') for line in Path('/proc/self/mountinfo').read_text().splitlines()))
        assert_run_unlocked(directory)
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
        result=subprocess.run(['git','-c','http.sslCAInfo='+f.env['GIT_SSL_CAINFO'],'on',url,'main','--','sh','-c','cat deep/file; mkdir out; printf kept > out/result'],cwd=f.cwd,env=git_on_environment(OPTIONS.binary,f.env,f.root/'selected-tools'),capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr); self.assertEqual(result.stdout,b'expected\n')
        assert_no_run_state(self, f.root)
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--','sleep','30'],cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+10
        while time.monotonic()<deadline and not any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()): time.sleep(.02)
        self.assertTrue(any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()),'remote run never mounted')
        process.send_signal(signal.SIGTERM); process.communicate(timeout=10)
        self.assertEqual(process.returncode,-signal.SIGTERM)
        assert_no_run_state(self, f.root)

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
            self.assertEqual(report['metrics']['spool_writes'],0)
            self.assertEqual(report['metrics']['hot_blob_cache_entries'],1)
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
        f = self.f
        signed, _ = self.signing()
        tag = f.git('rev-parse', 'refs/tags/signed')
        different_dir = f.git('mktree', data=f'100644 blob {f.other}\tfile\n'.encode())
        different_tree = f.git('mktree', data=f'040000 tree {different_dir}\tdeep\n'.encode())
        different = f.git('commit-tree', different_tree, data=b'distinct tree\n')
        f.git('tag', '-a', '-m', 'replacement tag', 'replacement', different)
        replacement = f.git('rev-parse', 'refs/tags/replacement')
        f.git('update-ref', 'refs/heads/main', signed)

        native = shutil.which('git', path=f.env['PATH'])
        tools = f.root / 'git-tools'
        tools.mkdir()
        git = tools / 'git'
        log = f.root / 'verified-object'
        git.write_text(f"""#!{sys.executable}
import json, os, subprocess, sys
from pathlib import Path

args = sys.argv[1:]
for operation in ('verify-commit', 'verify-tag'):
    if operation in args:
        result = subprocess.run([{native!r}, *args])
        if result.returncode == 0:
            with open({str(log)!r}, 'a') as output:
                output.write(json.dumps(args[args.index(operation):]) + '\\n')
            if obsolete := os.environ.get('FIXTURE_REMOVE_OBJECT'):
                Path(obsolete).unlink()
            else:
                subprocess.run([{native!r}, 'update-ref', os.environ['FIXTURE_MOVE_REF'],
                                os.environ['FIXTURE_MOVE_TO']], check=True)
        sys.exit(result.returncode)
os.execv({native!r}, [{native!r}, *args])
""")
        git.chmod(0o700)
        for reference, operation, oid, destination in (
            ('refs/heads/main', 'verify-commit', signed, different),
            ('refs/tags/signed', 'verify-tag', tag, replacement),
            ('refs/tags/replacement', 'verify-tag', tag, None),
        ):
            with self.subTest(operation=operation, reference=reference, destination=destination):
                log.unlink(missing_ok=True)
                env = dict(f.env, PATH=str(tools) + os.pathsep + f.env['PATH'])
                if destination is None:
                    f.git('update-ref', reference, tag)
                    env['FIXTURE_REMOVE_OBJECT'] = str(f.repo / 'objects' / signed[:2] / signed[2:])
                else:
                    env.update(FIXTURE_MOVE_REF=reference, FIXTURE_MOVE_TO=destination)
                result = subprocess.run(
                    [OPTIONS.binary, reference, '--verify', '--backend=copy', '--write-policy=all',
                     '--', 'cat', 'deep/file'],
                    cwd=f.repo, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual(git_invocations(log), [[operation, oid]])
                if destination is None:
                    self.assertEqual((result.returncode, result.stdout), (125, b''), result.stderr)
                else:
                    self.assertEqual((result.returncode, result.stdout), (0, b'expected\n'), result.stderr)
                    self.assertEqual(f.git('rev-parse', reference), destination)
                assert_no_run_state(self, f.root)

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
            # The -c case must override a permissive repository setting for
            # both local and remote native verification.
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
                        assert_no_run_state(self, f.root)
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
                    assert_no_run_state(self, f.root)
                    print('native default trust execution evidence:',json.dumps(dict(operation=operation,source=source,native_status=native.returncode,runner_status=result.returncode,command_bytes=sentinel.stat().st_size)),flush=True)

    def test_real_mount_native_git_c_verification_and_caught_transport_failure(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; signed,allowed=self.signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        env=git_on_environment(OPTIONS.binary,f.env,f.root/'selected-tools')
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
        assert_no_run_state(self, f.root)

    def test_real_mount_caught_verified_raw_storage_failure_is_sticky(self):
        if not (OPTIONS.require_fuse or OPTIONS.require_nfs):self.skipTest('requires a native mount')
        f=self.f;blob=f.git('hash-object','-w','--stdin',data=b'v'*(128*1024))
        rows=[f'100644 blob {blob}\tfile\n']
        for index in range(9):
            filler=f.git('hash-object','-w','--stdin',data=bytes([index])*(64*1024+1))
            rows.append(f'100644 blob {filler}\tfiller-{index}\n')
        tree=f.git('mktree',data=''.join(rows).encode())
        commit=f.git('commit-tree',tree,data=b'raw integrity failure\n');f.git('update-ref','refs/heads/main',commit)
        code=f"""import errno,os,time
from pathlib import Path
os.stat('file')
for index in range(9):
 with open(f'filler-{{index}}','rb') as filler:assert filler.read(1)==bytes([index])
raw=next(Path.cwd().parent.glob('raw-objects-*'))/{blob!r}
deadline=time.monotonic()+5
while not(raw.is_file() and raw.stat().st_size==128*1024):
 assert time.monotonic()<deadline,'the raw source never finished spilling'
 time.sleep(.01)
with raw.open('r+b') as output:output.write(b'!')
try:
 with open('file','rb') as source:source.read(1)
except OSError as error:
 assert error.errno==errno.EIO,error
else:raise AssertionError('raw corruption was not rejected')
print('caught-EIO-and-returned-success')
"""
        result=subprocess.run([str(OPTIONS.binary),'file://'+str(f.repo),'HEAD','--write-policy=none','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
        self.assertIn(b'caught-EIO-and-returned-success',result.stdout)
        self.assertEqual(result.returncode,125,result.stderr)
        self.assertIn(b'bytes were modified',result.stderr);self.assertIn(b'execution incomplete',result.stderr)
        assert_no_run_state(self, f.root)

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

    def test_large_remote_objects_reuse_run_odb_and_fresh_runs_stream(self):
        # Read a working set that fits the whole-blob cache without spilling.
        f=self.f
        size=64*1024+1
        ids=[f.git('hash-object','-w','--stdin',data=bytes([byte])*size)
             for byte in b'abc']
        tree=f.git('mktree',data=''.join(f'100644 blob {oid}\t{name}\n' for name,oid in zip('abc',ids)).encode())
        commit=f.git('commit-tree',tree,data=b'large run-local reuse fixture\n');f.git('update-ref','refs/heads/main',commit)
        url=f.https();self.addCleanup(f.server.shutdown)
        process=f.probe(url);f.line(process)
        for name in 'abc':
            value,last=f.operation(process,f'range 13 16 {name}')
            self.assertEqual(bytes(value['bytes']),name.encode()*16)
        self.assertEqual(last['metrics']['remote_fetches'],5)
        self.assertEqual(last['metrics']['hot_blob_cache_entries'],3)
        self.assertEqual(last['metrics']['spool_bytes'],0)
        requests=len(f.requests)
        value,last=f.operation(process,'range 13 16 a');self.assertEqual(bytes(value['bytes']),b'a'*16)
        self.assertEqual(len(f.requests),requests,'available run object fetched again')
        self.assertEqual(last['metrics']['spool_writes'],0,'closed hot blob was written')
        f.finish(process)
        # A separate invocation streams a newly fetched loose object again.
        requests=len(f.requests);process=f.probe(url);f.line(process)
        value,last=f.operation(process,'range 13 16 c');self.assertEqual(bytes(value['bytes']),b'c'*16)
        self.assertEqual(last['metrics']['remote_raw_blob_imports'],1)
        self.assertEqual(last['metrics']['blob_loads'],1)
        self.assertEqual({oid for request in f.requests[requests:] for oid in request['wants']},{commit,tree,ids[2]})
        f.finish(process)
        self.assertFalse((f.root/'cache').exists())
        print('large run object evidence:',json.dumps(dict(logical_blob_bytes=len(ids)*size,metrics=last['metrics'])),flush=True)

    def test_materialized_remote_fetches_the_selected_tree_once(self):
        f=self.f;entries=[]
        for index in range(12):
            blob=f.git('hash-object','-w','--stdin',data=f'file {index}\n'.encode())
            tree=f.git('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
            entries.append(f'040000 tree {tree}\td{index}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        ancestor=f.commit
        commit=f.git('commit-tree',tree,'-p',ancestor,data=b'materialization transfer regression\n')
        f.git('update-ref','refs/heads/main',commit)
        url=f.https();self.addCleanup(f.server.shutdown)
        socket_log = f.root / 'failed-nfs-endpoint'
        code=("import os,subprocess,sys\nfrom pathlib import Path\n"
              f"assert os.environ['GIT_ON_TREE_OID']=={tree!r}\n"
              "run = Path.cwd().parent\n"
              "assert not (run / '.state').exists() and not (run / '.lock').exists()\n"
              f"assert not (run/'remote-repository'/'objects'/{tree[:2]!r}/{tree[2:]!r}).exists()\n"
              "if sys.argv[1] == 'auto':\n"
              f"    socket = Path(os.fsdecode(Path({str(socket_log)!r}).read_bytes()))\n"
              "    assert not socket.parent.exists(), socket\n"
              "assert all(Path(f'd{i}/file').read_text()==f'file {i}\\n' for i in range(12)); "
              f"assert subprocess.run(['git','--git-dir',str(Path.cwd().parent/'remote-repository'),'cat-file','-e',{ancestor!r}],"
              "env=dict(os.environ,GIT_NO_LAZY_FETCH='1'),stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode!=0; "
              "print('verified')")
        backends = ('copy', 'auto') if OPTIONS.require_nfs else ('copy',)
        for backend in backends:
            with self.subTest(backend=backend):
                f.requests.clear()
                env = dict(f.env)
                if backend == 'auto':
                    env['GIT_ON_TEST_NFS_MOUNT_ERROR'] = str(socket_log)
                result = subprocess.run([str(OPTIONS.binary), url, 'HEAD', '--write-policy=all',
                                         '--backend='+backend, '--', sys.executable, '-c', code, backend],
                                        cwd=f.cwd, env=env, capture_output=True, timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,b'verified\n')
                wants = [request['wants'] for request in f.requests if request['wants']]
                expected = [[commit], [tree], [commit]] if backend == 'auto' else [[commit]]
                self.assertEqual(wants, expected,
                                 'eager preparation performs separate directory/object fetches')
                assert_no_run_state(self, f.root)

    def test_eager_ssh_releases_discovery_channel_before_native_fetch(self):
        f = self.f
        url = f.ssh(max_sessions=1)
        f.force_upload_pack(
            'trap "" HUP\n'
            f'git upload-pack {shlex.quote(str(f.repo))}\nstatus=$?\n'
            f'if mkdir {shlex.quote(str(f.root / "session-exited"))} 2>/dev/null; then sleep 0.6; fi\n'
            'exit "$status"\n'
        )
        with tempfile.TemporaryDirectory(dir='/tmp', prefix='go-copy-') as runtime:
            result = subprocess.run(
                [OPTIONS.binary, url, f.commit, '--backend=copy', '--write-policy=all',
                 '--', 'cat', 'deep/file'],
                cwd=f.cwd, env=dict(f.env, TMPDIR=runtime), capture_output=True, timeout=20,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')

    @unittest.skipUnless(OPTIONS.require_nfs, 'tests automatic fallback after NFS startup')
    def test_auto_copy_releases_the_pooled_channel_before_native_fetch(self):
        f = self.f
        source = f.ssh(protocol_v2=True, max_sessions=1)
        f.force_upload_pack('sleep 0.5\n' + f'exec git upload-pack {shlex.quote(str(f.repo))}\n')
        with tempfile.TemporaryDirectory(dir='/tmp', prefix='go-copy-') as runtime, \
                tempfile.TemporaryFile() as metrics:
            env = dict(f.env, GIT_ON_TEST_NFS_MOUNT_ERROR='1', TMPDIR=runtime,
                       GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='fetch.parallel', GIT_CONFIG_VALUE_0='1',
                       GIT_ON_TEST_METRICS_FD=str(metrics.fileno()))
            result = subprocess.run(
                [OPTIONS.binary, source, f.commit, '--write-policy=all', '--', 'cat', 'deep/file'],
                cwd=f.cwd, env=env, capture_output=True, timeout=20, pass_fds=(metrics.fileno(),),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, b'expected\n')
            metrics.seek(0)
            report = json.load(metrics)
            self.assertEqual(report['backend'], 'copy', report)
            self.assertGreater(report['remote_session_fetch_operations'], 0, report)
            self.assertEqual(report['remote_fetch_operations'], 1, report)
            assert_no_run_state(self, Path(runtime))

    def test_eager_ssh_closes_input_before_receiving_the_pack(self):
        f = self.f
        url = f.ssh(protocol_v2=True)
        upload_pack = f.root / 'wait-for-eof.py'
        upload_pack.write_text(
            'import os, select, subprocess, sys, time\n'
            'upload = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE)\n'
            'request = bytearray()\n'
            'deadline = time.monotonic() + 3\n'
            'while select.select([0], [], [], max(0, deadline - time.monotonic()))[0]:\n'
            '    chunk = os.read(0, 4096)\n'
            '    if not chunk:\n'
            '        upload.communicate(request)\n'
            '        sys.exit(upload.returncode)\n'
            '    request.extend(chunk)\n'
            'upload.communicate()\n'
            'sys.exit("expected SSH request EOF before sending the pack")\n'
        )
        f.force_upload_pack(shlex.join(
            [sys.executable, str(upload_pack), shutil.which('git'), 'upload-pack', str(f.repo)]
        ) + '\n')
        with tempfile.TemporaryDirectory(dir='/tmp', prefix='go-copy-') as runtime:
            result = subprocess.run(
                [OPTIONS.binary, url, f.commit, '--backend=copy', '--write-policy=all',
                 '--', 'cat', 'deep/file'],
                cwd=f.cwd, env=dict(f.env, TMPDIR=runtime), capture_output=True, timeout=20,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')

    def test_ssh_checks_exit_after_receiving_a_complete_pack(self):
        f = self.f
        url = f.ssh(protocol_v2=True)
        f.force_upload_pack(f'git upload-pack {shlex.quote(str(f.repo))}\nexit 1\n')
        modes = [('copy', 'all', b'')]
        if OPTIONS.require_fuse or OPTIONS.require_nfs:
            backend = 'nfs' if OPTIONS.require_nfs else 'fuse'
            modes.append((backend, 'none', b'expected\n'))
        for backend, policy, stdout in modes:
            with self.subTest(backend=backend), \
                    tempfile.TemporaryDirectory(dir='/tmp', prefix='go-ssh-') as runtime:
                result = subprocess.run(
                    [OPTIONS.binary, url, f.commit, '--backend=' + backend, '--write-policy=' + policy,
                     '--', 'cat', 'deep/file'],
                    cwd=f.cwd, env=dict(f.env, TMPDIR=runtime), capture_output=True, timeout=20,
                )
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertIn(b'SSH transport exited with exit status: 1', result.stderr)
                self.assertEqual(result.stdout, stdout)

    def assert_eager_ssh_discovery_releases_its_group(self, fail_before_advertising):
        f = self.f
        url = f.ssh()
        release = f.root / 'release-ssh-writer'
        once = f.root / 'started-ssh-writer'
        worker = f.root / 'ssh-writer.py'
        worker.write_text(
            'import fcntl, os, sys, time\n'
            'from pathlib import Path\n'
            'lock, ready, release = map(Path, sys.argv[1:4])\n'
            'if sys.argv[4] == "detach": os.setsid()\n'
            'with lock.open("wb") as file:\n'
            '    fcntl.flock(file, fcntl.LOCK_EX)\n'
            '    ready.touch()\n'
            '    while not release.exists(): time.sleep(.02)\n'
        )
        owned_lock = f.root / 'owned-writer.lock'
        detached_lock = f.root / 'detached-writer.lock'
        owned_ready = f.root / 'owned-writer.ready'
        detached_ready = f.root / 'detached-writer.ready'
        def spawn(lock, ready, mode):
            return shlex.join([sys.executable, str(worker), str(lock), str(ready), str(release), mode])
        ssh = f.root / 'fixture-ssh'
        reject_probe = 'if test "$SSH_ASKPASS_REQUIRE" = never; then exit 1; fi\n'
        ssh.write_text(
            '#!/bin/sh\n'
            f'if mkdir {shlex.quote(str(once))} 2>/dev/null; then\n'
            f' {spawn(owned_lock, owned_ready, "owned")} </dev/null &\n'
            f' {spawn(detached_lock, detached_ready, "detach")} </dev/null &\n'
            f' while test ! -e {shlex.quote(str(owned_ready))} || test ! -e {shlex.quote(str(detached_ready))}; do sleep .02; done\n'
            'fi\n'
            f'{reject_probe if fail_before_advertising else ""}'
            f'exec {f.env["GIT_SSH_COMMAND"]} "$@"\n'
        )
        ssh.chmod(0o700)
        with tempfile.TemporaryDirectory(dir='/tmp', prefix='go-copy-') as runtime:
            with subprocess.Popen(
                [OPTIONS.binary, url, f.commit, '--backend=copy', '--write-policy=all',
                 '--', 'cat', 'deep/file'],
                cwd=f.cwd, env=dict(f.env, GIT_SSH_COMMAND=str(ssh), TMPDIR=runtime),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            ) as process:
                try:
                    deadline = time.monotonic() + 4
                    while process.poll() is None and time.monotonic() < deadline:
                        time.sleep(.02)
                    finished = process.poll() is not None
                    if finished and owned_ready.exists() and detached_ready.exists():
                        with owned_lock.open('rb') as file:
                            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        with detached_lock.open('rb') as file:
                            with self.assertRaises(BlockingIOError):
                                fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                finally:
                    release.touch()
                try:
                    output, errors = process.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    raise
            self.assertEqual(process.returncode, 0, errors)
            assert_no_run_state(self, Path(runtime))
        self.assertTrue(once.is_dir())
        self.assertTrue(owned_ready.exists() and detached_ready.exists())
        self.assertTrue(finished, 'SSH discovery waited for the detached writer')
        self.assertEqual(output, b'expected\n')

    def test_eager_ssh_discovery_ignores_an_inherited_stdout_writer(self):
        self.assert_eager_ssh_discovery_releases_its_group(False)

    def test_eager_ssh_discovery_observes_exit_before_advertisement(self):
        self.assert_eager_ssh_discovery_releases_its_group(True)

    def test_eager_ssh_preserves_native_ref_selection_with_packet_diagnostics(self):
        f = self.f
        url = f.ssh(protocol_v2=True)
        f.git('update-ref', 'refs/heads/tag', f.commit)
        for revision in ('tag', f.commit):
            with self.subTest(revision=revision), \
                    tempfile.TemporaryDirectory(dir='/tmp', prefix='go-copy-') as runtime:
                result = subprocess.run(
                    [OPTIONS.binary, url, revision, '--backend=copy', '--write-policy=all',
                     '--', 'cat', 'deep/file'],
                    cwd=f.cwd, env=dict(f.env, TMPDIR=runtime, GIT_TRACE_PACKET='2'),
                    capture_output=True, timeout=20,
                )
                if revision == 'tag':
                    self.assertEqual(result.returncode, 125, result.stderr)
                    self.assertEqual(result.stdout, b'')
                    self.assertIn(b'tag', result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, b'expected\n')
                    self.assertIn(f'want {f.commit}'.encode(), result.stderr)

    @unittest.skipUnless(OPTIONS.require_fuse or OPTIONS.require_nfs, 'requires a native mount')
    def test_git_on_in_a_terminal_reuses_the_https_demand_session(self):
        f = self.f
        url = f.https()
        self.addCleanup(f.server.shutdown)
        programs = f.root / 'external-command'
        programs.mkdir()
        (programs / 'git-on').symlink_to(OPTIONS.binary.resolve())
        env = dict(f.env, PATH=str(programs) + os.pathsep + f.env['PATH'], TERM='dumb')
        backend = 'nfs' if OPTIONS.require_nfs else 'fuse'
        master, slave = pty.openpty()
        read_fd, write_fd = os.pipe()
        self.addCleanup(os.close, master)
        with os.fdopen(read_fd, 'rb') as metrics:
            try:
                def terminal():
                    os.setsid()
                    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
                process = subprocess.Popen(
                    ['git', 'on', url, 'HEAD', '--backend=' + backend, '--',
                     sys.executable, '-I', '-S', '-c',
                     "from pathlib import Path; print(Path('deep/file').read_text() + Path('other').read_text(), end='')"],
                    cwd=f.cwd, env=dict(env, GIT_ON_TEST_METRICS_FD=str(write_fd)),
                    stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    pass_fds=[write_fd], preexec_fn=terminal,
                )
                f.children.append(process)
            finally:
                os.close(slave)
                os.close(write_fd)
            output, errors = process.communicate(timeout=30)
            self.assertEqual(process.returncode, 0, errors)
            report = json.load(metrics)
        self.assertEqual(output, b'expected\nunrelated payload\n')
        self.assertEqual(report['backend'], backend)
        self.assertEqual(report['remote_fetches'], 5, report)
        self.assertEqual(report['remote_session_fetch_operations'], 3, report)
        # A following read can start while a helper validates the previous
        # pack trailer. At least one of the three fetches must reuse a helper.
        self.assertGreaterEqual(report['remote_session_starts'], 1, report)
        self.assertLess(report['remote_session_starts'], report['remote_session_fetch_operations'], report)
        self.assertEqual({oid for request in f.requests for oid in request['wants']},
                         {f.commit, f.tree, f.subtree, f.blob, f.other})
        assert_no_run_state(self, f.root)

    @unittest.skipUnless(OPTIONS.require_fuse or OPTIONS.require_nfs, 'requires a native mount')
    def test_cancel_concurrent_https_demands(self):
        f = self.f
        tree = f.git('mktree', data=(f'100644 blob {f.blob}\tfirst\n'
                                    f'100644 blob {f.other}\tsecond\n').encode())
        commit = f.git('commit-tree', tree, data=b'cancel concurrent fetches\n')
        f.git('update-ref', 'refs/heads/main', commit)
        url = f.https()
        self.addCleanup(f.server.shutdown)
        self.addCleanup(f.resume.set)
        f.stalled_oids.update((f.blob, f.other))
        backend = 'nfs' if OPTIONS.require_nfs else 'fuse'
        gate = f.root / 'start-second'
        code = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import os
import time
assert sorted(os.listdir('.')) == ['first', 'second']
with ThreadPoolExecutor(2) as pool:
    first = pool.submit(os.stat, 'first')
    while not Path({str(gate)!r}).exists():
        time.sleep(.01)
    second = pool.submit(os.stat, 'second')
    first.result()
    second.result()
"""
        process = subprocess.Popen(
            [str(OPTIONS.binary), url, commit, '--backend=' + backend, '--write-policy=none',
             '--', sys.executable, '-I', '-S', '-c', code],
            cwd=f.cwd, env=f.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        f.children.append(process)
        self.assertEqual(f.stalled_requests.get(timeout=12), [f.blob])
        gate.touch()
        self.assertEqual(f.stalled_requests.get(timeout=8), [f.other])
        process.send_signal(signal.SIGTERM)
        _, errors = process.communicate(timeout=12)
        self.assertEqual(process.returncode, -signal.SIGTERM, errors)
        self.assertNotIn(b'execution incomplete', errors)
        assert_no_run_state(self, f.root)

    @unittest.skipUnless(OPTIONS.require_fuse or OPTIONS.require_nfs, 'requires a native mount')
    def test_concurrent_https_lookups_combine_only_requested_objects(self):
        f = self.f
        sizes = list(range(21, 37))
        wanted = [f.git('hash-object', '-w', '--stdin', data=b'x' * size) for size in sizes]
        directories = [f.git('mktree', data=f'100644 blob {oid}\tfile\n'.encode())
                       for oid in wanted]
        entries = [f'040000 tree {oid}\td{index}'
                   for index, oid in enumerate(directories)]
        entries.extend((f'100644 blob {f.blob}\twarm', f'100644 blob {f.other}\tunused',
                        f'100644 blob {f.blob}\twarm-alias', f'040000 tree {directories[0]}\talias',
                        f'040000 tree {f.subtree}\tunvisited'))
        tree = f.git('mktree', data=('\n'.join(entries) + '\n').encode())
        commit = f.git('commit-tree', tree, data=b'concurrent lookups\n')
        f.git('update-ref', 'refs/heads/main', commit)
        url = f.https()
        self.addCleanup(f.server.shutdown)
        self.addCleanup(f.resume.set)
        f.stalled_oids.update(wanted)
        backend = 'nfs' if OPTIONS.require_nfs else 'fuse'
        command = f"""
import concurrent.futures
import os
from pathlib import Path
import time

assert 'unvisited' in os.listdir('.')
assert os.stat('warm').st_size == 9
sizes = {sizes!r}
parents = [Path(f'd{{index}}') for index in range(len(sizes))]
alias = Path('alias')
# Warm each parent with a lookup so this case measures foreground requests.
for parent in [*parents, alias]:
    assert not (parent / 'absent').exists()
control = Path({str(f.root)!r})

def wait_for(name):
    deadline = time.monotonic() + 20
    while not (control / name).exists():
        assert time.monotonic() < deadline, name
        time.sleep(.01)

def read(parent):
    path = parent / 'file'
    return path.stat().st_size, path.read_bytes()

with concurrent.futures.ThreadPoolExecutor(max_workers=len(sizes)) as pool:
    first = pool.submit(read, parents[0])
    wait_for('start-next')
    second = pool.submit(read, parents[1])
    duplicate = pool.submit(read, alias)
    wait_for('check-cached')
    assert os.stat('warm-alias').st_size == 9
    (control / 'cached-ready').touch()
    rest = list(pool.map(read, parents[2:]))
    assert [first.result(), second.result(), *rest] == [(size, b'x' * size) for size in sizes]
    assert duplicate.result() == first.result()
"""
        process = subprocess.Popen(
            [str(OPTIONS.binary), url, 'HEAD', '--backend=' + backend, '--write-policy=none', '--',
             sys.executable, '-I', '-S', '-c', command],
            cwd=f.cwd, env=f.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        f.children.append(process)
        def next_stalled(message, timeout=10):
            try:
                return f.stalled_requests.get(timeout=timeout)
            except queue.Empty:
                self.fail(message)
        self.assertEqual(next_stalled('first lookup did not reach HTTPS'), [wanted[0]])
        (f.root / 'start-next').touch()
        self.assertEqual(next_stalled('second lookup waited behind the first transfer'), [wanted[1]])
        (f.root / 'check-cached').touch()
        ready = f.root / 'cached-ready'
        deadline = time.monotonic() + 5
        while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertTrue(ready.exists(), 'cached blob waited behind the transfers')
        more_wants = next_stalled('concurrent sibling lookups did not reach HTTPS')
        if len(more_wants) == 1:
            next_stalled('a fourth exact demand did not reach HTTPS')
        # Give concurrent callbacks time to reach the mount while the initial
        # transfers are held. A free connection can batch those pending OIDs.
        time.sleep(.2)
        f.resume.set()
        _, errors = process.communicate(timeout=30)
        self.assertEqual(process.returncode, 0, errors)
        batches = [request['wants'] for request in f.requests if request['wants']]
        self.assertCountEqual([oid for batch in batches for oid in batch],
                              [commit, tree, f.blob, *directories, *wanted])
        self.assertTrue(any(len(set(batch).intersection(wanted)) > 1 for batch in batches), batches)
        assert_no_run_state(self, f.root)

    @unittest.skipUnless(OPTIONS.require_fuse or OPTIONS.require_nfs, 'requires a native mount')
    def test_cold_directory_demand_overlaps_file_lookup(self):
        f = self.f
        url = f.https()
        self.addCleanup(f.server.shutdown)
        self.addCleanup(f.resume.set)
        backend = 'nfs' if OPTIONS.require_nfs else 'fuse'
        for operation, check, children in (
            ("os.listdir, 'deep'", "['file']", []),
            ("lambda: os.stat('deep/file').st_size", '9', [f.blob]),
        ):
            with self.subTest(operation=operation):
                f.requests.clear()
                f.stalled.clear()
                f.resume.clear()
                f.stalled_oids = {f.subtree}
                gate = f.root / 'start-independent'
                done = f.root / 'independent-done'
                for path in (gate, done):
                    path.unlink(missing_ok=True)
                code = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import os
import time
assert 'deep' in os.listdir('.')
with ThreadPoolExecutor(1) as pool:
    first = pool.submit({operation})
    while not Path({str(gate)!r}).exists():
        time.sleep(.01)
    assert os.stat('other').st_size == 18
    Path({str(done)!r}).touch()
    assert first.result() == {check}
"""
                process = subprocess.Popen(
                    [str(OPTIONS.binary), url, 'HEAD', '--backend=' + backend,
                     '--write-policy=none', '--', sys.executable, '-I', '-S', '-c', code],
                    cwd=f.cwd, env=f.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                f.children.append(process)
                try:
                    reached_tree = f.stalled.wait(timeout=10)
                    gate.touch()
                    deadline = time.monotonic() + 5
                    while not done.exists() and process.poll() is None and time.monotonic() < deadline:
                        time.sleep(.01)
                    overlapped = done.exists()
                finally:
                    f.resume.set()
                _, errors = process.communicate(timeout=12)
                self.assertEqual(process.returncode, 0, errors)
                self.assertTrue(reached_tree, 'operation did not demand the cold tree')
                self.assertTrue(overlapped, 'independent lookup waited for the cold tree')
                self.assertCountEqual([oid for request in f.requests for oid in request['wants']],
                                      [f.commit, f.tree, f.subtree, f.other, *children])
                assert_no_run_state(self, f.root)

    @unittest.skipUnless(OPTIONS.require_fuse, 'requires the FUSE directory reference lifecycle')
    def test_fuse_lookup_survives_directory_close_during_https_wait(self):
        f = self.f
        tree = f.git('mktree', data=f'100644 blob {f.blob}\tfile\n'.encode())
        commit = f.git('commit-tree', tree, data=b'close directory during lookup\n')
        f.git('update-ref', 'refs/heads/main', commit)
        url = f.https()
        self.addCleanup(f.server.shutdown)
        self.addCleanup(f.resume.set)
        f.stalled_oids.add(f.blob)
        close = f.root / 'close-stream'
        closed = f.root / 'stream-closed'
        code = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import os
import time
with os.scandir('.') as stream, ThreadPoolExecutor(1) as pool:
    assert next(stream).name == 'file'
    result = pool.submit(os.stat, 'file')
    while not Path({str(close)!r}).exists():
        time.sleep(.01)
    stream.close()
    os.statvfs('.')
    Path({str(closed)!r}).touch()
    assert result.result().st_size == 9
"""
        process = subprocess.Popen(
            [str(OPTIONS.binary), url, commit, '--backend=fuse', '--write-policy=none',
             '--', sys.executable, '-I', '-S', '-c', code],
            cwd=f.cwd, env=f.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        f.children.append(process)
        self.assertTrue(f.stalled.wait(timeout=12), 'lookup did not reach HTTPS')
        close.touch()
        deadline = time.monotonic() + 5
        while not closed.exists() and process.poll() is None and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertTrue(closed.exists(), 'directory close did not reach FUSE')
        # The kernel may send RELEASEDIR asynchronously after close.
        time.sleep(.2)
        f.resume.set()
        _, errors = process.communicate(timeout=12)
        self.assertEqual(process.returncode, 0, errors)

    def test_startup_progress_covers_local_resolution_and_initial_remote_fetch(self):
        f=self.f
        revision=':/progress-\x1b'+'界'*60
        blob=f.git('hash-object','-w','--stdin',data=random.Random(16).randbytes(512*1024))
        tree=f.git('mktree',data=f'100644 blob {blob}\tpayload\n'.encode())
        f.commit=f.git('commit-tree',tree,data=(revision[2:]+'\n').encode())
        f.git('update-ref','refs/heads/main',f.commit)
        url=f.https(transfer_delay=.04);self.addCleanup(f.server.shutdown)
        tools=f.root/'startup-tools';tools.mkdir()
        entered=f.root/'git-entered';release=f.root/'git-release'
        wrapper=tools/'git'
        wrapper.write_text('#!'+sys.executable+'\nimport os,sys,time\nfrom pathlib import Path\n'
                           f'entered=Path({str(entered)!r});release=Path({str(release)!r})\n'
                           'entered.touch()\nwhile not release.exists(): time.sleep(.01)\n'
                           f'os.execv({shutil.which("git")!r},["git",*sys.argv[1:]])\n')
        wrapper.chmod(0o700)
        env=dict(f.env,PATH=str(tools),TERM='xterm')
        for variable in ('NO_COLOR','CLICOLOR','CLICOLOR_FORCE'):env.pop(variable,None)
        for phase,terminal,color in (('local-resolution',True,{}),('initial-fetch',True,{'NO_COLOR':'1'}),('initial-fetch',False,{'CLICOLOR_FORCE':'1'})):
            with self.subTest(phase=phase,terminal=terminal,color=color):
                if phase=='initial-fetch':release.touch()
                f.resume.clear();f.stalled.clear();f.stalled_oids={f.commit}
                master,slave=pty.openpty();self.addCleanup(os.close,master)
                termios.tcsetwinsize(slave,(24,48))
                operands=[revision] if phase=='local-resolution' else [url,'HEAD']
                started=time.monotonic()
                process=subprocess.Popen([str(OPTIONS.binary),*operands,'--write-policy=all','--',
                                          sys.executable,'-c',"import sys;sys.stderr.write('COMMAND-START\\n');print('command-output')"],
                                         cwd=f.repo,env=env|color,stdout=subprocess.PIPE,stderr=slave if terminal else subprocess.PIPE)
                os.close(slave);f.children.append(process)
                transcript=bytearray()
                try:
                    while time.monotonic()-started<.25:
                        if terminal and select.select([master],[],[],.01)[0]:transcript.extend(os.read(master,65536))
                        elif not terminal:time.sleep(.01)
                    self.assertNotIn(b'git on',transcript,'indicator appeared before its delay')
                    if phase=='local-resolution':self.assertTrue(entered.exists(),'first Git invocation did not start')
                    else:self.assertTrue(f.stalled.wait(10),'initial fetch did not reach its controlled stall')
                    if terminal:
                        deadline=started+4
                        while transcript.count(b'git on')<2 and time.monotonic()<deadline:
                            if select.select([master],[],[],.05)[0]:transcript.extend(os.read(master,65536))
                        self.assertGreaterEqual(transcript.count(b'git on'),2,'preparation wait hid the animation')
                    else:time.sleep(.6)
                finally:
                    release.touch();f.resume.set()
                if terminal:
                    deadline=time.monotonic()+20
                    while process.poll() is None and time.monotonic()<deadline:
                        if select.select([master],[],[],.05)[0]:
                            try:transcript.extend(os.read(master,65536))
                            except OSError as error:
                                if error.errno==errno.EIO:break
                                raise
                output,errors=process.communicate(timeout=20)
                while select.select([master],[],[],.05)[0]:
                    try:chunk=os.read(master,65536)
                    except OSError as error:
                        if error.errno==errno.EIO:break
                        raise
                    if not chunk:break
                    transcript.extend(chunk)
                self.assertEqual(process.returncode,0,transcript)
                self.assertEqual(output,b'command-output\n')
                if terminal:
                    frames=re.findall('[\u2800-\u28ff]',transcript.decode('utf-8'))
                    self.assertGreaterEqual(len(set(frames)),2,'indicator did not animate Unicode dots')
                    if color.get('NO_COLOR'):self.assertNotRegex(transcript,rb'\x1b\[[0-9;]*m')
                    else:self.assertIn(b'\x1b[36m',transcript,'TTY indicator has no color accent')
                    visible=re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]',b'',transcript)
                    if phase=='initial-fetch':
                        self.assertIn(b'Receiving objects',visible)
                        self.assertRegex(visible,rb'\d+/\d+','status omits Git object counts')
                        self.assertRegex(visible,rb'[KM]iB/s','status omits actual transfer speed')
                        self.assertIn('━'.encode(),transcript,'status has no progress bar')
                    else:
                        self.assertIn(b':/progress-\\u{1b}',visible,'revision is missing or its controls are not escaped')
                        frames=visible.split(b'COMMAND-START')[0].decode('utf-8').split('\r')
                        self.assertLessEqual(max(sum(2 if unicodedata.east_asian_width(c) in ('W','F') else 1 for c in frame) for frame in frames),48)
                    self.assertLess(transcript.rfind(b'\x1b[2K'),transcript.index(b'COMMAND-START'))
                    self.assertNotIn(b'git on',transcript[transcript.index(b'COMMAND-START'):])
                else:self.assertEqual(errors,b'COMMAND-START\n')
                assert_no_run_state(self, f.root)

    @unittest.skipUnless(sys.platform=='linux','controlling Bash job and /proc state observation')
    def test_preparation_progress_during_native_git_with_tostop(self):
        import native_job_control
        f=self.f;url=f.https();self.addCleanup(f.server.shutdown)
        original=f.env
        for selection in ('automatic','explicit-fsck'):
            with self.subTest(selection=selection):
                f.env=dict(original)
                if selection=='explicit-fsck':
                    f.env.update(GIT_CONFIG_COUNT='1',GIT_CONFIG_KEY_0='fetch.fsckObjects',GIT_CONFIG_VALUE_0='false')
                tools=f.root/f'tostop-{selection}';tools.mkdir();(tools/'git').symlink_to(shutil.which('git'))
                shell=native_job_control.Shell(f);self.addCleanup(shell.close)
                shell.send(b'stty tostop\n');shell.expect(shell.prompt)
                f.resume.clear();f.stalled.clear();f.stalled_oids={f.commit}
                runner_file=f.root/f'tostop-runner-{selection}'
                argv=['/bin/sh','-c','printf "%s" "$$" > "$1"; shift; exec "$@"','fixture',str(runner_file),str(OPTIONS.binary),url,'HEAD','--write-policy=all','--',sys.executable,'-c',"import sys,termios;assert termios.tcgetattr(0)[3]&termios.ECHO;assert sys.stdin.readline()=='typed-ahead\\n';print('child-completed')"]
                shell.send((f'PATH={shlex.quote(str(tools))} TERM=xterm '+shlex.join(argv)+'; printf "STATUS:%d\\n" "$?"\n').encode())
                try:
                    self.assertTrue(f.stalled.wait(10),'native fetch did not reach controlled TTY stall')
                    runner=int(runner_file.read_text());foreground=os.tcgetpgrp(shell.fd)
                    shell.groups.update((runner,foreground))
                    self.assertEqual(foreground,runner,'native Git took the terminal from git-on')
                    self.assertEqual(os.getsid(runner),shell.pid)
                    shell.send(b'typed-')
                    deadline=time.monotonic()+1.3
                    while time.monotonic()<deadline:
                        shell.read()
                        if native_job_control.state(runner)=='T':break
                    self.assertNotEqual(native_job_control.state(runner),'T',f'runner stopped during native Git ownership: {bytes(shell.transcript)!r}')
                    self.assertGreaterEqual(shell.transcript.count(b'git on'),2,'native Git wait hid the animation')
                    shell.send(b'ahead\n')
                    deadline=time.monotonic()+.2
                    while time.monotonic()<deadline:shell.read()
                    f.resume.set()
                    output=shell.expect(shell.prompt,timeout=15)
                    self.assertIn(b'STATUS:0',output)
                    visible=re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]',b'',output)
                    self.assertIn(b'child-completed',visible.splitlines())
                    self.assertNotIn(b'git on',output[output.rindex(b'child-completed'):])
                    screen=terminal_lines(output)
                    for artifact in ('git on HEAD','Receiving objects','Compressing objects','━'):
                        self.assertNotIn(artifact,screen,'preparation left an indicator on screen')
                    self.assertEqual(os.tcgetpgrp(shell.fd),shell.pid)
                    assert_no_run_state(self, f.root)
                    shell.groups.clear()
                finally:f.resume.set()
        f.env=original

    def test_materialization_progress_is_delayed_tty_only_and_cleared_before_child_or_error(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        programs=f.root/'progress-tools'; programs.mkdir(); (programs/'git').symlink_to(shutil.which('git'))
        env=dict(f.env,PATH=str(programs),TERM='xterm')
        f.stalled_oids={f.commit}
        for outcome in ('success','error','cancel'):
            with self.subTest(outcome=outcome):
                shutil.rmtree(f.root/'cache',ignore_errors=True)
                f.resume.clear(); f.stalled.clear(); f.denied_oids=set()
                master,slave=pty.openpty()
                self.addCleanup(os.close,master)
                # Keep the slave open until the parent drains the transcript.
                self.addCleanup(os.close,slave)
                child="import sys; sys.stderr.write('CHILD-START\\n'); print('child output')"
                process=subprocess.Popen([str(OPTIONS.binary),url,'HEAD','--backend=copy','--write-policy=all','--',sys.executable,'-c',child],cwd=f.cwd,env=env,stdout=subprocess.PIPE,stderr=slave)
                f.children.append(process)
                self.assertTrue(f.stalled.wait(10),'materialization never reached controlled object stall')
                transcript=bytearray(); deadline=time.monotonic()+5
                while b'git on' not in transcript and time.monotonic()<deadline:
                    if select.select([master],[],[],.1)[0]: transcript.extend(os.read(master,65536))
                self.assertIn(b'git on',transcript)
                self.assertNotIn(b'CHILD-START',transcript)
                if outcome=='error': f.denied_oids={f.commit}
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
                    if outcome=='error':
                        self.assertRegex(transcript.rsplit(b'\x1b[2K', 1)[-1],
                                         rb'\b(?:HTTP (?:error )?403|returned error: 403)\b')
                assert_no_run_state(self, f.root)

    @unittest.skipUnless(sys.platform=='linux' and shutil.which('strace'), 'Linux strace object-I/O observation')
    def test_private_remote_objects_do_not_sync_disposable_payloads(self):
        f=self.f;trace=f.root/'private-object-io.trace'
        # The caller can request durability for its real repository. Only the
        # disposable private destination overrides that policy, not signatures
        # or object-integrity checks, and no source configuration is rewritten.
        env=dict(f.env,GIT_CONFIG_COUNT='2',GIT_CONFIG_KEY_0='core.fsync',GIT_CONFIG_VALUE_0='all',
            GIT_CONFIG_KEY_1='core.fsyncObjectFiles',GIT_CONFIG_VALUE_1='true')
        before=f.git('config','--list')
        result=subprocess.run(['strace','--kill-on-exit','-f','-yy','-e','trace=process,read,write,fsync,fdatasync',
            '-o',str(trace),str(OPTIONS.probe),'file://'+str(f.repo),'HEAD','--metrics-only'],
            cwd=f.cwd,env=env,input=b'read deep/file\nread deep/file\nreport\nexit\n',capture_output=True,timeout=40)
        if result.returncode:
            self.fail(f'private import failed ({result.returncode}): {result.stderr!r}\n'+trace.read_text()[-65536:])
        rows=[json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([row['data'] for row in rows if 'data' in row],['expected\n']*2)
        reports=[row['metrics'] for row in rows if 'metrics' in row]
        self.assertEqual(reports[-1]['remote_fetches'],4)
        self.assertEqual(reports[-1]['remote_fetches'],reports[-2]['remote_fetches'])
        lines = [line for line in trace.read_text().splitlines()
                 if '/received-pack-' in line and '/objects/' in line]
        self.assertTrue(any(re.search(r'\bwrite\(',line) for line in lines),'object writes were not traced')
        self.assertEqual([line for line in lines if re.search(r'\b(fsync|fdatasync)\(',line)],[])
        self.assertEqual(f.git('config','--list'),before)

    def test_remote_raw_import_reuses_evicted_objects_without_loose_blob_copies(self):
        f=self.f;entries=[];oids=[];count=300
        for index in range(count):
            prefix=f'raw-{index:03d}:'.encode();data=prefix+b'x'*(65536-len(prefix))
            oid=f.git('hash-object','-w','--stdin',data=data);oids.append(oid)
            entries.append(f'100644 blob {oid}\tf{index:03d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'raw eviction fixture\n');f.git('update-ref','refs/heads/main',commit)
        process=f.probe('file://'+str(f.repo),'HEAD:');f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'range 0 32 f{index:03d}')
            self.assertEqual(bytes(value['bytes']),f'raw-{index:03d}:'.encode()+b'x'*24)
        value,again=f.operation(process,'range 0 32 f000');self.assertEqual(bytes(value['bytes']),b'raw-000:'+b'x'*24)
        self.assertEqual(again['metrics']['remote_fetches'],count+2)
        self.assertEqual(again['metrics']['blob_loads'],last['metrics']['blob_loads'],'evicted raw blob was unpacked again')
        destination=next(f.root.glob('*/remote-repository'))
        self.assertEqual(list((destination/'objects/pack').iterdir()),[])
        self.assertFalse(any((destination/'objects'/oid[:2]/oid[2:]).exists() for oid in oids),'received blobs were recompressed into loose files')
        self.assertGreater(again['metrics']['retained_raw_blob_entries'],0,'working set did not actually evict payloads')
        self.assertLessEqual(again['metrics']['blob_cache_bytes'],again['metrics']['blob_cache_limit'])
        f.finish(process)

    def test_protocol_v2_reuses_native_clients_across_many_blob_demands(self):
        f=self.f;count=140;entries=[];oids=[]
        for index in range(count):
            oid=f.git('hash-object','-w','--stdin',data=f'unique-{index}\n'.encode());oids.append(oid)
            entries.append(f'100644 blob {oid}\tf{index:04d}\n')
        tree=f.git('mktree',data=''.join(entries).encode());commit=f.git('commit-tree',tree,data=b'session reuse\n');f.git('update-ref','refs/heads/main',commit)
        f.env, log = record_git_invocations(f.root, f.env)
        process=f.probe('file://'+str(f.repo));f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'read f{index:04d}')
            self.assertEqual(value['data'],f'unique-{index}\n')
        value,again=f.operation(process,'read f0000');self.assertEqual(value['data'],'unique-0\n')
        self.assertEqual(again['metrics']['blob_loads'],count)
        self.assertEqual(again['metrics']['remote_fetches'],count+2)
        f.finish(process)
        starts = git_invocations(log)
        self.assertTrue(starts)
        fetches=[args for args in starts if 'fetch' in args]
        decoders=[args for args in starts if 'cat-file' in args]
        self.assertEqual(fetches,[],'file session should stream all objects from the first fetch')
        self.assertEqual(decoders,[],'full-object sessions should decode in-process')
        self.assertEqual(again['metrics']['remote_session_fetch_operations'],count+2)
        self.assertEqual(again['metrics']['remote_session_decode_operations'],count+2)
        self.assertEqual(again['metrics']['remote_session_starts'],1)
        self.assertEqual(set(again['objects']),{commit,tree,*oids})

    @unittest.skipUnless(sys.platform=='linux', 'Linux /proc descriptor observation')
    def test_session_keeps_descriptors_bounded_after_raw_blobs_spill(self):
        import resource
        # Leave more than 64 on disk with eight hot blobs and one pending spill.
        f=self.f;count=74;size=64*1024+1;entries=[]
        for index in range(count):
            data=(index+1).to_bytes(4,'little')+bytes(size-4)
            oid=f.git('hash-object','-w','--stdin',data=data)
            entries.append(f'100644 blob {oid}\tf{index:04d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'bounded session parts\n');f.git('update-ref','refs/heads/main',commit)
        def limit_descriptors():
            resource.setrlimit(resource.RLIMIT_NOFILE,(64,64))
        process=subprocess.Popen([str(OPTIONS.probe),'file://'+str(f.repo),'HEAD','--metrics-only'],cwd=f.cwd,
            env=f.env,preexec_fn=limit_descriptors,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        f.children.append(process);f.line(process)
        for index in range(count):
            value,_=f.operation(process,f'range 0 4 f{index:04d}')
            self.assertEqual(value['bytes'],list((index+1).to_bytes(4,'little')))
            f.operation(process,'release')
        value,again=f.operation(process,'range 0 4 f0000')
        self.assertEqual(value['bytes'],[1,0,0,0])
        self.assertEqual(again['metrics']['blob_loads'],count)
        self.assertEqual(again['metrics']['remote_session_fetch_operations'],count+2)
        self.assertEqual(again['metrics']['remote_session_decode_operations'],count+2)
        self.assertEqual(again['metrics']['remote_session_starts'],1)
        self.assertEqual(again['metrics']['remote_raw_blob_bytes'],count*size)
        self.assertGreater(len(list(f.root.glob('*/raw-objects-*/*'))),64)
        self.assertLessEqual(len(list(Path(f'/proc/{process.pid}/fd').iterdir())),48)
        f.finish(process)

    def test_short_ssh_tag_reuses_the_bootstrap_connection(self):
        f = self.f
        f.git('update-ref', 'refs/tags/release', f.commit)
        url = f.ssh(protocol_v2=True)
        process = f.probe(url, 'release')
        self.assertEqual(f.line(process)['tree'], f.tree)
        value, _ = f.operation(process, 'read deep/file')
        self.assertEqual(value['data'], 'expected\n')
        f.finish(process)
        self.assertEqual((f.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def test_mounted_ssh_reuses_native_upload_pack_setup(self):
        if not (OPTIONS.require_fuse or OPTIONS.require_nfs):
            self.skipTest('actual FUSE or NFS execution required')
        f = self.f
        url = f.ssh(protocol_v2=True)
        env, log = record_git_invocations(f.root, f.env)
        env['GIT_ON_TEST_DISABLE_SSH_STANDBY'] = '1'
        backend = 'fuse' if OPTIONS.require_fuse else 'nfs'
        result = subprocess.run(
            [OPTIONS.binary, url, f.commit, '--backend=' + backend,
             '--write-policy=none', '--', 'cat', 'deep/file'],
            cwd=f.cwd, env=env, capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')
        calls = git_invocations(log)
        setup = [args for args in calls
                 if ('--git-dir=/dev/null' in args and 'ls-remote' in args)
                 or (args[-2:] == ['rev-parse', '--local-env-vars'])]
        self.assertEqual([next(operation for operation in ('ls-remote', 'rev-parse') if operation in args)
                          for args in setup], ['ls-remote', 'rev-parse'], calls)
        assert_no_run_state(self, f.root)

    def test_ssh_spare_can_prepare_while_first_advertisement_waits(self):
        f = self.f
        url = f.ssh(protocol_v2=True, max_sessions=2)
        release = f.root / 'release-primary'
        self.addCleanup(release.touch)
        shim = f.root / 'hold-first-upload-pack'
        shim.write_text(
            '#!/bin/sh\n'
            f'if mkdir {shlex.quote(str(f.root / "first-channel"))} 2>/dev/null; then\n'
            f'    while test ! -f {shlex.quote(str(release))}; do sleep 0.02; done\n'
            'fi\n'
            f'exec git-upload-pack {shlex.quote(str(f.repo))}\n'
        )
        shim.chmod(0o700)
        authorized = f.root / 'authorized'
        authorized.write_text(f'command="{shlex.quote(str(shim))}" ' + authorized.read_text())
        process = f.probe(url, 'main')
        try:
            ready, _, _ = select.select([process.stdout], [], [], 5)
            self.assertTrue(ready, 'the spare did not complete preparation before the first advertisement')
            self.assertEqual(f.line(process)['tree'], f.tree)
        finally:
            release.touch()
        f.finish(process)
        self.assertEqual((f.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

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

    def test_session_reads_full_objects_without_pack_staging(self):
        f=self.f
        process=subprocess.Popen([str(OPTIONS.probe),'file://'+str(f.repo),'HEAD:','--metrics-only'],cwd=f.cwd,
            env=f.env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0,start_new_session=True)
        f.children.append(process);f.line(process)
        root=next(f.root.glob('*/remote-repository')).parent
        selected=f"Path({str(root)!r})"
        exec(private_odb_fault(selected),{'Path':Path})
        value,report=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n')
        self.assertEqual(report['metrics']['remote_session_starts'],1)
        subtree=f.git('rev-parse','HEAD:deep')
        self.assertFalse((root/'remote-repository/objects'/subtree[:2]/subtree[2:]).exists())
        self.assertFalse(list(root.glob('raw-parts-*')))
        f.finish(process)

    def delta_session_fixture(self, prefix_size=20000, include_base=False, root_extras=None):
        import hashlib,zlib
        f=self.f;native=shutil.which('git')
        base=b'a'*prefix_size+b'base';wanted=b'a'*prefix_size+b'changed'
        base_oid=f.git('hash-object','-w','--stdin',data=base)
        wanted_oid=f.git('hash-object','-w','--stdin',data=wanted)
        entries=f'100644 blob {wanted_oid}\tfile\n'
        if include_base:entries+=f'100644 blob {base_oid}\tbase\n'
        extra_oids=[]
        for name, content in (root_extras or {}).items():
            oid=f.git('hash-object','-w','--stdin',data=content)
            entries+=f'100644 blob {oid}\t{name}\n'
            extra_oids.append(oid)
        tree=f.git('mktree',data=entries.encode())
        commit=f.git('commit-tree',tree,data=b'delta session\n');f.git('update-ref','refs/heads/main',commit)
        def integer(value):
            output=bytearray()
            while True:
                byte=value&127;value>>=7;output.append(byte|(128 if value else 0))
                if not value:return bytes(output)
        def entry(kind,size):
            first=(kind<<4)|(size&15);size>>=4
            return bytes([first|(128 if size else 0)])+(integer(size) if size else b'')
        delta=integer(len(base))+integer(len(wanted))+bytes([0xf0,prefix_size&255,(prefix_size>>8)&255,(prefix_size>>16)&255,7])+b'changed'
        data=b'PACK'+(2).to_bytes(4,'big')+(2).to_bytes(4,'big')
        data+=entry(3,len(base))+zlib.compress(base)
        data+=entry(7,len(delta))+bytes.fromhex(base_oid)+zlib.compress(delta)
        data+=hashlib.sha1(data).digest()
        tree_pack=run([native,'-C',f.repo,'pack-objects','--stdout'],
                      input=''.join(f'{oid}\n' for oid in [tree, *extra_oids]).encode(),env=f.env)
        trace=f.root/'delta-demands'
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
 with open({str(trace)!r},'a') as output:output.write(wanted+'\\n')
 send(b'packfile\\n')
 for offset in range(0,len(packs[wanted]),60000):send(b'\\x01'+packs[wanted][offset:offset+60000])
 sys.stdout.buffer.write(b'0000');sys.stdout.buffer.flush()
''')
        wrapper.chmod(0o700);f.env['PATH']=str(tools)+os.pathsep+f.env['PATH']
        return wanted,base_oid,wanted_oid,tree,commit,trace

    def test_session_imports_required_delta_pack_without_a_second_fetch(self):
        f=self.f;wanted,base_oid,wanted_oid,tree,commit,_=self.delta_session_fixture()
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
        self.assertEqual(again['metrics']['remote_decode_operations'],0)
        self.assertEqual(again['metrics']['remote_session_decode_operations'],2)
        self.assertEqual(set(again['objects']),{commit,tree,base_oid,wanted_oid})
        destination=next(f.root.rglob('remote-repository'))
        self.assertEqual(list((destination/'objects/pack').iterdir()),[])
        self.assertFalse(list(destination.parent.glob('raw-parts-*')))
        f.finish(process)

    def test_delta_base_requested_later_uses_its_received_pack_without_refetch(self):
        f=self.f;wanted,base_oid,_,tree,_,trace=self.delta_session_fixture(include_base=True)
        process=f.probe('file://'+str(f.repo));f.line(process)
        _,before=f.operation(process,'range 0 1 file')
        f.operation(process,'release')
        value,prepared=f.operation(process,'range 19990 100 base')
        self.assertEqual(bytes(value['bytes']),b'a'*10+b'base')
        self.assertEqual(before['metrics']['remote_session_fetch_operations'],prepared['metrics']['remote_session_fetch_operations'])
        self.assertEqual(trace.read_text().splitlines(),[tree,f.git('hash-object','--stdin',data=wanted)])
        f.operation(process,'release')
        value,reopened=f.operation(process,'range 19990 100 base')
        self.assertEqual(bytes(value['bytes']),b'a'*10+b'base')
        for field in ('remote_fetch_operations','remote_decode_operations','blob_loads'):
            self.assertEqual(prepared['metrics'][field],reopened['metrics'][field])
        f.finish(process)

    def test_copy_reuses_an_unopened_base_received_by_a_lazy_read(self):
        f=self.f
        # More than eight large extras would spill if copy first admitted
        # their verified bytes to the lazy read cache.
        extras={f'extra-{index}':bytes([index])*65537 for index in range(10)}
        wanted,base_oid,_,tree,_,trace=self.delta_session_fixture(include_base=True,root_extras=extras)
        process=f.probe(f.repo.as_uri());f.line(process)
        f.operation(process,'range 0 1 file')
        f.operation(process,'release')
        destination=f.root/'copy-snapshot';destination.mkdir()
        _, report=f.operation(process,'materialize '+str(destination))
        self.assertEqual((destination/'file').read_bytes(),wanted)
        self.assertEqual((destination/'base').read_bytes(),f.git('cat-file','blob',base_oid).encode())
        for name, content in extras.items():
            self.assertEqual((destination/name).read_bytes(),content)
        self.assertEqual(report['metrics']['spool_bytes'],0)
        self.assertEqual(trace.read_text().splitlines(),[tree,f.git('hash-object','--stdin',data=wanted)])
        f.finish(process)

    def test_real_mount_session_delta_reuses_fitting_payloads_in_ram(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        f=self.f;wanted,_,wanted_oid,tree,_,trace=self.delta_session_fixture(40000)
        code=r'''
import errno,os,sys
from pathlib import Path
expected=b'a'*40000+b'changed'
for _ in range(2):
 assert Path('file').read_bytes()==expected
runs=Path.cwd().parent
raw=list(runs.glob('raw-objects-*'))
assert len(raw)==1 and not list(raw[0].iterdir()), 'fitting delta payloads spilled to disk'
assert not list((runs/'remote-repository/objects/pack').iterdir())
if sys.argv[1]=='all':
 reader=os.open('file',os.O_RDONLY)
 try:
  with open('file','r+b',buffering=0) as writer:
   writer.write(b'changed in mount')
  assert os.pread(reader,16,0)==b'changed in mount'
  os.fsync(reader);os.fdatasync(reader)
  os.rename('file','moved')
  os.link('moved','alias')
  assert Path('alias').read_bytes().startswith(b'changed in mount')
  os.unlink('moved');os.unlink('alias')
  assert os.fstat(reader).st_nlink==0 and os.pread(reader,16,0)==b'changed in mount'
 finally:os.close(reader)
else:
 try:open('file','wb')
 except OSError as error:assert error.errno==errno.EROFS,error
 else:raise AssertionError('protected source was writable')
if sys.argv[1]=='untracked':
 Path('generated').write_bytes(b'generated bytes')
 assert Path('generated').read_bytes()==b'generated bytes'
'''
        for policy in ('none','untracked','all'):
            with self.subTest(policy=policy):
                trace.unlink(missing_ok=True)
                result=subprocess.run([str(OPTIONS.binary),'file://'+str(f.repo),'HEAD',
                    '--write-policy='+policy,'--',sys.executable,'-I','-S','-c',code,policy],
                    cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(trace.read_text().splitlines(),[tree,wanted_oid])
                assert_no_run_state(self, f.root)
                self.assertEqual(f.git('cat-file','blob',wanted_oid).encode(),wanted)

    def large_delta_storage_fixture(self):
        size=7*1024*1024
        _,_,wanted_oid,tree,_,trace=self.delta_session_fixture(size-7)
        from filesystem_quota import STORAGE_OBSERVATION
        command=[sys.executable,'-I','-S','-c',STORAGE_OBSERVATION+r'''
import sys
expected=b'a'*(7*1024*1024-7)+b'changed'
assert Path('file').stat().st_size==len(expected)
assert Path('file').read_bytes()==expected
assert Path('file').read_bytes()==expected
assert allocated()<9*1024*1024,('delta import retained an extra decoded payload',allocated())
if sys.argv[1]=='rewrite':
 expected=b'z'*len(expected)
 with open('file','r+b',buffering=0) as output:assert output.write(expected)==len(expected)
 assert Path('file').read_bytes()==expected
 assert allocated()<9*1024*1024,('delta mutation duplicated source',allocated())
print('delta payload verified')
''']
        return command,wanted_oid,tree,trace

    def test_delta_base_is_not_a_second_materialized_file(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        f=self.f;command,wanted_oid,tree,trace=self.large_delta_storage_fixture()
        clone=f.root/'delta-clone'
        run(['git','clone','--no-local','--quiet','file://'+str(f.repo),str(clone)],env=f.env)
        for mode,policy in (('read','none'),('rewrite','all')):
            with self.subTest(mode=mode):
                trace.unlink(missing_ok=True)
                direct=subprocess.run([*command,mode],cwd=clone,env=f.env,capture_output=True,timeout=30)
                self.assertEqual(direct.returncode,0,direct.stderr)
                result=subprocess.run([str(OPTIONS.binary),'file://'+str(f.repo),'HEAD','--write-policy='+policy,'--',*command,mode],
                                      cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,direct.stdout)
                self.assertEqual(trace.read_text().splitlines(),[tree,wanted_oid])
                assert_no_run_state(self, f.root)

    def test_delta_decode_and_rewrite_succeed_on_nine_mib_tmpfs(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        from filesystem_quota import compare_on_tmpfs
        f=self.f;command,_,_,_=self.large_delta_storage_fixture()
        compare_on_tmpfs(self,f.root,OPTIONS.binary,f.repo,f.env['PATH'],[*command,'rewrite'],source=f.repo.as_uri())

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
 if b' blob ' in data.split(b'\\n',1)[0]:
  fault=os.environ['NATIVE_DECODER_FAULT']
  if fault=='corrupt':data=data.replace(b'expected\\n',b'changed!\\n',1)
  elif fault=='truncated':data=data[:-3]
  elif fault=='exit':result.returncode=42
 sys.stdout.buffer.write(data)
 raise SystemExit(result.returncode)
os.execv(native,[native,*args])
''')
        wrapper.chmod(0o700);f.env['PATH']=str(tools)+os.pathsep+f.env['PATH']
        for fault,message in [('corrupt','Git object hash mismatch'),
                              ('truncated','truncated Git object'),
                              ('exit','cannot decode received Git objects')]:
            with self.subTest(fault=fault):
                f.env['NATIVE_DECODER_FAULT']=fault
                process=f.probe('file://'+str(f.repo));f.line(process)
                with self.assertRaisesRegex(AssertionError,message):
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
            process=subprocess.Popen([str(binary),'file://'+str(f.repo),'HEAD:'],cwd=f.cwd,
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
                            (stage,) = f.root.glob('*/remote-repository/pack-staging')
                            self.assertEqual(len(list(stage.resolve().glob('received-pack-*'))),
                                             1, 'per-demand packs accumulated beside the selection')
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

    @unittest.skipUnless(sys.platform=='linux', 'Linux source-pack mapping observation')
    def test_idle_local_pack_mappings_do_not_retain_the_complete_source_pack(self):
        f=self.f; f.cwd=f.repo; entries=[]; expected=[]; generator=random.Random(140019)
        for index in range(24):
            data=generator.randbytes(2*1024*1024)
            blob=f.git('hash-object','-w','--stdin',data=data)
            entries.append(f'100644 blob {blob}\tf{index:03d}\n');expected.append((data[0],data[-1]))
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'packed mapping retention fixture\n')
        f.git('update-ref','refs/heads/main',commit)
        f.git('-c','core.compression=0','repack','-adf','--window=0','--depth=0')
        packs=list((f.repo/'objects/pack').glob('*.pack'))
        self.assertEqual(len(packs),1)
        self.assertGreater(packs[0].stat().st_size,48*1024*1024)
        index=packs[0].with_suffix('.idx')
        packed=f.git('verify-pack','-v',str(index)).splitlines()
        self.assertEqual(sum(' blob ' in line for line in packed),24)
        self.assertTrue(all(len(line.split())==5 for line in packed if ' blob ' in line),'fixture acquired delta chains')
        process=f.probe('local');f.line(process)
        for number,(first,_) in enumerate(expected):
            value,report=f.operation(process,f'range 0 1 f{number:03d}')
            self.assertEqual(value['bytes'],[first])
        _,report=f.operation(process,'release')
        self.assertEqual(report['metrics']['active_blob_readers'],0)
        # smaps Size measures retained mappings, independently of whether clean
        # file-backed pages have been reclaimed under host memory pressure.
        text=Path(f'/proc/{process.pid}/smaps').read_text()
        mapped_kib=0
        for section in re.split(r'(?=^[0-9a-f]+-[0-9a-f]+ )',text,flags=re.M):
            if section and str(packs[0]) in section.splitlines()[0]:
                mapped_kib+=int(re.search(r'^Size:\s+(\d+) kB$',section,re.M)[1])
        status=Path(f'/proc/{process.pid}/status').read_text()
        rss_kib=int(re.search(r'^VmRSS:\s+(\d+) kB$',status,re.M)[1])
        self.assertGreater(mapped_kib,0)
        self.assertLessEqual(mapped_kib,24*1024,'idle source pack exceeded the 16 MiB soft budget plus an active-window allowance')
        for number,(_,last) in enumerate(expected):
            value,again=f.operation(process,f'range {2*1024*1024-1} 1 f{number:03d}')
            self.assertEqual(value['bytes'],[last])
        self.assertEqual(again['metrics']['blob_loads'],24,'window eviction triggered object decoding')
        self.assertEqual(again['metrics']['blob_cache_limit'],16*1024*1024)
        f.finish(process)
        print('source pack mapping evidence:',json.dumps(dict(mapped_kib=mapped_kib,rss_kib=rss_kib,pack_bytes=packs[0].stat().st_size,blob_loads=24)),flush=True)

    def test_first_large_loose_range_stays_hot_without_spilling(self):
        f=self.f
        mib = 1024 * 1024
        before = OPTIONS.before_resource_probe if sys.platform == 'linux' else None
        sizes = (16 * mib, 64 * mib, 128 * mib) if before else (128 * 1024,)
        for size in sizes:
            chunk = b'x' * min(size, mib)
            with tempfile.TemporaryFile() as output:
                writer=subprocess.Popen(['git','-C',str(f.repo),'hash-object','-w','--stdin'],env=f.env,stdin=subprocess.PIPE,stdout=output)
                for _ in range(size//len(chunk)): writer.stdin.write(chunk)
                writer.stdin.close(); self.assertEqual(writer.wait(),0); output.seek(0); blob=output.read().decode().strip()
            tree=f.git('mktree',data=f'100644 blob {blob}\tlarge\n'.encode())
            for binary in ([before] if before else [])+[OPTIONS.probe]:
                process=subprocess.Popen([str(binary),'local',tree],cwd=f.repo,env=f.env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
                f.children.append(process); f.line(process)
                value,report=f.operation(process,'range 1234 16 large')
                self.assertEqual(bytes(value['bytes']),b'x'*16)
                repeated,again=f.operation(process,'range 4321 16 large')
                self.assertEqual(repeated,value)
                if binary==OPTIONS.probe:
                    self.assertEqual(report['metrics']['spool_bytes'],0)
                    self.assertEqual(report['metrics']['hot_blob_cache_entries'],1)
                    self.assertGreaterEqual(report['metrics']['hot_blob_cache_bytes'],size)
                    self.assertEqual(report['metrics']['streamed_blob_loads'],1)
                self.assertEqual(again['metrics']['spool_writes'],report['metrics']['spool_writes'])
                if before:
                    status=Path(f'/proc/{process.pid}/status').read_text()
                    peak_kib=int(re.search(r'^VmHWM:\s+(\d+)',status,re.M).group(1))
                    print('loose range evidence:',json.dumps(dict(binary=str(binary),object_mib=size//mib,peak_resident_kib=peak_kib,metrics=report['metrics'])),flush=True)
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
        owner = directory / 'ssh-control-owner'
        self.assertTrue(owner.is_file())
        self.assertFalse(owner.is_symlink())
        def cleanup():
            with contextlib.suppress(ProcessLookupError): os.killpg(state['pid'],signal.SIGKILL)
            if process.poll() is None: process.kill()
            process.communicate(timeout=10)
            if directory.exists(): subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.addCleanup(cleanup)
        active=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
        self.assertNotEqual(active.returncode,0); self.assertTrue((directory/'backing').exists())
        self.assertFalse((directory/'.state').exists())
        repository=directory/'remote-repository'
        self.assertEqual(f.git('for-each-ref','refs/git-on/',at=repository),'')
        mountpoint=directory/'mount'
        source=fuse_mount_source(mountpoint)
        self.assertTrue(source.startswith('git-on:'))
        def mounted():
            return fuse_mount_source(mountpoint)==source
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
        # Kernel flock ownership ends with the killed process.
        assert_run_unlocked(directory)
        print('SIGKILL runtime evidence:',json.dumps(dict(mount_remaining=mounted(),
              mount_errno=errno.ENOTCONN,child_alive=True,ssh_master_alive=True,
              native_repository_retained=True,locks_released=True)),flush=True)
        os.killpg(state['pid'],signal.SIGKILL); process.communicate(timeout=10)
        stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.assertEqual(stale.returncode,0,stale.stderr); self.assertFalse(directory.exists())
        self.assertFalse(mounted())
        self.assertNotEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertTrue(select.select([master_fd],[],[],5)[0],'recorded owned master survived stale recovery')
        assert_no_run_state(self, f.root)

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
                child=int(ready.read_text()); directory,=run_directories(f.root)
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
                candidates=run_directories(f.root)
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
        self.assertFalse((directory/'.state').exists())
        source=fuse_mount_source(mountpoint)
        self.assertTrue(source.startswith('git-on:'))
        def mounted():
            return fuse_mount_source(mountpoint)==source
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
        directory=Path(ready.read_text())
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
        self.assertFalse((directory/'.state').exists())
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
        assert_no_run_state(self, f.root)

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
            else: process.send_signal(signal.SIGTERM)
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

    def handled_term_during_active_remote_read(self,native):
        f=self.f;url=f.https();self.addCleanup(f.server.server_close);self.addCleanup(f.server.shutdown)
        f.stalled.clear();f.resume.clear();f.stalled_oids={f.blob}
        ready=f.root/'handled-read-ready'
        code="""
import json,os,signal,sys,threading
from pathlib import Path
signal.signal(signal.SIGTERM,lambda *args:sys.exit(42))
Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)}))
def read():
 try:Path('deep/file').read_bytes()
 except OSError:pass
threading.Thread(target=read,daemon=True).start()
signal.pause()
"""
        env=dict(f.env,READY=str(ready))
        if native:env.update(GIT_CONFIG_COUNT='1',GIT_CONFIG_KEY_0='fetch.fsckObjects',GIT_CONFIG_VALUE_0='true')
        process=subprocess.Popen([str(OPTIONS.binary),url,'HEAD','--',sys.executable,'-I','-S','-c',code],
                                 cwd=f.cwd,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process);directory=None
        try:
            self.assertTrue(f.stalled.wait(timeout=20),'remote read never reached the controlled stall')
            state=json.loads(ready.read_text());directory=Path(state['run'])
            process.send_signal(signal.SIGTERM)
            _,errors=process.communicate(timeout=10)
            self.assertEqual(process.returncode,42,errors)
            self.assertFalse(directory.exists(),'handled termination must clean its run')
        finally:
            f.resume.set();f.stalled_oids.clear()
            if process.poll() is None:process.kill();process.communicate(timeout=10)
            if ready.exists():
                with contextlib.suppress(ProcessLookupError):os.killpg(json.loads(ready.read_text())['pid'],signal.SIGKILL)
            if directory is not None and directory.exists():
                cleanup=subprocess.run([str(OPTIONS.binary),'--cleanup',str(directory)],capture_output=True,timeout=20)
                self.assertEqual(cleanup.returncode,0,cleanup.stderr)

    def test_handled_term_during_active_remote_read_preserves_child_exit(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        for native in (False,True):
            with self.subTest(native_fetch=native):self.handled_term_during_active_remote_read(native)

    def test_review_real_mount_stalled_lazy_fetch_shutdown(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; self.stall_url=f.https(); self.addCleanup(f.server.shutdown)
        for escaped in (False,True):
            with self.subTest(escaped=escaped):
                if OPTIONS.before_binary and escaped: self.stalled_mount_shutdown(OPTIONS.before_binary,escaped,True)
                self.stalled_mount_shutdown(OPTIONS.binary,escaped,False)

    @unittest.skipUnless(OPTIONS.require_fuse or OPTIONS.require_nfs, 'requires a native mount')
    def test_review_real_mount_caught_run_odb_fault_forces_runner_failure(self):
        f = self.f
        url = f.https()
        self.addCleanup(f.server.shutdown)
        env = dict(f.env, GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='fetch.fsckObjects',
                   GIT_CONFIG_VALUE_0='false')
        code = "import errno,os; from pathlib import Path\n" + private_odb_fault() + """
try:
    Path('deep/file').read_bytes()
except OSError as error:
    assert error.errno == errno.EIO, error
else:
    raise AssertionError('private storage fault not observed')
"""
        result = subprocess.run([str(OPTIONS.binary), url, 'HEAD', '--', sys.executable, '-c', code],
                                cwd=f.cwd, env=env, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'remote object loading failed', result.stderr)
        self.assertIn(b'execution incomplete', result.stderr)
        assert_no_run_state(self, f.root)

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
            for fault in ('transport','odb'):
                for operation,path in (('create','created.tmp'),('mkdir','created-dir')):
                    with self.subTest(binary=str(binary),fault=fault,operation=operation):
                        shutil.rmtree(f.root/'cache',ignore_errors=True)
                        f.requests.clear(); f.denied_oids={f.ignore} if fault=='transport' else set()
                        code="import json,os,sys; from pathlib import Path\n"
                        if fault=='odb':code+=private_odb_fault()
                        code+="try:\n"
                        code+=(" fd=os.open(sys.argv[1],os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600); os.close(fd)\n" if operation=='create' else " os.mkdir(sys.argv[1])\n")
                        code+="except OSError as error:\n print(json.dumps({'errno':error.errno,'exists':os.path.lexists(sys.argv[1])}),flush=True)\nelse:\n raise AssertionError('ignore acquisition fault permitted mutation')\n"
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
                        assert_no_run_state(self, f.root)
                        print('ignore acquisition errno regression evidence:',json.dumps(dict(binary=str(binary),before=expected==errno.EROFS,fault=fault,operation=operation,errno=expected)),flush=True)
        # With healthy rules, policy denial retains its distinct EROFS meaning.
        f.denied_oids.clear(); shutil.rmtree(f.root/'cache',ignore_errors=True)
        code="import errno,os\nfor operation in (lambda: os.open('denied',os.O_CREAT|os.O_WRONLY,0o600),lambda: os.mkdir('denied-dir')):\n try: operation()\n except OSError as error: assert error.errno==errno.EROFS,error\n else: raise AssertionError('unignored mutation allowed')\nfd=os.open('created.tmp',os.O_CREAT|os.O_WRONLY,0o600); os.close(fd)\nos.mkdir('created-dir')\n"
        result=subprocess.run([str(OPTIONS.binary),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr)
        assert_no_run_state(self, f.root)

    def test_real_mount_delayed_ignore_acquisition_preserves_policy(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=self.ignore_policy_fixture()
        for operation,path,allowed in (('create','created.tmp',True),('mkdir','created-dir',True),('create','denied',False),('mkdir','denied-dir',False)):
            with self.subTest(operation=operation,allowed=allowed):
                f.stalled.clear(); f.resume.clear(); f.stalled_oids={f.ignore}
                ready=f.root/'ignore-request-ready'; handled=f.root/'ignore-request-handled';observed=f.root/'ignore-result'
                for marker in (ready,handled,observed):marker.unlink(missing_ok=True)
                code="import errno,json,os,signal,sys; from pathlib import Path\ndef handler(*args): Path(os.environ['HANDLED']).touch()\nsignal.signal(signal.SIGTERM,handler)\nPath(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)}))\n"
                code+=f"allowed={allowed!r}\ntry:\n"
                code+=(" fd=os.open(sys.argv[1],os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600);os.close(fd)\n" if operation=='create' else " os.mkdir(sys.argv[1])\n")
                code+="except OSError as error:\n assert not allowed and error.errno==errno.EROFS,error\n assert not os.path.lexists(sys.argv[1])\nelse:\n assert allowed,'forbidden mutation succeeded'\n assert os.path.exists(sys.argv[1])\nassert Path(os.environ['HANDLED']).exists(),'handler not delivered after source completed'\nPath(os.environ['OBSERVED']).write_text(str(allowed))\n"
                env=dict(f.env,READY=str(ready),HANDLED=str(handled),OBSERVED=str(observed))
                process=subprocess.Popen([str(OPTIONS.binary),url,'HEAD','--',sys.executable,'-c',code,path],cwd=f.cwd,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                f.children.append(process); state=None
                try:
                    self.assertTrue(f.stalled.wait(timeout=20),'ignore request did not reach controlled source delay')
                    self.assertTrue(ready.exists());state=json.loads(ready.read_text())
                    os.kill(state['pid'],signal.SIGTERM)
                    # Observe completion outside FUSE: probing the mount here
                    # could itself block behind the controlled callback.
                    time.sleep(.05)
                    self.assertFalse(observed.exists(),'write completed before ignore rules were obtained')
                    self.assertIsNone(process.poll(),'write failed while policy source was merely delayed')
                    f.resume.set()
                    _,errors=process.communicate(timeout=20)
                    self.assertEqual(process.returncode,0,errors)
                    self.assertTrue(handled.exists())
                    self.assertEqual(observed.read_text(),str(allowed))
                    self.assertNotIn(b'execution incomplete',errors)
                    assert_no_run_state(self, f.root)
                finally:
                    f.resume.set();f.stalled_oids.clear()
                    if process.poll() is None:
                        if state is not None:
                            with contextlib.suppress(ProcessLookupError):os.killpg(state['pid'],signal.SIGKILL)
                        process.kill()
                    process.communicate(timeout=20)
                    if state is not None and Path(state['run']).exists():
                        cleanup=subprocess.run([str(OPTIONS.binary),'--cleanup',state['run']],capture_output=True,timeout=20)
                        self.assertEqual(cleanup.returncode,0,cleanup.stderr)

    def delayed_mount_read(self,mode):
        f=self.f
        f.stalled.clear();f.resume.clear();f.stalled_oids={f.blob}
        ready=f.root/'request-ready';handled=f.root/'request-handled'
        for marker in (ready,handled):marker.unlink(missing_ok=True)
        code="import ctypes,json,os,signal,sys; from pathlib import Path\n"
        if mode=='handled-exit':
            code+="def handle(*args):\n Path(os.environ['HANDLED']).touch()\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM,handle)\n"
        elif mode=='caught-return':
            code+="def handle(*args): Path(os.environ['HANDLED']).touch()\nsignal.signal(signal.SIGTERM,handle)\n"
        code+="Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)}))\n"
        if mode in ('plain','caught-return'):
            # Observe the actual result: Python's file wrappers could hide a
            # synthetic EINTR by retrying the open/read automatically.
            code+="libc=ctypes.CDLL(None,use_errno=True)\nlibc.open.argtypes=[ctypes.c_char_p,ctypes.c_int];libc.open.restype=ctypes.c_int\nlibc.read.argtypes=[ctypes.c_int,ctypes.c_void_p,ctypes.c_size_t];libc.read.restype=ctypes.c_ssize_t\nfd=libc.open(b'deep/file',os.O_RDONLY);assert fd>=0,('open',ctypes.get_errno())\nbuffer=ctypes.create_string_buffer(64)\ncount=libc.read(fd,buffer,64);assert count==9,('read',count,ctypes.get_errno())\nassert buffer.raw[:count]==b'expected\\n'\nassert libc.read(fd,buffer,64)==0\nlibc.close(fd)\nos.write(1,b'expected\\n')\n"
            if mode=='caught-return':code+="assert Path(os.environ['HANDLED']).exists()\n"
        else:code+="assert open('deep/file').read()=='expected\\n'\n"
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),self.signal_url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=dict(f.env,READY=str(ready),HANDLED=str(handled)),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process);state=None;directory=None
        try:
            self.assertTrue(f.stalled.wait(timeout=20),'read did not reach controlled source delay')
            self.assertTrue(ready.exists());state=json.loads(ready.read_text());directory=Path(state['run'])
            if mode!='plain':os.killpg(state['pid'],signal.SIGINT if mode=='python-int' else signal.SIGTERM)
            if mode in ('plain','caught-return'):
                # Harness-only observation window; no product read deadline.
                # Availability has not changed and no I/O error was injected.
                time.sleep(.05)
                self.assertIsNone(process.poll(),'read failed while the valid source was merely delayed')
            f.resume.set()
            output,errors=process.communicate(timeout=20)
            self.assertEqual(process.returncode,-signal.SIGINT if mode=='python-int' else 0,errors)
            if mode in ('plain','caught-return'):self.assertEqual(output,b'expected\n')
            if mode in ('caught-return','handled-exit'):self.assertTrue(handled.exists(),'handler did not run')
            self.assertNotIn(b'execution incomplete',errors)
            self.assertFalse(directory.exists(),'completed command did not clean its run')
        finally:
            f.resume.set();f.stalled_oids.clear()
            if process.poll() is None:
                if state is not None:
                    with contextlib.suppress(ProcessLookupError):os.killpg(state['pid'],signal.SIGKILL)
                process.kill()
            process.communicate(timeout=20)
            if directory is not None and directory.exists():
                cleanup=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
                self.assertEqual(cleanup.returncode,0,cleanup.stderr)

    def test_real_mount_delayed_source_returns_data_without_invented_error(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        f=self.f;self.signal_url=f.https();self.addCleanup(f.server.shutdown)
        for mode in ('plain','caught-return','handled-exit','python-int'):
            with self.subTest(mode=mode):self.delayed_mount_read(mode)

    def test_native_failed_verification_never_executes_command(self):
        f=self.f; signed,allowed=self.signing(); sentinel=f.root/'command-executed'
        allowed.write_text('')
        for revision in (signed,f.commit,f.tree):
            result=subprocess.run([str(OPTIONS.binary.resolve()),'--verify',revision,'--',sys.executable,'-c',f'from pathlib import Path; Path({str(sentinel)!r}).touch()'],cwd=f.repo,env=f.env,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,125,result.stderr)
            self.assertTrue(b'execution forbidden' in result.stderr or b'raw trees' in result.stderr,result.stderr)
            self.assertFalse(sentinel.exists())
            assert_no_run_state(self, f.root)

if __name__=='__main__': unittest.main(argv=[__file__,*ARGS])
