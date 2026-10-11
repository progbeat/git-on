#!/usr/bin/env python3
"""Deterministic native Git, HTTPS and OpenSSH acceptance. Keys are disposable.

The source probe runs the production loader without FUSE; --require-fuse also
executes the installed CLI through actual mounts. No transport is mocked.
"""
import argparse
import contextlib
import http.server
import json
import os
import pty
from pathlib import Path
import pwd
import random
import re
import select
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
PARSER.add_argument('--before-binary', type=Path, help='Authentic pre-shutdown-fix binary for focused red-before replay')
OPTIONS, ARGS = PARSER.parse_known_args()

def run(argv, **kwargs):
    result=subprocess.run(list(map(str, argv)), capture_output=True, **kwargs)
    if result.returncode: raise RuntimeError('fixture command failed: '+result.stderr.decode())
    return result.stdout

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
    def ssh(self, encrypted=False):
        sshd = shutil.which('sshd') or '/usr/sbin/sshd'
        if not Path(sshd).exists(): raise RuntimeError('OpenSSH server missing; install openssh-server')
        host, identity = self.root/'host', self.root/'identity'
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', host])
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', 'fixture-password' if encrypted else '', '-f', identity])
        authorized = self.root/'authorized'; authorized.write_bytes((self.root/'identity.pub').read_bytes())
        with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port = sock.getsockname()[1]
        config = self.root/'sshd_config'
        config.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nPidFile {self.root}/sshd.pid\nAuthorizedKeysFile {authorized}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nLogLevel VERBOSE\n')
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

