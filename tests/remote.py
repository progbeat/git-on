#!/usr/bin/env python3
"""Deterministic native Git, HTTPS and OpenSSH acceptance. Keys are disposable.

The source probe runs the production loader without a mount. --require-fuse
and --require-nfs also execute the CLI through their native mounts.
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
from urllib.parse import quote_from_bytes, urlsplit
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
from support.run_storage import run_directories, run_id, run_lock, ssh_owner_marker

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
        # The fixture adds a directory above Git-on's own run directory.
        # Keep that extra prefix short enough for OpenSSH on macOS.
        self.temp = tempfile.TemporaryDirectory(
            prefix='git-on-transport-', dir='/tmp' if sys.platform == 'darwin' else None,
        )
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
        self.servers = []
    def git(self, *args, data=None, at=None):
        return run(['git', '-C', at or self.repo, *args], input=data, env=self.env).decode(errors='surrogateescape').strip()
    def close(self):
        self.resume.set()
        for stop in reversed(self.servers):
            stop()
        if hasattr(self,'server'):
            self.server.shutdown()
            self.server.server_close()
        for process in reversed(self.children):
            if process.poll() is None:
                process.terminate()
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=5)
        for handle in self.handles: handle.close()
        self.temp.cleanup()
    def https(self, authorization=None, transfer_delay=0):
        return self.https_server(authorization, transfer_delay).url

    def https_server(self, authorization=None, transfer_delay=0):
        from support.http2_server import Multiplexer

        backend = self.http1_upstream(authorization, transfer_delay)
        server = Multiplexer(self, backend, None)
        server.start(self.servers.append)
        return server

    def http1_upstream(self, authorization=None, transfer_delay=0):
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
                    else: fields.append((name.decode(), value.strip().decode()))
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
                fixture.requests.append(dict(
                    path=path, request_bytes=len(data), response_bytes=len(body), wants=wants,
                    shallow=b'deepen 1\n' in data, headers=self.headers.get_all('X-Git-On-Fixture', []),
                ))
        class Server(http.server.ThreadingHTTPServer):
            request_queue_size = socket.SOMAXCONN
        server = Server(('127.0.0.1', 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); context.load_cert_chain(cert, key)
        server.socket = context.wrap_socket(server.socket, server_side=True)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        self.server = server
        self.env['GIT_SSL_CAINFO'] = str(cert)
        return f'https://127.0.0.1:{server.server_port}/server.git'
    def ssh(self, encrypted=False, protocol_v2=True, max_sessions=None):
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
    def probe(self, source, revision='HEAD', verify=False, eager=False):
        flags = [flag for enabled, flag in ((verify, '--verify'), (eager, '--eager')) if enabled]
        process = subprocess.Popen([str(OPTIONS.probe.resolve()), source, revision, *flags], cwd=self.cwd, env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
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
    def wait_for_packs(self, process, count, *keys):
        # Verified objects are readable before the enclosing pack ends.
        deadline = time.monotonic()+10
        while True:
            process.stdin.write(b'report\n'); process.stdin.flush()
            report = self.line(process)
            if all(report['metrics'][key] >= count for key in keys):
                return report
            if time.monotonic() >= deadline:
                raise AssertionError(f'pack counters did not reach {count}: {report["metrics"]}')
            time.sleep(.01)
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

    def native_version(self, version):
        wrapper = self.tools / 'git'
        wrapper.write_text(f'''#!{sys.executable}
import json, os, sys
args = sys.argv[1:]
with open({str(self.root / 'native-requests')!r}, 'a') as output:
    output.write(json.dumps(args) + '\\n')
if args == ['--version']:
    print('git version ' + {version!r}); sys.exit(0)
os.execv({self.native_git!r}, [{self.native_git!r}, *args])
''')
        wrapper.chmod(0o700)
        self.env['PATH'] = str(self.tools) + os.pathsep + os.environ['PATH']

    def failure(self, source='https://github.com/owner/repository.git'):
        result = subprocess.run([str(OPTIONS.probe), source, 'HEAD'], cwd=self.root,
                                env=self.env, capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(run_directories(self.root), [])
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
        self.assertEqual(run_directories(self.root), [])

    def test_github_publickey_failure_offers_conditional_https_without_retry(self):
        self.native_version('2.45.1')
        ssh = self.tools / 'ssh'
        ssh.write_text("#!/bin/sh\nprintf '%s\\n' 'git@github.com: Permission denied (publickey).' >&2\nexit 255\n")
        ssh.chmod(0o700)
        self.env['GIT_SSH_COMMAND'] = str(ssh)
        for source in ('git@github.com:owner/repository.git',
                       'ssh://git@github.com/owner/repository.git'):
            with self.subTest(source=source):
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
    def test_explicit_local_paths_preserve_original_bytes(self):
        f=self.f
        weird=f.root/os.fsdecode(b'server-\xff.git')
        literal = f.cwd / '~git-on-test-source'
        literal.symlink_to(f.repo, target_is_directory=True)
        paths = [str(f.repo), '../server.git', literal.name]
        for separators in ('', '/'):
            encoded = quote_from_bytes(os.fsencode(f.repo), safe=separators)
            encoded = '%2F' + encoded[1:] if separators else encoded
            source = 'file://' + encoded
            self.assertEqual(f.git('ls-remote', source, 'HEAD', at=f.cwd).split()[0], f.commit)
            paths.append(source)
        try:
            weird.symlink_to(f.repo, target_is_directory=True)
        except OSError as error:
            if error.errno != errno.EILSEQ:
                raise
        else:
            paths.append(str(weird))
        for source in paths:
            with self.subTest(source=source):
                process=f.probe(source,'main')
                self.assertEqual(f.line(process)['tree'],f.tree)
                value,_=f.operation(process,'read deep/file')
                self.assertEqual(value['data'],'expected\n'); f.finish(process)
    def test_relative_local_remote_from_bare_subdirectory_follows_native_git(self):
        f = self.f
        caller = f.root / 'caller.git'
        f.git('init', '--bare', '--quiet', str(caller), at=f.root)
        nested = caller / 'nested'
        nested.mkdir()
        source = caller / f.repo.name
        f.git('clone', '--bare', '--quiet', str(f.repo), str(source), at=f.root)
        f.git('config', 'remote.origin.url', '../' + source.name, at=caller)
        decoy = f.git('mktree', data=f'100644 blob {f.blob}\tother\n'.encode())
        f.git('update-ref', 'HEAD', f.git('commit-tree', decoy, data=b'decoy\n'))
        self.assertEqual(f.git('ls-remote', 'origin', 'HEAD', at=nested).split()[0], f.commit)
        expected = f.git('show', 'HEAD:other', at=source).encode() + b'\n'
        result = subprocess.run([OPTIONS.binary.resolve(), 'origin', 'HEAD', '--backend=copy',
                                 '--write-policy=all', '--', 'cat', 'other'],
                                cwd=nested, env=f.env, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, expected)
    def test_relative_legacy_local_remotes_use_the_caller_worktree_root(self):
        f = self.f
        f.git('init', '--quiet', at=f.cwd)
        nested = f.cwd / 'nested'
        nested.mkdir()
        for directory, definition in (
            ('remotes', 'URL: ../server.git\nPull: refs/heads/main:refs/heads/main\n'),
            ('branches', '../server.git#main\n'),
        ):
            with self.subTest(directory=directory):
                name = f'local-{directory}'
                remote = f.cwd / '.git' / directory / name
                remote.parent.mkdir(exist_ok=True)
                remote.write_text(definition)
                self.assertEqual(f.git('ls-remote', name, 'HEAD', at=nested).split()[0],
                                 f.commit)
                result = subprocess.run(
                    [OPTIONS.binary, name, 'HEAD', '--backend=copy',
                     '--write-policy=all', '--', 'cat', 'deep/file'],
                    cwd=nested, env=f.env, capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'expected\n')
    def test_local_branch_tag_collision_follows_native_revision_precedence(self):
        f=self.f; source='file://'+str(f.repo)
        other_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        other=f.git('commit-tree',other_tree,data=b'tag collision fixture\n')
        f.git('update-ref','refs/tags/main',other)
        for revision in ('main','heads/main','tags/main'):
            with self.subTest(revision=revision):
                oid=f.git('rev-parse','--verify',revision)
                process=f.probe(source,revision)
                self.assertEqual(f.line(process)['tree'],f.git('rev-parse',oid+'^{tree}'))
                f.finish(process)
    def test_invalid_local_revision_reports_native_git_error(self):
        f=self.f; source='file://'+str(f.repo)
        for revision in ('missing-branch','main:bad..destination'):
            with self.subTest(revision=revision):
                process=f.probe(source,revision)
                _,errors=process.communicate(timeout=20)
                self.assertNotEqual(process.returncode,0)
                self.assertIn(b'Git rev-parse failed',errors)
                self.assertNotIn(b'not advertised',errors)
    @unittest.skipIf(sys.platform == 'darwin', 'requires byte-valued ref filenames')
    def test_local_non_utf8_ref_is_passed_to_git_unchanged(self):
        f=self.f; revision=os.fsdecode(b'branch-\xff')
        f.git('update-ref','refs/heads/'+revision,f.commit)
        process=f.probe('file://'+str(f.repo),revision)
        self.assertEqual(f.line(process)['tree'],f.tree)
        value,_=f.operation(process,'read deep/file')
        self.assertEqual(value['data'],'expected\n'); f.finish(process)
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
                self.assertEqual(run_directories(f.root),[])
    def test_relative_remote_transport_paths_follow_native_worktree(self):
        f = self.f
        f.git('init', '--quiet', at=f.cwd)
        nested = f.cwd / 'nested'
        nested.mkdir()
        https, ssh = f.https(), f.ssh()
        f.git('remote', 'add', 'origin', https, at=f.cwd)
        f.git('config', 'http.sslCAInfo', '../tls.crt', at=f.cwd)
        env = f.env.copy()
        env.pop('GIT_SSL_CAINFO')
        env['GIT_SSH_COMMAND'] = env['GIT_SSH_COMMAND'].replace(str(f.root / 'identity'), '../identity')
        for url in (https, ssh):
            with self.subTest(url=url):
                f.git('remote', 'set-url', 'origin', url, at=f.cwd)
                native = run(['git', 'ls-remote', 'origin', 'HEAD'], cwd=nested, env=env)
                self.assertEqual(native.split()[0].decode(), f.commit)
                result = subprocess.run(
                    [str(OPTIONS.binary), 'origin', 'HEAD', '--backend=copy',
                     '--write-policy=all', '--', 'cat', 'deep/file'],
                    cwd=nested, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'expected\n')
        self.assertFalse(run_directories(f.root))

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
    @unittest.skipIf(sys.platform == 'darwin', 'requires byte-valued ref filenames')
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
        from support.git_invocations import git_invocations, record_git_invocations
        observed,log=record_git_invocations(f.root,original)
        for variable in ('GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES'):
            with self.subTest(variable=variable):
                log.write_text('')
                f.env=dict(observed,**{variable:str(objects)})
                self.assertEqual(f.git('rev-parse','--verify',commit+'^{tree}'),tree)
                process=f.probe('local',commit)
                self.assertEqual(f.line(process)['tree'],tree)
                value,_=f.operation(process,'read file')
                self.assertEqual(value['data'],'external object database\n'); f.finish(process)
                batches=[args[-1] for args in git_invocations(log) if 'cat-file' in args]
                self.assertCountEqual(batches,['--batch','--batch-check'] if 'ALTERNATE' in variable else [])
                if OPTIONS.require_fuse:
                    result=subprocess.run([str(OPTIONS.binary),commit,'--','cat','file'],
                                          cwd=f.cwd,env=f.env,capture_output=True,timeout=20)
                    self.assertEqual(result.returncode,0,result.stderr)
                    self.assertEqual(result.stdout,b'external object database\n')
                    self.assertEqual(run_directories(f.root),[])
    def test_local_namespace_environment_reads_native_repository(self):
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
            self.assertEqual(run_directories(f.root),[])
    def test_local_common_directory_reads_native_repository(self):
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
            child=int(ready.read_text()); runs=run_directories(f.root)
            self.assertEqual(len(runs),1)
            process.kill(); process.wait(timeout=5)
            with contextlib.suppress(ProcessLookupError): os.kill(child,signal.SIGTERM)
            process.communicate(timeout=5)
            cleanup=subprocess.run([str(OPTIONS.binary),'--cleanup',str(runs[0])],
                                   cwd=f.cwd,env=unscoped,capture_output=True,timeout=15)
            self.assertEqual(cleanup.returncode,0,cleanup.stderr)
            self.assertFalse(runs[0].exists())
            for repository in before: self.assertEqual(refs(repository),before[repository])
    @unittest.skipUnless(sys.platform == 'linux', 'byte-valued repository filenames require Linux')
    def test_local_c_quoted_alternate_with_colon_and_non_utf8_path_matches_native_git(self):
        f=self.f; f.cwd=f.repo; objects=f.root/os.fsdecode(b'alternate:objects-\xff'); objects.mkdir()
        nested=f.root/os.fsdecode(b'nested-\xfe'); nested.mkdir()
        (objects/'info').mkdir()
        (objects/'info/alternates').write_bytes(b'../'+os.fsencode(nested.name)+b'\n')
        writer=dict(f.env,GIT_OBJECT_DIRECTORY=str(nested))
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
        self.assertFalse(list(f.root.glob('*/local-object-bridge-*')))
        value,_=f.operation(process,'read file')
        self.assertEqual(value['data'],'quoted alternate\n'); f.finish(process)
        backends = ['copy', 'fuse'] if OPTIONS.require_fuse else ['copy']
        for backend in backends:
            with self.subTest(backend=backend):
                result = subprocess.run(
                    [OPTIONS.binary, commit, '--backend=' + backend, '--write-policy=all',
                     '--', 'cat', 'file'], cwd=f.cwd, env=f.env, capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'quoted alternate\n')
                self.assertFalse(run_directories(f.root))
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
                self.assertEqual(run_directories(f.root),[])
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
    def test_named_remote_rejects_an_alternate_upload_pack_before_connecting(self):
        f = self.f
        url = f.https()
        f.git('init', '--quiet', at=f.cwd)
        f.git('remote', 'add', 'fixture', url, at=f.cwd)
        f.git('config', 'remote.fixture.uploadpack', 'alternate-upload-pack', at=f.cwd)
        process = f.probe('fixture')
        _, errors = process.communicate(timeout=20)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn(b'outside the Git protocol v2 profile', errors)
        self.assertFalse(f.requests)

    def signing(self):
        f=self.f; key=f.root/'signature'
        run(['ssh-keygen','-q','-t','ed25519','-N','','-f',key])
        allowed=f.root/'allowed-signers'
        allowed.write_text('fixture@example.invalid '+(f.root/'signature.pub').read_text())
        f.env.update(GIT_CONFIG_COUNT='2',GIT_CONFIG_KEY_0='gpg.format',GIT_CONFIG_VALUE_0='ssh',GIT_CONFIG_KEY_1='gpg.ssh.allowedSignersFile',GIT_CONFIG_VALUE_1=str(allowed))
        commit=f.git('-c','gpg.format=ssh','-c','user.signingkey='+str(key),'commit-tree','-S',f.tree,data=b'signed fixture\n')
        f.git('-c','gpg.format=ssh','-c','user.signingkey='+str(key),'tag','-s','-m','signed tag','signed',commit)
        return commit,allowed
    def test_native_verification_uses_owned_temporary_files(self):
        f = self.f
        signed, _ = self.signing()
        observed = f.root / "observed-signature"
        verifier = f.root / "observe-verifier"
        verifier.write_text(f"""#!{sys.executable}
import json, os, sys
from pathlib import Path
sys.path.insert(0, {str(Path(__file__).resolve().parent)!r})
from support.run_storage import run_directories
if '-s' in sys.argv:
    signature = Path(sys.argv[sys.argv.index('-s') + 1]).resolve()
    roots = []
    for run in run_directories(Path({str(f.root)!r})):
        roots.append(run.resolve())
        marker = run / '.native-scratch'
        if marker.is_symlink():
            roots.append(marker.resolve())
    assert signature.is_file()
    with open({str(observed)!r}, 'a') as output:
        print(json.dumps([str(signature), any(signature.is_relative_to(root) for root in roots)]),
              file=output)
os.execvp('ssh-keygen', ['ssh-keygen', *sys.argv[1:]])
""")
        verifier.chmod(0o700)
        url = f.https()
        f.git('init', '--quiet', at=f.cwd)
        nested = f.cwd / 'nested'
        nested.mkdir()
        env = dict(f.env, GIT_CONFIG_COUNT='3', GIT_CONFIG_VALUE_1='../allowed-signers',
                   GIT_CONFIG_KEY_2='gpg.ssh.program',
                   GIT_CONFIG_VALUE_2=str(verifier))
        for source in (str(f.repo), url):
            with self.subTest(source=source):
                observed.unlink(missing_ok=True)
                result = subprocess.run(
                    [OPTIONS.binary, source, signed, '--verify', '--backend=copy',
                     '--write-policy=all', '--', 'cat', 'deep/file'],
                    cwd=nested, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'expected\n')
                signatures = list(map(json.loads, observed.read_text().splitlines()))
                self.assertTrue(signatures)
                for file, owned in signatures:
                    self.assertTrue(owned, file)
                    self.assertFalse(Path(file).parent.exists(), file)
                self.assertEqual(run_directories(f.root), [])

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
                self.assertIn(b'select a single remote ref',errors)
                self.assertFalse(any(x['wants'] for x in f.requests[requests:]),'unsupported selector fetched objects')
    def test_ssh_signatures_over_https_ignore_replace_refs_and_reject_unsigned_objects(self):
        f=self.f; signed,allowed=self.signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        replacement_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        replacement=f.git('commit-tree',replacement_tree,data=b'different replacement\n')
        f.git('update-ref','refs/replace/'+signed,replacement)
        for revision in (signed, 'refs/tags/signed'):
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
    @unittest.skipUnless(sys.platform == 'linux', 'Linux PID descriptors')
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
        self.assertEqual(run_directories(f.root),[])
    @unittest.skipUnless(sys.platform == 'linux', 'Linux PID descriptors')
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
                    self.assertEqual(run_directories(f.root),[])
                finally:
                    if not select.select([descriptor],[],[],0)[0]:
                        with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(descriptor,signal.SIGKILL)
                    os.close(descriptor)
                    if process.poll() is None:
                        process.kill(); process.communicate(timeout=5)
    @unittest.skipUnless(sys.platform == 'linux', 'Linux PID descriptors')
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
        self.assertIsNone(ssh_owner_marker(directory))
        self.assertFalse(any(line.split()[4]==str(directory/'mount') for line in Path('/proc/self/mountinfo').read_text().splitlines()))
        with run_lock(directory).open('r+b') as held: fcntl.flock(held,fcntl.LOCK_EX|fcntl.LOCK_NB)
        # This live pidfd identifies only the disposable child captured above.
        os.killpg(native_pid,signal.SIGKILL)
        self.assertTrue(select.select([descriptor],[],[],5)[0])
        process.communicate(timeout=5)
        cleanup=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
        self.assertEqual(cleanup.returncode,0,cleanup.stderr); self.assertFalse(directory.exists())
        print('SIGKILL startup evidence:',json.dumps(dict(native_child_survives=True,mount_present=False,
              ssh_owner_present=False,run_lock_released=True,explicit_recovery_succeeded=True)),flush=True)
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
    def test_ssh_refuses_unavailable_filtering(self):
        f=self.f; url=f.ssh()
        f.git('config','uploadpack.allowFilter','false')
        process=f.probe(url); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'object filtering',errors)
    def test_review_connection_loss_cannot_reconnect_with_explicit_proxy_option(self):
        f=self.f; url=f.ssh()
        f.env['GIT_SSH_COMMAND'] += ' -o ProxyCommand=none'
        process=f.probe(url); f.line(process)
        log=f.root/'sshd.log'
        connections=log.read_bytes().count(b'Connection from ')
        self.assertGreater(connections,0)
        sockets=list(f.root.glob('*/ssh-control')); self.assertEqual(len(sockets),1)
        socket=sockets[0]
        run(['ssh','-F','/dev/null','-S',socket,'-O','exit','git-on-control'],env=f.env)
        deadline=time.monotonic()+10
        while socket.exists() and time.monotonic()<deadline: time.sleep(.01)
        self.assertFalse(socket.exists(),'closed OpenSSH master left its control socket')
        process.stdin.write(b'read deep/file\n'); process.stdin.flush()
        _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0,'read succeeded after the master closed')
        self.assertEqual(log.read_bytes().count(b'Connection from '),connections,errors)
    def test_real_mount_remote_disposable_cleanup_and_cancellation(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        env=git_on_environment(OPTIONS.binary,f.env,f.root/'selected-tools')
        ca=env.pop('GIT_SSL_CAINFO')
        result=subprocess.run(['git','-c','http.sslCAInfo='+ca,'on',url,'main','--','sh','-c','cat deep/file; mkdir out; printf kept > out/result'],cwd=f.cwd,env=env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr); self.assertEqual(result.stdout,b'expected\n')
        self.assertEqual(run_directories(f.root),[])
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--','sleep','30'],cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+10
        while time.monotonic()<deadline and not any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()): time.sleep(.02)
        self.assertTrue(any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()),'remote run never mounted')
        process.send_signal(signal.SIGTERM); process.communicate(timeout=10)
        self.assertEqual(process.returncode,-signal.SIGTERM)
        self.assertEqual(run_directories(f.root),[])

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
        writes=None
        for offset in (0,len(payload)-4096,512,512):
            value,report=f.operation(process,f'range {offset} 4096 large')
            self.assertEqual(bytes(value['bytes']),payload[offset:offset+4096])
            self.assertEqual(report['metrics']['blob_loads'],1)
            self.assertEqual(report['metrics']['spool_bytes'],0)
            if writes is None:writes=report['metrics']['spool_writes']
            self.assertEqual(report['metrics']['spool_writes'],writes)
            self.assertNotIn(f.other,report['objects'])
        self.assertEqual(len(f.requests),request_count)
        _,report=f.operation(process,'release'); self.assertEqual(report['metrics']['active_spool_bytes'],0)
        f.finish(process)

    def test_authentication_and_exact_oid_tree_blob_refusals(self):
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        f.env['GIT_TERMINAL_PROMPT']='0'; f.env['GIT_ASKPASS']='/bin/false'
        f.deny_auth=True
        process=f.probe(url); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'Git HTTPS authentication failed',errors)
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

    def test_eager_ssh_verifies_the_fixed_tag_object_before_materializing(self):
        f = self.f
        _, allowed = self.signing()
        ref = "refs/tags/signed"
        tag = f.git("rev-parse", ref)
        tree = f.git("mktree", data=f"100644 blob {f.other}\tother\n".encode())
        replacement = f.git("commit-tree", tree, data=b"moved after verification\n")
        source = f.ssh()
        wrapper_dir = f.root / "git-wrapper"
        wrapper_dir.mkdir()
        wrapper = wrapper_dir / "git"
        log = f.root / "verified-tag"
        native = shutil.which("git", path=f.env["PATH"])
        wrapper.write_text(
            f"""#!{sys.executable}