class RemoteTests(unittest.TestCase):
    def setUp(self):
        self.f = Fixture(); self.addCleanup(self.f.close)
    def test_review_fixture_binary_stdout_is_preserved(self):
        payload=b' \t\r\n\v\f\x00\xffbinary\x80\x00 \t\r\n\v\f'
        actual=run([sys.executable,'-c','import sys; sys.stdout.buffer.write(sys.stdin.buffer.read())'],input=payload)
        self.assertEqual(actual,payload)
    def test_https_cold_warm_lazy_objects_ref_race_and_local_equivalence(self):
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
        warm=f.probe(url, f.commit); f.line(warm); f.operation(warm,'read deep/file'); f.finish(warm)
        # Warm access needs advertisement/authentication but no object fetch.
        new=f.requests[requests:]
        self.assertEqual(len([x for x in new if x['path'].endswith('git-upload-pack')]), 1)
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
    def test_native_ssh_signature_tag_tree_path_replacements_and_failures(self):
        f=self.f; signed,allowed=self.signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        replacement_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        replacement=f.git('commit-tree',replacement_tree,data=b'different replacement\n')
        f.git('update-ref','refs/replace/'+signed,replacement)
        for revision in (signed,signed+':deep','signed','signed:deep'):
            process=f.probe(url,revision,verify=True); report=f.line(process)
            self.assertEqual(report['tree'],f.subtree if ':' in revision else f.tree)
            value,_=f.operation(process,'read file' if ':' in revision else 'read deep/file')
            self.assertEqual(value['data'],'expected\n'); f.finish(process)
        # Local verification uses the same effective inherited git -c settings.
        f.cwd=f.repo
        process=f.probe('local',signed+':deep',verify=True); f.line(process); f.operation(process,'read file'); f.finish(process)
        raw=subprocess.run(['git','-C',str(f.repo),'--no-replace-objects','cat-file','commit',signed],capture_output=True,check=True,env=f.env).stdout
        invalid=f.git('hash-object','-t','commit','-w','--stdin',data=raw.replace(b'signed fixture',b'altered fixture'))
        for revision in (invalid,f.commit,f.tree,f.blob):
            process=f.probe('local',revision,verify=True); _,errors=process.communicate(timeout=20)
            self.assertNotEqual(process.returncode,0,revision)
            self.assertTrue(b'execution forbidden' in errors or b'raw trees' in errors,errors)
        allowed.write_text('')
        process=f.probe('local',signed,verify=True); _,errors=process.communicate(timeout=20)
        self.assertNotEqual(process.returncode,0); self.assertIn(b'execution forbidden',errors)
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
        process.send_signal(signal.SIGTERM)
        try: process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            os.kill(int(pid.read_text()),signal.SIGKILL)
            raise
        self.assertEqual(process.returncode,-signal.SIGTERM)
        proc=Path('/proc/'+pid.read_text().strip())
        # Some container PID 1 implementations do not reap adopted zombies.
        self.assertTrue(not proc.exists() or (proc/'stat').read_text().split()[2]=='Z')
        self.assertEqual(list(f.root.glob('git-on-*')),[])
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
    def test_real_mount_remote_keep_cleanup_and_cancellation(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        result=subprocess.run(['git','-c','http.sslCAInfo='+f.env['GIT_SSL_CAINFO'],'on',url,'main','-k','--','sh','-c','cat deep/file; mkdir out; printf kept > out/result'],cwd=f.cwd,env=dict(f.env,PATH=str(OPTIONS.binary.resolve().parent)+':'+f.env['PATH']),capture_output=True,timeout=30)
        self.assertEqual(result.returncode,0,result.stderr); self.assertEqual(result.stdout,b'expected\n')
        retained=list(f.root.glob('git-on-*')); self.assertEqual(len(retained),1)
        self.assertEqual((retained[0]/'backing/out/result').read_text(),'kept')
        self.assertFalse((retained[0]/'remote-repository').exists())
        self.assertFalse((retained[0]/'ssh-control-owner').exists())
        refused=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(retained[0])],capture_output=True,timeout=10)
        self.assertNotEqual(refused.returncode,0)
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--','sleep','30'],cwd=f.cwd,env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+10
        while time.monotonic()<deadline and not any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()): time.sleep(.02)
        self.assertTrue(any('/mount' in x and 'git-on:' in x for x in Path('/proc/self/mountinfo').read_text().splitlines()),'remote run never mounted')
        process.send_signal(signal.SIGTERM); process.communicate(timeout=10)
        self.assertEqual(process.returncode,-signal.SIGTERM)
        self.assertEqual(list(f.root.glob('git-on-*')),retained)

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
        self.assertNotEqual(process.returncode,0); self.assertIn(unreachable.encode(),errors)
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
                if depth==63: self.assertEqual(f.line(process)['tree'],f.tree); f.finish(process)
                else:
                    _,errors=process.communicate(timeout=20)
                    self.assertNotEqual(process.returncode,0); self.assertIn(b'exceeds 64',errors)

    def test_native_verifier_moving_ref_keeps_exact_distinct_tree(self):
        f=self.f; signed,allowed=self.signing(); f.cwd=f.repo
        different_tree=f.git('mktree',data=f'100644 blob {f.other}\tfile\n'.encode())
        different=f.git('commit-tree',different_tree,data=b'distinct tree B\n')
        f.git('update-ref','refs/heads/main',signed)
        verifier=f.root/'ssh-verifier'
        verifier.write_text(f'#!/bin/sh\ngit --git-dir="{f.repo}" update-ref refs/heads/main {different}\nexec ssh-keygen "$@"\n')
        verifier.chmod(0o700)
        f.env.update(GIT_CONFIG_COUNT='3',GIT_CONFIG_KEY_2='gpg.ssh.program',GIT_CONFIG_VALUE_2=str(verifier))
        process=f.probe('local','HEAD:deep',verify=True)
        self.assertEqual(f.line(process)['tree'],f.subtree)
        value,_=f.operation(process,'read file'); self.assertEqual(value['data'],'expected\n'); f.finish(process)
        self.assertEqual(f.git('rev-parse','HEAD'),different)

    def test_openpgp_invalid_and_unknown_trust_match_native_git(self):
        f=self.f; signing=f.root/'gpg-sign'; signing.mkdir(mode=0o700)
        self.addCleanup(run,['gpgconf','--kill','all'],env=dict(f.env,GNUPGHOME=str(signing)))
        run(['gpg','--batch','--pinentry-mode','loopback','--passphrase','','--quick-generate-key','Fixture <fixture@example.invalid>','ed25519','sign','0'],env=dict(f.env,GNUPGHOME=str(signing)))
        wrapper=f.root/'gpg-oracle'; wrapper.write_text(f'#!/bin/sh\nGNUPGHOME="{signing}" exec gpg "$@"\n'); wrapper.chmod(0o700)
        f.git('config','gpg.program',str(wrapper)); f.git('config','user.signingkey','fixture@example.invalid')
        signed=f.git('commit-tree','-S',f.tree,data=b'trust fixture\n')
        public=run(['gpg','--batch','--export'],env=dict(f.env,GNUPGHOME=str(signing)))
        verification=f.root/'gpg-public'; verification.mkdir(mode=0o700)
        self.addCleanup(run,['gpgconf','--kill','all'],env=dict(f.env,GNUPGHOME=str(verification)))
        run(['gpg','--batch','--import'],input=public,env=dict(f.env,GNUPGHOME=str(verification)))
        wrapper.write_text(f'#!/bin/sh\nGNUPGHOME="{verification}" exec gpg "$@"\n')
        f.cwd=f.repo
        raw=subprocess.run(['git','-C',str(f.repo),'cat-file','commit',signed],env=f.env,capture_output=True,check=True).stdout
        invalid=f.git('hash-object','-t','commit','-w','--stdin',data=raw.replace(b'trust fixture',b'changed fixture'))
        for oid in (signed,invalid):
            native=subprocess.run(['git','-C',str(f.repo),'verify-commit',oid],env=f.env,capture_output=True)
            process=f.probe('local',oid,verify=True)
            if native.returncode==0:
                self.assertIn(b'unknown',native.stderr.lower()); f.line(process); f.finish(process)
            else:
                _,errors=process.communicate(timeout=20); self.assertNotEqual(process.returncode,0)
                self.assertIn(b'execution forbidden',errors)
        self.assertEqual(subprocess.run(['git','-C',str(f.repo),'verify-commit',signed],env=f.env,capture_output=True).returncode,0)

    def test_real_mount_native_git_c_verification_and_caught_transport_failure(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; signed,allowed=self.signing(); url=f.https(); self.addCleanup(f.server.shutdown)
        env=dict(f.env,PATH=str(OPTIONS.binary.resolve().parent)+':'+f.env['PATH'])
        for key in list(env):
            if key.startswith('GIT_CONFIG_KEY_') or key.startswith('GIT_CONFIG_VALUE_') or key=='GIT_CONFIG_COUNT': del env[key]
        for cwd,operands in ((f.cwd,[url,signed+':deep']),(f.repo,[signed+':deep'])):
            result=subprocess.run(['git','-c','gpg.format=ssh','-c','gpg.ssh.allowedSignersFile='+str(allowed),'on','--verify',*operands,'--','cat','file'],cwd=cwd,env=env,capture_output=True,timeout=30)
            self.assertEqual(result.returncode,0,result.stderr); self.assertEqual(result.stdout,b'expected\n')
        shutil.rmtree(f.root/'cache')
        # The command catches EIO and returns 0. The runner must still report 125.
        f.denied_oids.add(f.blob)
        code="import errno\ntry:\n open('deep/file').read()\nexcept OSError as e:\n assert e.errno == errno.EIO\nelse:\n raise AssertionError('transport fault not observed')\n"
        result=subprocess.run([str(OPTIONS.binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
        self.assertEqual(result.returncode,125,result.stderr)
        self.assertIn(f.blob.encode(),result.stderr); self.assertIn(b'execution incomplete',result.stderr)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def test_million_distinct_file_remote_cold_and_warm_bounded_fetch(self):
        if not OPTIONS.million: self.skipTest('million-file acceptance explicitly selected in CI')
        import benchmark
        f=self.f; fixture=benchmark.digit_fixture(f.repo,1_000_000)
        commit=f.git('rev-parse','refs/bench/digits-1000000')
        f.git('update-ref','refs/heads/main',commit); tree=fixture['tree_oid']
        url=f.https(); self.addCleanup(f.server.shutdown)
        selected=f'{random.Random(0x5eed).randrange(1_000_000):06d}'; path='/'.join(selected)
        process=f.probe(url); initial=f.line(process)
        self.assertEqual(set(initial['objects']),{commit,tree}); f.finish(process)
        shutil.rmtree(f.root/'cache')
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
        self.assertFalse(any(x['wants'] for x in f.requests[start:]))
        if OPTIONS.require_fuse:
            for index,command in enumerate((['true'],['cat',path],['cat',path])):
                if index<2: shutil.rmtree(f.root/'cache',ignore_errors=True)
                begin=len(f.requests)
                result=subprocess.run([str(OPTIONS.binary.resolve()),url,'HEAD','--',*command],cwd=f.cwd,env=f.env,capture_output=True,timeout=60)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(result.stdout,b'' if command[0]=='true' else (selected+'\n').encode())
                demands={oid for x in f.requests[begin:] for oid in x['wants']}
                self.assertEqual(demands, {commit,tree} if index==0 else set(read['objects']) if index==1 else set())
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
            os.chdir(f.cwd)
            os.execve(str(OPTIONS.probe.resolve()),[str(OPTIONS.probe.resolve()),url,'HEAD'],env)
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
        code="import json,os,time; from pathlib import Path; Path('out').mkdir(); Path('out/result').write_text('recover'); Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'run':str(Path.cwd().parent)})); time.sleep(300)"
        process=subprocess.Popen([str(OPTIONS.binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=dict(f.env,READY=str(ready)),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        f.children.append(process)
        deadline=time.monotonic()+20
        while not ready.exists() and process.poll() is None and time.monotonic()<deadline: time.sleep(.02)
        self.assertTrue(ready.exists())
        state=json.loads(ready.read_text()); directory=Path(state['run'])
        control=['ssh','-F','/dev/null','-S',str(directory/'ssh-control'),'-O','check','git-on-control']
        self.assertEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertTrue((directory/'ssh-control-owner').is_file())
        def cleanup():
            with contextlib.suppress(ProcessLookupError): os.killpg(state['pid'],signal.SIGKILL)
            if process.poll() is None: process.kill()
            process.communicate(timeout=10)
            if directory.exists(): subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.addCleanup(cleanup)
        active=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=10)
        self.assertNotEqual(active.returncode,0); self.assertTrue((directory/'backing').exists())
        process.kill(); os.killpg(state['pid'],signal.SIGKILL); process.communicate(timeout=10)
        self.assertEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertTrue((directory/'remote-repository').exists())
        stale=subprocess.run([str(OPTIONS.binary.resolve()),'--cleanup',str(directory)],capture_output=True,timeout=20)
        self.assertEqual(stale.returncode,0,stale.stderr); self.assertFalse(directory.exists())
        self.assertNotEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

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

    def test_review_real_mount_caught_cache_fault_forces_runner_failure(self):
        if not OPTIONS.require_fuse: self.skipTest('real FUSE explicitly required in CI')
        f=self.f; url=f.https(); self.addCleanup(f.server.shutdown)
        code="import errno,os; from pathlib import Path; (Path(os.environ['XDG_CACHE_HOME'])/'git-on/objects-v1/not-an-object').write_text('invalid cache entry')\ntry:\n open('deep/file').read()\nexcept OSError as e:\n assert e.errno == errno.EIO\nelse:\n raise AssertionError('cache fault not observed')\n"
        cases=([(OPTIONS.before_binary,0)] if OPTIONS.before_binary else [])+[(OPTIONS.binary,125)]
        for binary,expected in cases:
            shutil.rmtree(f.root/'cache',ignore_errors=True)
            result=subprocess.run([str(binary.resolve()),url,'HEAD','--',sys.executable,'-c',code],cwd=f.cwd,env=f.env,capture_output=True,timeout=30)
            self.assertEqual(result.returncode,expected,result.stderr)
            if expected==125:
                self.assertIn(b'not-an-object',result.stderr)
                self.assertIn(b'execution incomplete',result.stderr)
            self.assertEqual(list(f.root.glob('git-on-*')),[])
            print('cache fault regression evidence:',json.dumps(dict(binary=str(binary),expected=expected,actual=result.returncode)),flush=True)

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