import os, subprocess, sys
args = sys.argv[1:]
if 'verify-tag' in args:
    status = subprocess.call([{native!r}, *args])
    if status != 0:
        sys.exit(status)
    with open({str(log)!r}, 'w') as output:
        output.write(args[-1])
    sys.exit(subprocess.call([{native!r}, '--git-dir={f.repo}', 'update-ref', {ref!r}, {replacement!r}]))
os.execv({native!r}, [{native!r}, *args])
"""
        )
        wrapper.chmod(0o700)
        env = dict(f.env, PATH=str(wrapper_dir) + os.pathsep + f.env["PATH"])
        args = [OPTIONS.binary, source, ref, "--backend=copy", "--write-policy=all",
                "--verify", "--", "sh", "-c", 'printf "%s\\n" "$GIT_ON_TREE_OID"; cat deep/file']
        result = subprocess.run(args, cwd=f.cwd, env=env, capture_output=True, timeout=25)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(log.read_text(), tag)
        self.assertEqual(f.git("rev-parse", ref), replacement)
        self.assertEqual(result.stdout, f"{f.tree}\nexpected\n".encode())
        f.git("update-ref", ref, tag)
        allowed.unlink()
        result = subprocess.run(args, cwd=f.cwd, env=env, capture_output=True, timeout=25)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertEqual(result.stdout, b"")

    def test_ssh_leaves_known_hosts_and_local_command_state_unchanged(self):
        f = self.f
        source = f.ssh()
        known = f.root / "known_hosts"
        original = known.read_bytes()
        marker = f.root / "local-command"
        config = f.root / "client-config"
        config.write_text(f"Host *\n  PermitLocalCommand yes\n  LocalCommand /usr/bin/touch {marker}\n")
        f.env["GIT_SSH_COMMAND"] = (f.env["GIT_SSH_COMMAND"]
                                     .replace("-F /dev/null", "-F " + shlex.quote(str(config)))
                                     .replace("StrictHostKeyChecking=yes", "StrictHostKeyChecking=accept-new"))
        args = [OPTIONS.binary, source, "HEAD", "--backend=copy", "--write-policy=all",
                "--", "/bin/cat", "deep/file"]
        known.unlink()
        result = subprocess.run(args, cwd=f.cwd, env=f.env, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertFalse(known.exists())
        known.write_bytes(original)
        result = subprocess.run(args, cwd=f.cwd, env=f.env, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"expected\n")
        self.assertEqual(known.read_bytes(), original)
        self.assertFalse(marker.exists())
        with config.open("a") as settings:
            settings.write("  CheckHostIP yes\n")
        result = subprocess.run(args, cwd=f.cwd, env=f.env, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b"CheckHostIP", result.stderr)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(known.read_bytes(), original)

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
                        self.assertEqual(run_directories(f.root),[])
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
                    self.assertEqual(run_directories(f.root),[])
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
        self.assertEqual(run_directories(f.root),[])

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
        # Each blob fits the whole-object cache, so reopening requires no
        # additional requests or service writes.
        f=self.f; ids=[]; object_mib=1
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
        self.assertEqual(last['metrics']['spool_bytes'],0)
        writes=last['metrics']['spool_writes']
        requests=len(f.requests)
        value,last=f.operation(process,'range 13 16 a');self.assertEqual(bytes(value['bytes']),b'a'*16)
        self.assertEqual(len(f.requests),requests,'available run object fetched again')
        self.assertEqual(last['metrics']['spool_writes'],writes,'closed blob was written again')
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

    @unittest.skipIf(OPTIONS.require_fuse, 'copy and NFS fallback run in their own jobs')
    def test_materialized_remote_fetches_the_selected_tree_once(self):
        f=self.f;entries=[]
        for index in range(2):
            blob=f.git('hash-object','-w','--stdin',data=f'file {index}\n'.encode())
            tree=f.git('mktree',data=f'100644 blob {blob}\tfile\n'.encode())
            entries.append(f'040000 tree {tree}\td{index}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        ancestor=f.commit
        commit=f.git('commit-tree',tree,'-p',ancestor,data=b'materialization transfer regression\n')
        f.git('update-ref','refs/heads/main',commit)
        url=f.https();self.addCleanup(f.server.shutdown)
        code=("import os; from pathlib import Path; "
              f"assert os.environ['GIT_ON_TREE_OID']=={tree!r}; "
              "assert all(Path(f'd{i}/file').read_text()==f'file {i}\\n' for i in range(2)); "
              "print('verified')")
        if OPTIONS.require_nfs:
            ssh = f.ssh()
            target = urlsplit(ssh)
            scp = f"{target.username}@{target.hostname}:{target.path}"
            cases = [('https', url, 'auto'), ('ssh', ssh, 'auto'), ('scp', scp, 'auto')]
        else:
            cases = [('https', url, 'copy')]
        for transport, source, backend in cases:
            with self.subTest(transport=transport, backend=backend):
                # Git syntax selects the transport before any local path check.
                lookalike = f.cwd / source
                lookalike.parent.mkdir(parents=True, exist_ok=True)
                lookalike.symlink_to(f.repo, target_is_directory=True)
                f.requests.clear()
                env = dict(f.env)
                if backend == 'auto':
                    env['GIT_ON_TEST_NFS_MOUNT_ERROR'] = '1'
                if transport in ('ssh', 'scp'):
                    log = f.root / 'sshd.log'
                    connections = log.read_text().count('Accepted publickey')
                    env['GIT_SSH_COMMAND'] += ' -J none'
                    if transport == 'scp':
                        env['GIT_SSH_COMMAND'] += f' -p {target.port}'
                    root = f.root.resolve()
                    tmpdir = root / ('s' * (58 - len(os.fsencode(root))))
                    tmpdir.mkdir(exist_ok=True)
                    self.assertEqual(len(os.fsencode(tmpdir)), 59)
                    env['TMPDIR'] = str(tmpdir)
                else:
                    env.update(GIT_CONFIG_COUNT='6', GIT_CONFIG_KEY_0='http.version',
                               GIT_CONFIG_VALUE_0='HTTP/2',
                               GIT_CONFIG_KEY_1='http.proxy', GIT_CONFIG_VALUE_1='',
                               GIT_CONFIG_KEY_2='http.https://elsewhere.invalid/.version',
                               GIT_CONFIG_VALUE_2='HTTP/1.1',
                               GIT_CONFIG_KEY_3='remote.unused.uploadpack',
                               GIT_CONFIG_VALUE_3='unused-command',
                               GIT_CONFIG_KEY_4='protocol.https.allow',
                               GIT_CONFIG_VALUE_4='always',
                               GIT_CONFIG_KEY_5='http.sslVerify', GIT_CONFIG_VALUE_5='true')
                result = subprocess.run([str(OPTIONS.binary), source, 'HEAD', '--write-policy=all',
                                         '--backend='+backend, '--', sys.executable, '-c', code],
                                        cwd=f.cwd, env=env, capture_output=True, timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,b'verified\n')
                if transport == 'https':
                    wants = [request['wants'] for request in f.requests if request['wants']]
                    expected = [[commit], [tree], [commit]] if backend == 'auto' else [[commit]]
                    self.assertEqual(wants, expected,
                                     'eager preparation performs separate directory/object fetches')
                    self.assertTrue(f.requests[-1]['shallow'], 'copy fetch did not ask for depth one')
                else:
                    self.assertEqual(log.read_text().count('Accepted publickey'), connections + 1)
                    if transport == 'scp':
                        local = subprocess.run([str(OPTIONS.binary), './' + source, 'HEAD',
                                                '--backend=copy', '--write-policy=all', '--',
                                                sys.executable, '-c', code],
                                               cwd=f.cwd, env=env, capture_output=True, timeout=30)
                        self.assertEqual(local.returncode, 0, local.stderr)
                        self.assertEqual(local.stdout, result.stdout)
                        self.assertEqual(log.read_text().count('Accepted publickey'), connections + 1)
                self.assertEqual(run_directories(Path(env['TMPDIR'])), [])

    @unittest.skipIf(OPTIONS.require_fuse, 'eager preparation runs in the copy suite')
    def test_eager_copy_keeps_large_blobs_in_the_shared_cache(self):
        f = self.f
        size = 64 * 1024 + 1
        objects = [f.git('hash-object', '-w', '--stdin', data=byte * size) for byte in (b'a', b'b')]
        tree = f.git('mktree', data=''.join(
            f'100644 blob {oid}\tfile-{index}\n' for index, oid in enumerate(objects)
        ).encode())
        commit = f.git('commit-tree', tree, data=b'eager large-blob policy\n')
        f.git('update-ref', 'refs/heads/main', commit)
        probe = f.probe(f.https(), commit, eager=True)
        metrics = f.line(probe)['metrics']
        f.finish(probe)
        self.assertEqual(metrics['spool_bytes'], 0, metrics)
        self.assertEqual(metrics['large_blob_cache_entries'], 2, metrics)

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
        self.assertEqual(report['remote_http2_fetch_operations'], 5, report)
        self.assertEqual(report['remote_session_starts'], 1, report)
        self.assertEqual(report['remote_fetch_operations'], 0, report)
        self.assertEqual({oid for request in f.requests for oid in request['wants']},
                         {f.commit, f.tree, f.subtree, f.blob, f.other})
        self.assertFalse(run_directories(f.root))

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
        first_stall = f.stalled_requests.get(timeout=12)
        gate.touch()
        second_stall = f.stalled_requests.get(timeout=8)
        # The scheduler can start the speculative stream first. Both distinct
        # streams must still be stalled before the runner is cancelled.
        self.assertCountEqual([first_stall, second_stall], [[f.blob], [f.other]])
        process.send_signal(signal.SIGTERM)
        _, errors = process.communicate(timeout=12)
        self.assertEqual(process.returncode, -signal.SIGTERM, errors)
        self.assertNotIn(b'execution incomplete', errors)
        self.assertFalse(run_directories(f.root))

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
        server = f.https_server()
        forward = server.forward

        def delayed(method, path, headers, body):
            if b'want ' in body:
                time.sleep(.08)
            return forward(method, path, headers, body)

        server.forward = delayed
        url = server.url
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
        self.assertFalse(run_directories(f.root))

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
                self.assertFalse(run_directories(f.root))

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
                    if phase == 'local-resolution':
                        deadline = time.monotonic() + 5
                        while not entered.exists() and process.poll() is None and time.monotonic() < deadline:
                            time.sleep(.01)
                        self.assertTrue(entered.exists(), 'first Git invocation did not start')
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
                        self.assertIn(b'git on HEAD',visible)
                    else:
                        self.assertIn(b':/progress-\\u{1b}',visible,'revision is missing or its controls are not escaped')
                        frames=visible.split(b'COMMAND-START')[0].decode('utf-8').split('\r')
                        self.assertLessEqual(max(sum(2 if unicodedata.east_asian_width(c) in ('W','F') else 1 for c in frame) for frame in frames),48)
                    self.assertLess(transcript.rfind(b'\x1b[2K'),transcript.index(b'COMMAND-START'))
                    self.assertNotIn(b'git on',transcript[transcript.index(b'COMMAND-START'):])
                else:self.assertEqual(errors,b'COMMAND-START\n')
                self.assertEqual(run_directories(f.root),[])

    @unittest.skipUnless(sys.platform=='linux','controlling Bash job and /proc state observation')
    def test_preparation_progress_during_https_transfer_with_tostop(self):
        import native_job_control
        f=self.f;url=f.https();self.addCleanup(f.server.shutdown)
        tools=f.root/'tostop-tools';tools.mkdir();(tools/'git').symlink_to(shutil.which('git'))
        shell=native_job_control.Shell(f);self.addCleanup(shell.close)
        shell.send(b'stty tostop\n');shell.expect(shell.prompt)
        f.resume.clear();f.stalled.clear();f.stalled_oids={f.commit}
        runner_file=f.root/'tostop-runner'
        argv=['/bin/sh','-c','printf "%s" "$$" > "$1"; shift; exec "$@"','fixture',str(runner_file),str(OPTIONS.binary),url,'HEAD','--write-policy=all','--',sys.executable,'-c',"import sys,termios;assert termios.tcgetattr(0)[3]&termios.ECHO;assert sys.stdin.readline()=='typed-ahead\\n';print('child-completed')"]
        shell.send((f'PATH={shlex.quote(str(tools))} TERM=xterm '+shlex.join(argv)+'; printf "STATUS:%d\\n" "$?"\n').encode())
        try:
            self.assertTrue(f.stalled.wait(10),'HTTPS transfer did not reach controlled TTY stall')
            runner=int(runner_file.read_text());foreground=os.tcgetpgrp(shell.fd)
            shell.groups.update((runner,foreground))
            self.assertEqual(foreground,runner,'HTTPS helper took the terminal from git-on')
            self.assertEqual(os.getsid(runner),shell.pid)
            shell.send(b'typed-')
            deadline=time.monotonic()+1.3
            while time.monotonic()<deadline:
                shell.read()
                if native_job_control.state(runner)=='T':break
            self.assertNotEqual(native_job_control.state(runner),'T',f'runner stopped during an HTTPS transfer: {bytes(shell.transcript)!r}')
            self.assertGreaterEqual(shell.transcript.count(b'git on'),2,'HTTPS transfer hid the animation')
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
            self.assertEqual(run_directories(f.root),[])
            shell.groups.clear()
        finally:f.resume.set()
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
                    if outcome=='error': self.assertIn(b'403',transcript)
                self.assertEqual(run_directories(f.root),[])

    @unittest.skipUnless(sys.platform=='linux' and shutil.which('strace'), 'Linux strace object-I/O observation')
    def test_private_remote_objects_do_not_sync_disposable_payloads(self):
        f=self.f;trace=f.root/'private-object-io.trace'
        # Durability for the caller's repository does not apply to streamed
        # objects or change its Git configuration.
        env=dict(f.env,GIT_CONFIG_COUNT='2',GIT_CONFIG_KEY_0='core.fsync',GIT_CONFIG_VALUE_0='all',
            GIT_CONFIG_KEY_1='core.fsyncObjectFiles',GIT_CONFIG_VALUE_1='true')
        before=f.git('config','--list')
        source=f.https()
        env['GIT_SSL_CAINFO']=f.env['GIT_SSL_CAINFO']
        result=subprocess.run(['strace','-f','-yy','-e','trace=fsync,fdatasync',
            '-o',str(trace),str(OPTIONS.probe),source,'HEAD','--metrics-only'],
            cwd=f.cwd,env=env,input=b'read deep/file\nread deep/file\nreport\nexit\n',capture_output=True,timeout=40)
        self.assertEqual(result.returncode,0,result.stderr)
        rows=[json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([row['data'] for row in rows if 'data' in row],['expected\n']*2)
        reports=[row['metrics'] for row in rows if 'metrics' in row]
        self.assertEqual(reports[-1]['remote_fetches'],4)
        self.assertEqual(reports[-1]['remote_fetches'],reports[-2]['remote_fetches'])
        self.assertNotRegex(trace.read_text(), r'\b(?:fsync|fdatasync)\(')
        self.assertEqual(f.git('config','--list'),before)

    def test_remote_raw_import_reuses_evicted_objects_without_loose_blob_copies(self):
        f=self.f;entries=[];oids=[];count=300
        for index in range(count):
            prefix=f'raw-{index:03d}:'.encode();data=prefix+b'x'*(65536-len(prefix))
            oid=f.git('hash-object','-w','--stdin',data=data);oids.append(oid)
            entries.append(f'100644 blob {oid}\tf{index:03d}\n')
        tree=f.git('mktree',data=''.join(entries).encode())
        commit=f.git('commit-tree',tree,data=b'raw eviction fixture\n');f.git('update-ref','refs/heads/main',commit)
        process=f.probe(f.https());f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'range 0 32 f{index:03d}')
            self.assertEqual(bytes(value['bytes']),f'raw-{index:03d}:'.encode()+b'x'*24)
        value,again=f.operation(process,'range 0 32 f000');self.assertEqual(bytes(value['bytes']),b'raw-000:'+b'x'*24)
        self.assertEqual(again['metrics']['remote_fetches'],count+2)
        self.assertEqual(again['metrics']['blob_loads'],last['metrics']['blob_loads'],'evicted raw blob was unpacked again')
        self.assertFalse(list(f.root.rglob('remote-repository')))
        self.assertGreater(again['metrics']['retained_raw_blob_entries'],0,'working set did not actually evict payloads')
        self.assertLessEqual(again['metrics']['blob_cache_bytes'],again['metrics']['blob_cache_limit'])
        f.finish(process)

    def test_protocol_v2_reuses_native_clients_across_many_blob_demands(self):
        f=self.f;count=140;entries=[];oids=[]
        for index in range(count):
            oid=f.git('hash-object','-w','--stdin',data=f'unique-{index}\n'.encode());oids.append(oid)
            entries.append(f'100644 blob {oid}\tf{index:04d}\n')
        tree=f.git('mktree',data=''.join(entries).encode());commit=f.git('commit-tree',tree,data=b'session reuse\n');f.git('update-ref','refs/heads/main',commit)
        source=f.ssh()
        from support.git_invocations import git_invocations, record_git_invocations
        f.env, trace = record_git_invocations(f.root, f.env)
        process=f.probe(source);f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'read f{index:04d}')
            self.assertEqual(value['data'],f'unique-{index}\n')
        value,again=f.operation(process,'read f0000');self.assertEqual(value['data'],'unique-0\n')
        self.assertEqual(again['metrics']['blob_loads'],count)
        self.assertEqual(again['metrics']['remote_fetches'],count+2)
        again=f.wait_for_packs(process,count+2,'remote_session_decode_operations')
        f.finish(process)
        self.assertFalse(any('fetch' in args or 'cat-file' in args for args in git_invocations(trace)))
        self.assertEqual(again['metrics']['remote_session_fetch_operations'], count + 2)
        self.assertEqual(again['metrics']['remote_session_decode_operations'], count + 2)
        self.assertEqual((f.root/'sshd.log').read_text().count('Accepted publickey'),1)
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
        source=f.https()
        process=subprocess.Popen([str(OPTIONS.probe),source,'HEAD','--metrics-only'],cwd=f.cwd,
            env=f.env,preexec_fn=reduced_limits,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        f.children.append(process);f.line(process)
        for index in range(count):
            value,last=f.operation(process,f'range 0 32 f{index:04d}')
            self.assertEqual(value['bytes'],list(hashlib.shake_256(f'session-parts:{index}'.encode()).digest(32)))
            f.operation(process,'release')
        value,again=f.operation(process,'range 0 32 f0000')
        self.assertEqual(value['bytes'],list(hashlib.shake_256(b'session-parts:0').digest(32)))
        self.assertEqual(again['metrics']['blob_loads'],count)
        again=f.wait_for_packs(process,count+2,'remote_http2_fetch_operations')
        self.assertEqual(again['metrics']['remote_http2_fetch_operations'],count+2)
        self.assertEqual(again['metrics']['remote_session_decode_operations'],count+2)
        self.assertEqual(again['metrics']['remote_session_starts'],1)
        self.assertEqual(again['metrics']['remote_raw_blob_bytes'],count*limit)
        self.assertGreater(again['metrics']['remote_raw_blob_bytes'],limit)
        self.assertEqual(again['metrics']['spool_bytes'], 0)
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
        value,_=f.operation(process,'read deep/file');self.assertEqual(value['data'],'expected\n')
        report=f.wait_for_packs(process,4,'remote_session_fetch_operations')
        self.assertEqual(report['metrics']['remote_session_fetch_operations'],4)
        socket=next(f.root.rglob('ssh-control'))
        run(['ssh','-F','/dev/null','-S',socket,'-O','exit','fixture-host'],env=f.env)
        with self.assertRaises(AssertionError):f.operation(process,'read other')
        self.assertNotEqual(process.wait(timeout=10),0)
        self.assertEqual((f.root/'sshd.log').read_text().count('Accepted publickey'),1)

    def delta_session_fixture(self, prefix_size=20000, include_base=False, root_extras=None,
                              packet_size=60000, payload_prefix=None, hold_tail=False):
        import hashlib,zlib
        f=self.f;native=shutil.which('git')
        prefix = b'a' * prefix_size if payload_prefix is None else payload_prefix
        prefix_size = len(prefix)
        base=prefix+b'base';wanted=prefix+b'changed'
        base_oid=f.git('hash-object','-w','--stdin',data=base)
        wanted_oid=f.git('hash-object','-w','--stdin',data=wanted)
        entries=f'100644 blob {wanted_oid}\tfile\n'
        if include_base:entries+=f'100644 blob {base_oid}\tbase\n'
        for name, content in (root_extras or {}).items():
            oid=f.git('hash-object','-w','--stdin',data=content)
            entries+=f'100644 blob {oid}\t{name}\n'
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
                      input=f'{tree}\n'.encode(),env=f.env)
        trace = f.root / 'delta-demands'
        server = f.https_server()
        forward = server.forward
        packs = {tree.encode(): tree_pack, wanted_oid.encode(): data}

        def framed(pack):
            reply = bytearray(b'000dpackfile\n')
            for start in range(0, len(pack), packet_size):
                chunk = pack[start:start + packet_size]
                reply.extend(f'{len(chunk)+5:04x}'.encode() + b'\x01' + chunk)
            return bytes(reply)

        if hold_tail:
            from types import SimpleNamespace
            server.split_pack = SimpleNamespace(
                wants={wanted_oid.encode()},
                prefix=framed(data[:-20]),
                tail=b'0019\x01' + data[-20:] + b'0000',
                started=f.root / 'delta-pack-started',
                release=f.root / 'delta-pack-release',
            )

        def send_pack(method, path, headers, body):
            wants = re.findall(rb'want ([0-9a-f]{40})', body)
            if len(wants) != 1 or wants[0] not in packs:
                return forward(method, path, headers, body)
            [oid] = wants
            with trace.open('ab') as output:
                output.write(oid + b'\n')
            return 200, [('content-type', 'application/x-git-upload-pack-result')], framed(packs[oid]) + b'0000'

        server.forward = send_pack
        return wanted, base_oid, wanted_oid, tree, commit, trace, server

    def test_session_imports_required_delta_pack_without_a_second_fetch(self):
        f = self.f
        extras = {f'extra{index}': bytes([index]) * 70000 for index in range(7)}
        wanted, _, wanted_oid, tree, _, trace, server = self.delta_session_fixture(70000, root_extras=extras)
        process=f.probe(server.url);f.line(process)
        value,first=f.operation(process,'range 69990 100 file')
        self.assertEqual(bytes(value['bytes']),wanted[69990:])
        f.operation(process,'release')
        value,again=f.operation(process,'range 69990 100 file')
        self.assertEqual(bytes(value['bytes']),wanted[69990:])
        self.assertEqual(first['metrics']['blob_loads'], again['metrics']['blob_loads'])
        self.assertEqual(trace.read_text().splitlines(), [tree, wanted_oid])
        f.operation(process,'release')
        for name, expected in extras.items():
            value, _ = f.operation(process, f'range 0 1 {name}')
            self.assertEqual(bytes(value['bytes']), expected[:1])
            f.operation(process,'release')
        report = f.wait_for_packs(process, 10, 'remote_http2_fetch_operations')
        self.assertEqual(report['metrics']['large_blob_cache_entries'], 8)
        self.assertEqual(report['metrics']['spool_bytes'], 0, 'a delta-only base displaced a wanted blob')
        self.assertFalse(list(f.root.rglob('remote-repository')))
        self.assertFalse(list(f.root.rglob('raw-parts-*')))
        f.finish(process)

    def test_delta_base_requested_later_uses_its_received_pack_without_refetch(self):
        f = self.f
        _, _, wanted, tree, commit, _, server = self.delta_session_fixture(include_base=True, packet_size=31)
        process = f.probe(server.url)
        f.line(process)
        f.operation(process, 'range 0 1 file')
        f.operation(process, 'release')
        f.wait_for_packs(process, 3, 'remote_http2_fetch_operations')
        value, _ = f.operation(process, 'range 19990 100 base')
        self.assertEqual(bytes(value['bytes']), b'a' * 10 + b'base')
        f.finish(process)
        self.assertCountEqual(
            [oid.decode() for _, wants in server.records for oid in wants],
            [commit, tree, wanted],
        )

    def test_real_mount_session_delta_reuses_fitting_payloads_in_ram(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        f=self.f;wanted,_,wanted_oid,tree,_,trace,server=self.delta_session_fixture(40000)
        code=r'''
import errno,os,sys
from pathlib import Path
expected=b'a'*40000+b'changed'
for _ in range(2):
 assert Path('file').read_bytes()==expected
runs=Path.cwd().parent
raw=list(runs.glob('raw-objects-*'))
assert len(raw)==1 and not list(raw[0].iterdir()), 'fitting delta payloads spilled to disk'
assert not (runs/'remote-repository').exists()
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
                result=subprocess.run([str(OPTIONS.binary),server.url,'HEAD',
                    '--write-policy='+policy,'--',sys.executable,'-I','-S','-c',code,policy],
                    cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(trace.read_text().splitlines(),[tree,wanted_oid])
                self.assertEqual(run_directories(f.root), [])
                self.assertEqual(f.git('cat-file','blob',wanted_oid).encode(),wanted)

    def large_delta_storage_fixture(self):
        size=7*1024*1024
        _,_,wanted_oid,tree,_,trace,server=self.delta_session_fixture(size-7)
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
        return command,wanted_oid,tree,trace,server.url

    def test_delta_base_is_not_a_second_materialized_file(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        f=self.f;command,wanted_oid,tree,trace,url=self.large_delta_storage_fixture()
        clone=f.root/'delta-clone'
        run(['git','clone','--no-local','--quiet',f.repo,str(clone)],env=f.env)
        for mode,policy in (('read','none'),('rewrite','all')):
            with self.subTest(mode=mode):
                trace.unlink(missing_ok=True)
                direct=subprocess.run([*command,mode],cwd=clone,env=f.env,capture_output=True,timeout=30)
                self.assertEqual(direct.returncode,0,direct.stderr)
                result=subprocess.run([str(OPTIONS.binary),url,'HEAD','--write-policy='+policy,'--',*command,mode],
                                      cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,direct.stdout)
                self.assertEqual(trace.read_text().splitlines(),[tree,wanted_oid])
                self.assertEqual(run_directories(f.root), [])

    def test_delta_decode_and_rewrite_succeed_on_nine_mib_tmpfs(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        from filesystem_quota import compare_on_tmpfs
        f=self.f;command,_,_,_,url=self.large_delta_storage_fixture()
        compare_on_tmpfs(self,f.root,OPTIONS.binary,f.repo,f.env['PATH'],[*command,'rewrite'],source=url,tls_ca=f.env['GIT_SSL_CAINFO'])



    def test_spilled_delta_copy_up_before_the_trailer_fits_two_checkouts(self):
        if not OPTIONS.require_fuse:self.skipTest('real FUSE explicitly required in CI')
        from filesystem_quota import compare_on_tmpfs
        from random import Random
        f = self.f
        pressure = {f'pressure-{index}': bytes([index]) * (65536 + 1) for index in range(8)}
        prefix = Random(18).randbytes(7 * 1024 * 1024 - 7)
        _, _, wanted_oid, _, _, _, server = self.delta_session_fixture(
            root_extras=pressure, payload_prefix=prefix, hold_tail=True,
        )
        release = f.root / 'delta-pack-release'
        code = r'''
import sys
from pathlib import Path
from random import Random
entry = Path('file')
release = Path(sys.argv[1])
mounted = not Path('.git').exists()
try:
    expected = Random(18).randbytes(7 * 1024 * 1024 - 7) + b'changed'
    assert entry.read_bytes() == expected
    for index in range(8):
        with open(f'pressure-{index}', 'rb', buffering=0) as pressure:
            assert pressure.read(1) == bytes([index])
    if mounted:
        (spill,) = Path.cwd().parent.glob('raw-objects-*/' + sys.argv[2])
        assert spill.stat().st_size == len(expected)
        assert not release.exists()
    with entry.open('r+b', buffering=0) as output:
        assert output.write(b'z') == 1
    assert entry.read_bytes() == b'z' + expected[1:]
    print('spilled delta copy-up verified')
finally:
    if mounted:
        release.touch()
'''
        command = [sys.executable, '-I', '-S', '-c', code, str(release), wanted_oid]
        compare_on_tmpfs(self, f.root, OPTIONS.binary, f.repo, f.env['PATH'], command,
                         size_mib=16, source=server.url, tls_ca=f.env['GIT_SSL_CAINFO'])
        self.assertTrue((f.root / 'delta-pack-started').exists())
        self.assertTrue(release.exists())

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
        source = f.https()
        limit=64
        def reduced_limit(): resource.setrlimit(resource.RLIMIT_NOFILE,(limit,limit))
        def exercise(binary,cache,expect_failure=False):
            process=subprocess.Popen([str(binary), 'file://' + str(f.repo) if expect_failure else source, 'HEAD'],cwd=f.cwd,
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
                            self.assertFalse(list(f.root.rglob('remote-repository')))
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

    @unittest.skipUnless(sys.platform=='linux', 'Linux /proc peak resident-memory observation')
    def test_first_large_loose_range_reuses_the_hot_object(self):
        import hashlib
        f = self.f
        size = 4 * 1024 * 1024
        data = hashlib.shake_256(b"hot-loose-object").digest(size)
        blob = f.git("hash-object", "-w", "--stdin", data=data)
        tree = f.git("mktree", data=f"100644 blob {blob}\tlarge\n".encode())
        f.cwd = f.repo
        process = f.probe("local", tree)
        f.line(process)
        for offset in (1234, size - 16, 1234):
            value, report = f.operation(process, f"range {offset} 16 large")
            self.assertEqual(bytes(value["bytes"]), data[offset:offset+16])
        self.assertEqual(report["metrics"]["streamed_blob_loads"], 1)
        self.assertEqual(report["metrics"]["large_blob_cache_bytes"], size)
        self.assertEqual(report["metrics"]["spool_bytes"], 0)
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
        self.assertRegex(errors, rb'authentication failed|Permission denied|requires filtered Git protocol')
        self.assertFalse((f.root/'prompts').exists())

    def test_pty_selected_encrypted_identity_prompts_once(self):
        f=self.f; url=f.ssh(encrypted=True)
        env=dict(f.env)
        for key in ('SSH_ASKPASS','SSH_ASKPASS_REQUIRE','DISPLAY'): env.pop(key,None)
        backend = ('--backend=nfs' if OPTIONS.require_nfs else
                   '--backend=fuse' if OPTIONS.require_fuse else '--backend=copy')
        pid,fd=pty.fork()
        if pid==0:
            exec_fixture_child([str(OPTIONS.binary), url, 'HEAD', backend, '--write-policy=all',
                                '--', '/bin/cat', 'deep/file', 'other'], f.cwd, env)
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
            await_bytes(b'expected')
            await_bytes(b'unrelated payload')
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
        self.assertIsNotNone(ssh_owner_marker(directory))
        def cleanup():
            with contextlib.suppress(ProcessLookupError): os.killpg(state['pid'],signal.SIGKILL)
            if process.poll() is None: process.kill()
            process.communicate(timeout=10)
            if directory.exists(): subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.addCleanup(cleanup)
        active=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
        self.assertNotEqual(active.returncode,0); self.assertTrue((directory/'backing').exists())
        recovery_id=run_id(directory)
        mountpoint=directory/'mount'
        def mounted():
            return any(line.split()[4]==str(mountpoint) and 'git-on:'+recovery_id in line
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
        self.assertIsNotNone(ssh_owner_marker(directory))
        self.assertTrue(any(path.is_file() and path.read_bytes()==b'recover'
                            for path in (directory/'backing').iterdir()),'raw writable payload was lost')
        # Kernel flock ownership ends with the killed process.
        with run_lock(directory).open('r+b') as held:
            fcntl.flock(held,fcntl.LOCK_EX|fcntl.LOCK_NB)
        print('SIGKILL runtime evidence:',json.dumps(dict(mount_remaining=mounted(),
              mount_errno=errno.ENOTCONN,child_alive=True,ssh_master_alive=True,
              run_retained=True,locks_released=True)),flush=True)
        os.killpg(state['pid'],signal.SIGKILL); process.communicate(timeout=10)
        stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.assertEqual(stale.returncode,0,stale.stderr); self.assertFalse(directory.exists())
        self.assertFalse(mounted())
        self.assertNotEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertTrue(select.select([master_fd],[],[],5)[0],'recorded owned master survived stale recovery')
        self.assertEqual(run_directories(f.root),[])

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
        recovery_id=run_id(directory)
        def mounted():
            return any(line.split()[4]==str(mountpoint) and 'git-on:'+recovery_id in line
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

    def test_review_real_mount_failed_ssh_shutdown_retains_recovery(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        if OPTIONS.before_lifecycle_binary:
            with self.assertRaisesRegex(AssertionError,'0 != 125'):
                self.failed_ssh_shutdown_retains_recovery(OPTIONS.before_lifecycle_binary)
            print('RED-BEFORE mounted SSH shutdown: old runner returned0 and removed recovery for a live master',flush=True)
        self.failed_ssh_shutdown_retains_recovery(OPTIONS.binary)

    def failed_ssh_shutdown_retains_recovery(self,binary):
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
        self.assertIsNotNone(ssh_owner_marker(directory))
        self.assertFalse(select.select([master_fd],[],[],0)[0])
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
            self.assertIsNotNone(run_lock(directory))
            self.assertIn(str(directory/'mount'),[line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()])
            refused=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
            self.assertNotEqual(refused.returncode,0)
        finally:
            if ready.exists():
                with contextlib.suppress(ProcessLookupError): os.killpg(int(ready.read_text()),signal.SIGKILL)
            if directory is not None:
                stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
                self.assertEqual(stale.returncode,0,stale.stderr)
        self.assertEqual(run_directories(f.root),[])

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

    @unittest.skipUnless(OPTIONS.require_fuse, 'real FUSE explicitly required in CI')
    def test_handled_term_during_active_remote_read_preserves_child_exit(self):
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

    def test_review_real_mount_stalled_lazy_fetch_shutdown(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; self.stall_url=f.https(); self.addCleanup(f.server.shutdown)
        for escaped in (False,True):
            with self.subTest(escaped=escaped):
                if OPTIONS.before_binary and escaped: self.stalled_mount_shutdown(OPTIONS.before_binary,escaped,True)
                self.stalled_mount_shutdown(OPTIONS.binary,escaped,False)

    @unittest.skipUnless(OPTIONS.require_fuse or OPTIONS.require_nfs, 'requires a native mount')
    def test_real_mount_caught_spilled_blob_failure_is_sticky(self):
        f = self.f
        oids = [f.git("hash-object", "-w", "--stdin", data=bytes([index]) * 65537)
                for index in range(9)]
        # Keep the target and each pressure blob in a separate tree.
        subtrees = [f.git("mktree", data=f"100644 blob {oid}\tfile\n".encode())
                    for oid in oids]
        rows = "".join(f"040000 tree {oid}\td{index}\n" for index, oid in enumerate(subtrees))
        tree = f.git("mktree", data=rows.encode())
        commit = f.git("commit-tree", tree, data=b"spill fixture\n")
        f.git("update-ref", "refs/heads/main", commit)
        source = f.https()
        body = f"""import errno
from pathlib import Path
from time import monotonic, sleep
target = Path('d0/file')
assert target.stat().st_size == 65537
for index in range(1, 9):
    with Path(f'd{{index}}/file').open('rb', buffering=0) as pressure:
        assert pressure.read(1) == bytes([index])
run = Path.cwd().parent
deadline = monotonic() + 5
while not (spills := list(run.glob('raw-objects-*/{oids[0]}'))):
    assert monotonic() < deadline, 'target blob was not spilled after eight later reads'
    sleep(.01)
spill, = spills
assert spill.stat().st_size == 65537
with spill.open('r+b') as output:
    output.write(b'!')
try:
    with target.open('rb') as source:
        source.read(1)
except OSError as error:
    assert error.errno == errno.EIO, error
else:
    raise AssertionError('changed backing bytes were exposed')
print('caught-EIO')
"""
        mode = "nfs" if OPTIONS.require_nfs else "fuse"
        result = subprocess.run([OPTIONS.binary, source, "HEAD", "--backend=" + mode,
                                 "--write-policy=none", "--", sys.executable, "-c", body],
                                cwd=f.cwd, env=f.env, capture_output=True, timeout=30)
        self.assertEqual(result.stdout, b"caught-EIO\n", result.stderr)
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b"bytes were modified", result.stderr)
        self.assertIn(b"execution incomplete", result.stderr)
        self.assertFalse(run_directories(f.root))

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
        cases = ([(OPTIONS.before_policy_binary, errno.EROFS)] if OPTIONS.before_policy_binary else [])
        cases.append((OPTIONS.binary, errno.EIO))
        for binary, expected in cases:
            f.denied_oids = {f.ignore}
            for operation, path in (('create', 'created.tmp'), ('mkdir', 'created-dir')):
                with self.subTest(binary=str(binary), operation=operation):
                    f.requests.clear()
                    code = "import json,os,sys\ntry:\n"
                    code += (" fd=os.open(sys.argv[1],os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600); os.close(fd)\n"
                             if operation == 'create' else " os.mkdir(sys.argv[1])\n")
                    code += ("except OSError as error:\n"
                             " print(json.dumps({'errno':error.errno,'exists':os.path.lexists(sys.argv[1])}))\n"
                             "else:\n raise AssertionError('ignore acquisition allowed mutation')\n")
                    result = subprocess.run([binary, url, 'HEAD', '--', sys.executable, '-c', code, path],
                                            cwd=f.cwd, env=f.env, capture_output=True, timeout=30)
                    self.assertEqual(result.returncode, 125, result.stderr)
                    self.assertEqual(json.loads(result.stdout), {'errno': expected, 'exists': False}, result.stderr)
                    self.assertIn(b'execution incomplete', result.stderr)
                    self.assertTrue(any(f.ignore in r['wants'] and r.get('status') == 403 for r in f.requests))
                    self.assertFalse(run_directories(f.root))
        # With healthy rules, policy denial retains its distinct EROFS meaning.
        f.denied_oids.clear(); shutil.rmtree(f.root/'cache',ignore_errors=True)
        code="import errno,os\nfor operation in (lambda: os.open('denied',os.O_CREAT|os.O_WRONLY,0o600),lambda: os.mkdir('denied-dir')):\n try: operation()\n except OSError as error: assert error.errno==errno.EROFS,error\n else: raise AssertionError('unignored mutation allowed')\nfd=os.open('created.tmp',os.O_CREAT|os.O_WRONLY,0o600); os.close(fd)\nos.mkdir('created-dir')\n"
        result=subprocess.run([str(OPTIONS.binary),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(run_directories(f.root),[])

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
                    self.assertEqual(run_directories(f.root),[])
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
            self.assertEqual(run_directories(f.root),[])

if __name__=='__main__': unittest.main(argv=[__file__,*ARGS])
