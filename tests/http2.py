"""Exercise the HTTP/2 demand path on either real native mount."""
from concurrent.futures import ThreadPoolExecutor
import base64
from collections import Counter
from contextlib import ExitStack
import errno
import gzip
import http.client
import io
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import socket
import socketserver
import ssl
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest
from types import SimpleNamespace
from urllib.parse import urlsplit

from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import DataReceived, RequestReceived, StreamEnded, StreamReset

REJECTION_CHALLENGE = 'Basic realm="git-on-rejected"'
REJECTION_INPUT = shlex.quote('wwwauth[]=' + REJECTION_CHALLENGE)

from remote import ARGS, OPTIONS, Fixture


class Multiplexer(socketserver.ThreadingTCPServer):
    daemon_threads = True

    def __init__(self, fixture, backend, authorization):
        self.backend = urlsplit(backend)
        self.authorization = authorization
        self.upstream_tls = ssl.create_default_context(cafile=fixture.env['GIT_SSL_CAINFO'])
        self.records = []
        self.authorized_fetch_packets = []
        self.split_pack = None
        self.unauthenticated_wants = set()
        self.unexpected_credential = False
        self.authenticate_wants_only = False
        self.gzip_pack = False
        self.switch_to_http1 = False
        self.http1_headers_sent = threading.Event()
        self.release_http1_body = threading.Event()
        super().__init__(('127.0.0.1', 0), MultiplexedRequests)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(fixture.root / 'tls.crt', fixture.root / 'tls.key')
        tls.set_alpn_protocols(['h2', 'http/1.1'])
        self.socket = tls.wrap_socket(self.socket, server_side=True)

    def forward(self, method, path, headers, body):
        authorization = next((value for key, value in headers.items()
                              if key.lower() == 'authorization'), None)
        if (method == 'POST' and self.authorization is not None
                and authorization != self.authorization
                and (not self.authenticate_wants_only or b'want ' in body)):
            self.unauthenticated_wants.update(re.findall(rb'want ([0-9a-f]{40})', body))
            if authorization is not None:
                self.unexpected_credential = True
            challenge = REJECTION_CHALLENGE if authorization is not None else 'Basic realm="git-on-fixture"'
            return 401, [('www-authenticate', challenge), ('content-length', '0')], b''
        upstream = http.client.HTTPSConnection(
            self.backend.hostname, self.backend.port, context=self.upstream_tls, timeout=30,
        )
        try:
            upstream.request(method, path, body=body, headers=headers)
            response = upstream.getresponse()
            fields = response.getheaders()
            payload = response.read()
            if (self.gzip_pack and response.status == 200
                    and (b'want ' in body or b'want-ref ' in body)):
                payload = gzip.compress(payload)
                fields = [(key, value) for key, value in fields if key.lower() != 'content-length']
                fields.extend([('Content-Encoding', 'gzip'), ('Content-Length', str(len(payload)))])
            return response.status, fields, payload
        finally:
            upstream.close()


class MultiplexedRequests(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            if self.request.selected_alpn_protocol() == 'h2':
                self.handle_h2()
            else:
                self.handle_http1()
        except (BrokenPipeError, ConnectionResetError, ssl.SSLEOFError):
            pass

    def handle_h2(self):
        h2 = H2Connection(H2Configuration(client_side=False, header_encoding='utf-8'))
        h2.initiate_connection()
        self.request.sendall(h2.data_to_send())
        self.request.settimeout(.02)
        pending = {}
        responses = {}
        sending = {}
        split_positions = {}
        with ThreadPoolExecutor() as workers:
            while True:
                try:
                    data = self.request.recv(65536)
                except socket.timeout:
                    data = None
                if data == b'':
                    return
                for event in h2.receive_data(data) if data else ():
                    stream = getattr(event, 'stream_id', None)
                    if isinstance(event, RequestReceived):
                        pending[stream] = (dict(event.headers), bytearray())
                    elif isinstance(event, DataReceived):
                        pending[stream][1].extend(event.data)
                        h2.acknowledge_received_data(event.flow_controlled_length, stream)
                    elif isinstance(event, StreamReset):
                        pending.pop(stream, None)
                        response = responses.pop(stream, None)
                        if response is not None:
                            response.cancel()
                        sending.pop(stream, None)
                        split_positions.pop(stream, None)
                    elif isinstance(event, StreamEnded):
                        headers, body = pending.pop(stream)
                        wants = re.findall(rb'want ([0-9a-f]{40})', body)
                        authorized = headers.get('authorization') == self.server.authorization
                        if wants and authorized:
                            self.server.records.append((self.client_address, wants))
                            self.server.authorized_fetch_packets.append(bytes(body))
                        split = self.server.split_pack
                        if split is not None and set(wants) == split.wants and authorized:
                            h2.send_headers(stream, [
                                (':status', '200'),
                                ('content-type', 'application/x-git-upload-pack-result'),
                                ('content-length', str(len(split.prefix) + len(split.tail))),
                            ])
                            split_positions[stream] = 0
                            continue
                        forwarded = {key: value for key, value in headers.items() if not key.startswith(':')}
                        responses[stream] = workers.submit(
                            self.server.forward, headers[':method'], headers[':path'], forwarded, body,
                        )
                completed = False
                for stream, result in list(responses.items()):
                    if not result.done():
                        continue
                    status, headers, body = responses.pop(stream).result()
                    headers = [(':status', str(status)), *(
                        (key.lower(), value) for key, value in headers if key.lower() != 'connection'
                    )]
                    h2.send_headers(stream, headers, end_stream=not body)
                    if body:
                        sending[stream] = (body, 0)
                    else:
                        completed = True
                for stream, (body, offset) in list(sending.items()):
                    offset = self.send_body(h2, stream, body, offset, end=True)
                    if offset == len(body):
                        sending.pop(stream)
                        completed = True
                    else:
                        sending[stream] = (body, offset)
                for stream, position in list(split_positions.items()):
                    split = self.server.split_pack
                    prefix_end = len(split.prefix)
                    if position < prefix_end:
                        position = self.send_body(h2, stream, split.prefix, position)
                        split_positions[stream] = position
                        if position == prefix_end:
                            split.started.touch()
                    if position >= prefix_end and split.release.exists():
                        if split.tail is None:
                            return
                        offset = self.send_body(h2, stream, split.tail, position - prefix_end, end=True)
                        if offset == len(split.tail):
                            split_positions.pop(stream)
                        else:
                            split_positions[stream] = prefix_end + offset
                switch = self.server.switch_to_http1 and completed
                if switch:
                    self.server.switch_to_http1 = False
                    self.server.socket.context.set_alpn_protocols(['http/1.1'])
                    h2.close_connection()
                outgoing = h2.data_to_send()
                if outgoing:
                    # The short receive timeout polls upstream futures. A
                    # large TLS write can take longer under backpressure.
                    self.request.settimeout(5)
                    try:
                        self.request.sendall(outgoing)
                    finally:
                        self.request.settimeout(.02)
                if switch:
                    return

    @staticmethod
    def send_body(h2, stream, data, offset, end=False):
        while offset < len(data):
            count = min(len(data) - offset, h2.max_outbound_frame_size, h2.local_flow_control_window(stream))
            if count <= 0:
                break
            next_offset = offset + count
            h2.send_data(stream, data[offset:next_offset], end_stream=end and next_offset == len(data))
            offset = next_offset
        return offset

    def handle_http1(self):
        # Native Git can bootstrap using either version of HTTP.
        with self.request.makefile('rb') as stream:
            request = stream.readline()
            if not request:
                return
            method, path, _ = request.decode().split()
            headers = http.client.parse_headers(stream)
            body = stream.read(int(headers.get('Content-Length', 0)))
        status, headers, body = self.server.forward(method, path, dict(headers), body)
        fields = ''.join(f'{key}: {value}\r\n' for key, value in headers if key.lower() != 'connection')
        self.request.sendall(f'HTTP/1.1 {status} Git\r\nConnection: close\r\n{fields}\r\n'.encode())
        if body is None:
            self.server.http1_headers_sent.set()
            self.server.release_http1_body.wait(30)
        else:
            self.request.sendall(body)


class Http2Tests(unittest.TestCase):
    def test_copy_streams_http1_pack_with_discovery_and_ref_retry(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        for scenario in ('http1', 'downgrade', 'ref-probe'):
            with self.subTest(scenario=scenario), ExitStack() as stack:
                proxy = Multiplexer(fixture, backend, None)
                stack.callback(proxy.server_close)
                if scenario == 'downgrade':
                    proxy.switch_to_http1 = True
                else:
                    proxy.socket.context.set_alpn_protocols(['http/1.1'])
                requests = []
                upstream = proxy.forward

                def forward(method, path, headers, body):
                    wants = b'want ' in body
                    requests.append((method, wants))
                    if scenario == 'ref-probe' and wants and ('POST', False) not in requests:
                        return 503, [('content-length', '0')], b''
                    return upstream(method, path, headers, body)

                proxy.forward = forward
                threading.Thread(target=proxy.serve_forever, daemon=True).start()
                stack.callback(proxy.shutdown)
                source = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
                with tempfile.TemporaryFile() as metrics:
                    env = dict(fixture.env, GIT_ON_TEST_METRICS_FD=str(metrics.fileno()))
                    result = subprocess.run(
                        [OPTIONS.binary, source, fixture.commit, '--backend=copy',
                         '--write-policy=all', '--', 'cat', 'deep/file'],
                        cwd=fixture.cwd, env=env, capture_output=True, timeout=20,
                        pass_fds=(metrics.fileno(),),
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, b'expected\n')
                    self.assertFalse(proxy.switch_to_http1, 'the discovery response did not use HTTP/2')
                    expected = [('GET', False), ('POST', True)]
                    if scenario == 'ref-probe':
                        expected += [('POST', False), ('POST', True)]
                    self.assertEqual(requests, expected)
                    metrics.seek(0)
                    report = json.load(metrics)
                    self.assertEqual(report['remote_session_fetch_operations'], 1, report)
                    self.assertEqual(report['remote_http2_fetch_operations'], 0, report)
                    self.assertEqual(report['remote_native_pack_bytes'], 0, report)

    def test_https_native_fallback_keeps_the_selected_revision(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        changed_subtree = fixture.git('mktree', data=f'100644 blob {fixture.other}\tfile\n'.encode())
        changed_tree = fixture.git('mktree', data=f'040000 tree {changed_subtree}\tdeep\n'.encode())
        moved = fixture.git('commit-tree', changed_tree, '-p', fixture.commit, data=b'moved ref\n')
        proxy = Multiplexer(fixture, backend, None)
        self.addCleanup(proxy.server_close)
        upstream = proxy.forward
        wants = []

        def forward(method, path, headers, body):
            request = re.findall(rb'want ([0-9a-f]{40})', body)
            if request:
                wants.extend(request)
                if scenario in ('lazy', 'copy') and len(wants) == len(request):
                    fixture.git('update-ref', 'refs/heads/main', moved)
                    return 407, [('content-length', '0')], b''
            response = upstream(method, path, headers, body)
            if scenario == 'downgrade' and not wants and b'command=ls-refs\n' in body:
                if fixture.git('rev-parse', 'refs/heads/main') == fixture.commit:
                    fixture.git('update-ref', 'refs/heads/main', moved)
                    proxy.switch_to_http1 = True
            return response

        proxy.forward = forward
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)
        url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        for scenario in ('lazy', 'copy', 'mapped', 'remote-group', 'downgrade'):
            with self.subTest(scenario=scenario):
                wants.clear()
                fixture.git('update-ref', 'refs/heads/main', fixture.commit)
                if scenario == 'copy':
                    result = subprocess.run(
                        [OPTIONS.binary, url, 'main', '--backend=copy', '--write-policy=all',
                         '--', 'cat', 'deep/file'],
                        cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=20,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, b'expected\n')
                elif scenario == 'mapped':
                    env = fixture.env.copy()
                    try:
                        fixture.env.update(
                            GIT_CONFIG_COUNT='2',
                            GIT_CONFIG_KEY_0='remote.origin.url', GIT_CONFIG_VALUE_0=url,
                            GIT_CONFIG_KEY_1='remote.origin.fetch',
                            GIT_CONFIG_VALUE_1='+refs/heads/*:refs/remotes/origin/*',
                        )
                        process = fixture.probe('origin', 'main')
                        self.assertEqual(fixture.line(process)['tree'], fixture.tree)
                        repositories = list(fixture.root.glob('*/remote-repository'))
                        self.assertEqual(len(repositories), 1, repositories)
                        tracked = fixture.git('--git-dir', str(repositories[0]), 'rev-parse',
                                              'refs/remotes/origin/main')
                        self.assertEqual(tracked, fixture.commit)
                        fixture.finish(process)
                    finally:
                        fixture.env = env
                    continue
                elif scenario == 'remote-group':
                    local = fixture.repo.as_uri()
                    for name_url, revision in ((url, 'main'), (None, fixture.commit)):
                        with self.subTest(own_url=name_url is not None):
                            options = [('remotes.pool', 'one two'),
                                       ('remote.one.url', local), ('remote.two.url', local)]
                            if name_url:
                                options.append(('remote.pool.url', name_url))
                            env = dict(fixture.env, GIT_CONFIG_COUNT=str(len(options)))
                            for index, (key, value) in enumerate(options):
                                env[f'GIT_CONFIG_KEY_{index}'] = key
                                env[f'GIT_CONFIG_VALUE_{index}'] = value
                            before = len(fixture.requests)
                            result = subprocess.run(
                                [OPTIONS.probe, 'pool', revision], cwd=fixture.cwd, env=env,
                                input=b'exit\n', capture_output=True, timeout=20,
                            )
                            self.assertNotEqual(result.returncode, 0, result)
                            self.assertEqual(result.stdout, b'')
                            self.assertIn(b'fetching a group and specifying refspecs', result.stderr)
                            self.assertEqual(len(fixture.requests), before)
                    continue
                else:
                    process = fixture.probe(url, 'main')
                    self.assertEqual(fixture.line(process)['tree'], fixture.tree)
                    fixture.finish(process)
                self.assertEqual(wants[:2], [fixture.commit.encode()] * 2)

    def test_mount_can_recover_https_preparation(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        mount = 'nfs' if OPTIONS.require_nfs else 'fuse'
        token = base64.b64encode(b'fixture:example-password').decode()
        for scenario in ('403', '404', 'downgrade', 'held-downgrade'):
            with self.subTest(scenario=scenario), ExitStack() as stack:
                denial = int(scenario) if scenario in ('403', '404') else None
                proxy = Multiplexer(fixture, backend, 'Basic ' + token)
                stack.callback(proxy.server_close)
                upstream = proxy.forward
                requests = []

                def forward(method, path, headers, body):
                    if method == 'POST':
                        requests.append(bytes(body))
                        want = b'want ' in body
                        authorized = any(key.lower() == 'authorization' for key in headers)
                        if denial is not None and want and not authorized:
                            return denial, [('content-length', '0')], b''
                        if len(requests) == 1 and denial is None:
                            proxy.switch_to_http1 = True
                        if scenario.endswith('downgrade') and len(requests) == 2 and want:
                            hold = scenario == 'held-downgrade'
                            return 200, [
                                ('Content-Type', 'application/x-git-upload-pack-result'),
                                ('Content-Length', '123' if hold else '0'),
                            ], None if hold else b''
                    return upstream(method, path, headers, body)

                proxy.forward = forward
                threading.Thread(target=proxy.serve_forever, daemon=True).start()
                stack.callback(proxy.shutdown)
                stack.callback(proxy.release_http1_body.set)
                operations = fixture.root / f'credentials-{scenario}'
                quoted = shlex.quote(str(operations))
                env = dict(
                    fixture.env, GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='credential.helper',
                    GIT_CONFIG_VALUE_0=(
                        f'!f() {{ if test "$1" = get; then '
                        f'if grep -qx get {quoted} && ! grep -qx store {quoted}; then '
                        "echo quit=1; else echo username=fixture; echo password=example-password; fi; fi; "
                        f'echo "$1" >>{quoted}; }}; f'
                    ),
                )
                url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
                with tempfile.TemporaryFile() as metrics:
                    env['GIT_ON_TEST_METRICS_FD'] = str(metrics.fileno())
                    result = subprocess.run(
                        [OPTIONS.binary, url, fixture.commit, f'--backend={mount}',
                         '--write-policy=none', '--', 'cat', 'deep/file'],
                        cwd=fixture.cwd, env=env, capture_output=True, timeout=15,
                        pass_fds=(metrics.fileno(),),
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    if denial is not None:
                        metrics.seek(0)
                        report = json.load(metrics)
                        self.assertGreater(report['remote_http2_fetch_operations'], 0, report)
                        self.assertEqual(report['remote_native_pack_bytes'], 0, report)
                self.assertEqual(result.stdout, b'expected\n')
                self.assertIn(f'want {fixture.commit}\n'.encode(), requests[0])
                self.assertTrue(any(b'command=ls-refs\n' in request for request in requests[1:]))
                if scenario == 'held-downgrade':
                    self.assertTrue(proxy.http1_headers_sent.is_set())
                self.assertEqual(operations.read_text().splitlines()[:2], ['get', 'store'])

    def test_basic_credential_continuation_for_copy_and_lazy_reads(self):
        capabilities = subprocess.run(['git', 'credential', 'capability'], capture_output=True)
        advertised = capabilities.stdout.splitlines()
        if b'capability state' not in advertised:
            self.skipTest('native Git lacks the credential state capability')
        fixture = Fixture()
        self.addCleanup(fixture.close)
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, backend, None)
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)
        upstream = proxy.forward
        first = 'Basic ' + base64.b64encode(b'fixture:phase-one').decode()
        second = 'Basic ' + base64.b64encode(b'fixture:phase-two').decode()
        multiple = False
        challenge_discovery = False
        scheme_change = False
        challenge_refs = False
        ref_requests = []
        ref_recovery = []

        def forward(method, path, headers, body):
            refs = b'command=ls-refs\n' in body
            if refs:
                ref_requests.append(path)
            authorization = next((value for name, value in headers.items()
                                  if name.lower() == 'authorization'), None)
            initial = 'Bearer fixture-phase-one' if scheme_change else first
            accepted = 'Bearer fixture-phase-two' if scheme_change else second
            if challenge_refs:
                if method == 'GET' and authorization != first:
                    return 401, [('www-authenticate', 'Basic realm="phase-one"')], b''
                if refs:
                    status = 200 if authorization == second else 401
                    ref_recovery.append((status, ()))
                    if status == 401:
                        return 401, [('www-authenticate', 'Basic realm="phase-two"')], b''
                wants = tuple(re.findall(rb'want ([0-9a-f]{40})', body))
                if wants:
                    status = 200 if authorization == second else 403
                    ref_recovery.append((status, wants))
                    if status == 403:
                        return 403, [('content-length', '0')], b''
                return upstream(method, path, headers, body)
            if (b'want ' in body or (challenge_discovery and method == 'GET')) and authorization != accepted:
                phase = 'two' if authorization == initial else 'one'
                scheme = 'Bearer' if scheme_change and phase == 'two' else 'Basic'
                challenge = f'{scheme} realm="phase-{phase}"'
                if phase == 'two' and not scheme_change:
                    challenge += ', charset="UTF-8"'
                fields = [('www-authenticate', challenge), ('content-length', '0')]
                if multiple and phase == 'two':
                    fields.append(('www-authenticate', 'Basic realm="alternative"'))
                return 401, fields, b''
            return upstream(method, path, headers, body)

        proxy.forward = forward
        helper = fixture.root / 'credential-helper.py'
        helper.write_text(
            'import base64\n'
            'import sys\n'
            'from pathlib import Path\n'
            'log, mode, operation = sys.argv[1:]\n'
            'values = sys.stdin.read().splitlines()\n'
            'state = next((line[8:] for line in values if line.startswith("state[]=")), "-")\n'
            'with Path(log).open("a") as output:\n'
            '    output.write(f"{operation} {state}\\n")\n'
            'encoded = mode in ("encoded", "scheme-change")\n'
            'scheme = "Bearer" if mode == "scheme-change" else "Basic"\n'
            'def emit(password):\n'
            '    if encoded:\n'
            '        print(f"capability[]=authtype\\nauthtype={scheme}")\n'
            '        value = (f"fixture-{password}" if mode == "scheme-change" else\n'
            '                 base64.b64encode(f"fixture:{password}".encode()).decode())\n'
            '        print(f"credential={value}")\n'
            '    else:\n'
            '        print(f"username=fixture\\npassword={password}")\n'
            'if encoded and "capability[]=authtype" not in values:\n'
            '    print("quit=1")\n'
            '    sys.exit(0)\n'
            'if encoded and operation == "store":\n'
            '    if f"authtype={scheme}" not in values or not any(v.startswith("credential=") for v in values):\n'
            '        Path(log).write_text("lost encoded credential before approval\\n")\n'
            'if operation == "get":\n'
            '    print("capability[]=state")\n'
            '    if state == "-" and Path(log).read_text().count("get -\\n") > 1:\n'
            '        print("quit=1")\n'
            '    elif state == "-":\n'
            '        emit("phase-one")\n'
            '        print("continue=1\\nstate[]=first")\n'
            '    elif state == "first":\n'
            '        if mode == "scheme-change" and \'wwwauth[]=Bearer realm="phase-two"\' not in values:\n'
            '            print("quit=1")\n'
            '            sys.exit(0)\n'
            '        emit("phase-two")\n'
            '        print("continue=0\\nstate[]=second")\n'
            '    else:\n'
            '        print("quit=1")\n'
        )
        env = dict(fixture.env, HOME=str(fixture.root), GIT_CONFIG_COUNT='1',
                   GIT_CONFIG_KEY_0='credential.helper')
        env.pop('NETRC', None)
        source = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        oracle = fixture.root / 'oracle.git'
        fixture.git('init', '--bare', str(oracle), at=fixture.root)
        native = ['git', '--git-dir', oracle, 'fetch', '--no-tags', '--depth=1', '--', source, 'HEAD']
        copy = [OPTIONS.binary, source, fixture.commit, '--backend=copy',
                '--write-policy=all', '--', 'cat', 'deep/file']
        mount = 'fuse' if OPTIONS.require_fuse else 'nfs'
        lazy = [OPTIONS.binary, source, fixture.commit, f'--backend={mount}',
                '--', 'cat', 'deep/file']
        encoded_cases = [
            ('native-encoded', native), ('copy-encoded', copy), ('lazy-encoded', lazy),
            ('native-scheme-change', native), ('copy-scheme-change', copy), ('lazy-scheme-change', lazy),
        ] if b'capability authtype' in advertised else []
        for kind, command in (
            ('native', native),
            ('copy', copy),
            ('lazy', lazy),
            ('lazy-refs-after-discovery', lazy),
            ('copy-multiple', copy),
            *encoded_cases,
            ('copy-http1', copy),
        ):
            with self.subTest(kind=kind):
                ref_requests.clear()
                ref_recovery.clear()
                if kind == 'copy-http1':
                    proxy.socket.context.set_alpn_protocols(['http/1.1'])
                scheme_change = kind.endswith('-scheme-change')
                multiple = kind in ('native', 'native-encoded', 'copy-multiple')
                challenge_discovery = multiple or kind.startswith('native')
                challenge_refs = kind == 'lazy-refs-after-discovery'
                operations = fixture.root / kind
                mode = ('scheme-change' if scheme_change else
                        'encoded' if kind.endswith('-encoded') else 'password')
                config = '!' + shlex.join([sys.executable, str(helper), str(operations), mode])
                result = subprocess.run(command, cwd=fixture.cwd, env=dict(env, GIT_CONFIG_VALUE_0=config),
                                        capture_output=True, timeout=20)
                self.assertEqual(result.returncode, 0, (
                    result.stderr, operations.read_text() if operations.exists() else '',
                ))
                if not kind.startswith('native'):
                    self.assertEqual(result.stdout, b'expected\n')
                expected = ['get -', 'get first', 'store second']
                if challenge_refs:
                    expected.insert(1, 'store first')
                    want = (fixture.commit.encode(),)
                    self.assertEqual(ref_recovery[:4], [(403, want), (401, ()), (200, ()), (200, want)])
                elif kind.startswith('lazy'):
                    self.assertEqual(ref_requests, [])
                self.assertEqual(operations.read_text().splitlines(), expected)

    def test_copy_materializes_one_shallow_pack_over_https_and_ssh(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        link = fixture.git('hash-object', '-w', '--stdin', data=b'deep/file')
        entries = fixture.git('ls-tree', fixture.tree).encode() + f'\n120000 blob {link}\tlink\n'.encode()
        fixture.tree = fixture.git('mktree', data=entries)
        fixture.commit = fixture.git('commit-tree', fixture.tree, data=b'eager copy fixture\n')
        fixture.git('update-ref', 'refs/heads/main', fixture.commit)
        fixture.git('config', 'uploadpack.allowFilter', 'false')
        fixture.git('update-server-info')
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, backend, None)
        alias_wants = []
        required_user_agent = None
        default_user_agent = 'git/' + fixture.git('--version').removeprefix('git version ')
        upstream = proxy.forward

        def forward_with_alias(method, path, headers, body):
            if required_user_agent and b'want ' in body and not any(
                    key.lower() == 'user-agent' and value == required_user_agent
                    for key, value in headers.items()):
                return 403, [('content-length', '0')], b''
            if path.startswith('/server.git//'):
                if b'want ' in body:
                    alias_wants.append(path)
                path = path.replace('/server.git//', '/server.git/', 1)
            status, fields, payload = upstream(method, path, headers, body)
            if proxy.gzip_pack and status == 200 and b'want ' in body:
                fields = [
                    (name, 'Application/X-Git-Upload-Pack-Result' if name.lower() == 'content-type' else value)
                    for name, value in fields
                ]
            return status, fields, payload

        proxy.forward = forward_with_alias
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)

        code = r"""
from pathlib import Path
import sys
assert Path('deep/file').read_bytes() == b'expected\n'
assert Path('other').read_bytes() == b'unrelated payload\n'
assert Path('link').readlink() == Path('deep/file')
repository = Path.cwd().parent / 'remote-repository'
if sys.argv[1] == 'credential-context':
    assert not any(path.is_file() for path in (repository / 'objects').rglob('*'))
elif sys.argv[1] == 'absent':
    assert not repository.exists()
"""
        https = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        ssh = fixture.ssh(protocol_v2=True, max_sessions=1)
        fallbacks = [('auto-https', https), ('auto-ssh', ssh)] if OPTIONS.require_nfs else []
        pack_config = {'pack.indexversion': '3', 'pack.writereverseindex': 'invalid'}
        native_cases = {
            'ambiguous-https', 'ref-in-want', 'session-id', 'scoped-auth', 'path-auth',
            'proxy-ca', 'user-agent', *pack_config,
        }
        for transport, url in [
            ('https', https), ('ssh', ssh),
            ('branch-https', https), ('branch-ssh', ssh), ('head-https', https + '//'),
            ('ambiguous-https', https), *fallbacks,
            ('authenticated', https), ('scoped-auth', https), ('path-auth', https),
            ('auth-rejected', https), ('auth-quit', https), ('auth-quit-traced', https), ('netrc', https),
            ('proxy-ca', https), ('user-agent', https),
            ('native-dumb', https),
            *((key, https) for key in pack_config),
            ('session-id', https), ('ref-in-want', https),
        ]:
            with self.subTest(transport=transport), tempfile.TemporaryFile() as metrics, \
                    tempfile.TemporaryDirectory(dir='/tmp', prefix='go-copy-') as runtime, \
                    ExitStack() as cleanup:
                proxy.gzip_pack = transport == 'https'
                env = dict(fixture.env, TMPDIR=runtime, GIT_ON_TEST_METRICS_FD=str(metrics.fileno()))
                required_user_agent = default_user_agent if transport == 'https' else None
                if transport == 'https':
                    env['GIT_TRACE_PACKET'] = '0'
                if transport == 'user-agent':
                    required_user_agent = 'git-on-fixture/1'
                    env['GIT_USER_AGENT'] = required_user_agent
                auth_quit = transport in ('auth-quit', 'auth-quit-traced')
                if transport == 'auth-quit-traced':
                    env['GIT_TRACE2'] = '1'
                native_error = None
                native_fetches = 0
                source = url
                revision = fixture.commit
                if transport in ('branch-https', 'branch-ssh', 'ambiguous-https', 'ref-in-want'):
                    revision = 'main'
                elif transport == 'head-https':
                    revision = 'HEAD'
                fallback = transport.startswith('auto-')
                if fallback:
                    fixture.git('config', 'uploadpack.allowFilter', 'true')
                    env['GIT_ON_TEST_NFS_MOUNT_ERROR'] = '1'
                    if transport == 'auto-ssh':
                        starts = fixture.root / 'auto-ssh-channel-starts'
                        fixture.force_upload_pack(
                            f'echo start >> {shlex.quote(str(starts))}\n'
                            'sleep 0.5\n'
                            f'exec git upload-pack {shlex.quote(str(fixture.repo))}\n'
                        )
                elif auth_quit or transport in ('authenticated', 'scoped-auth', 'path-auth', 'auth-rejected'):
                    fixture.git('config', 'uploadpack.allowFilter', 'false')
                    token = base64.b64encode(b'fixture:example-password').decode()
                    proxy.authorization = 'Basic ' + token
                    proxy.authenticate_wants_only = True
                    operations = Path(runtime) / 'credential-operations'
                    key = f'credential.{url}.helper' if transport == 'scoped-auth' else 'credential.helper'
                    password = 'declined-by-fixture' if transport == 'auth-rejected' else 'example-password'
                    challenge = shlex.quote('wwwauth[]=Basic realm="git-on-fixture"')
                    quit_once = ''
                    if auth_quit:
                        quit_once = (f'if test "$(wc -l <{shlex.quote(str(operations))})" -eq 1; '
                                     'then echo quit=1; return; fi; ')
                    env.update(
                        GIT_CONFIG_COUNT='1',
                        GIT_CONFIG_KEY_0=key,
                        GIT_CONFIG_VALUE_0=(
                            f'!f() {{ echo "$1" >>{shlex.quote(str(operations))}; '
                            'if test "$1" = get; then '
                            f'grep -Fxq {challenge} || return 1; '
                            f'{quit_once}'
                            f'echo username=fixture; echo password={password}; '
                            f'elif test "$1" = erase && grep -Fxq {REJECTION_INPUT}; '
                            f'then echo challenge >>{shlex.quote(str(operations))}; fi; }}; f'
                        ),
                    )
                    if auth_quit or transport == 'auth-rejected':
                        native_error = b'Git HTTPS authentication failed'
                    elif transport == 'path-auth':
                        env.update(GIT_CONFIG_COUNT='2', GIT_CONFIG_KEY_1='credential.useHttpPath',
                                   GIT_CONFIG_VALUE_1='true')
                elif transport == 'proxy-ca':
                    proxy.authorization = None
                    env['GIT_PROXY_SSL_CAINFO'] = fixture.env['GIT_SSL_CAINFO']
                elif transport == 'netrc':
                    proxy.authorization = 'Basic ' + base64.b64encode(b'fixture:netrc-fixture').decode()
                    home = Path(runtime) / 'home'
                    home.mkdir()
                    netrc = home / '.netrc'
                    netrc.write_text('machine 127.0.0.1 login fixture password netrc-fixture\n')
                    netrc.chmod(0o600)
                    operations = Path(runtime) / 'credential-operations'
                    helper = (
                        f'!f() {{ echo "$1" >>{shlex.quote(str(operations))}; '
                        'echo username=fixture; echo password=expired-fixture; }; f'
                    )
                    env.update(HOME=str(home), GIT_CONFIG_COUNT='1',
                               GIT_CONFIG_KEY_0='credential.helper', GIT_CONFIG_VALUE_0=helper)
                    env.pop('NETRC', None)
                elif transport == 'native-dumb':
                    env['GIT_SMART_HTTP'] = '0'
                    native_error = b'dumb http transport does not support shallow'
                elif transport in pack_config:
                    env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0=transport,
                               GIT_CONFIG_VALUE_0=pack_config[transport])
                elif transport == 'session-id':
                    fixture.git('config', 'transfer.advertiseSID', 'true')
                    env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='transfer.advertiseSID',
                               GIT_CONFIG_VALUE_0='true')
                elif transport == 'ambiguous-https':
                    tag = fixture.git('commit-tree', fixture.tree, data=b'competing tag\n')
                    fixture.git('update-ref', 'refs/tags/main', tag)
                    cleanup.callback(fixture.git, 'update-ref', '-d', 'refs/tags/main', tag)
                elif transport == 'ref-in-want':
                    proxy.authorization = None
                    fixture.git('config', 'uploadpack.allowRefInWant', 'true')
                    fixture.denied_oids.add(fixture.commit)
                if transport in pack_config or transport in (
                        'https', 'ambiguous-https', 'ref-in-want', 'netrc', 'user-agent'):
                    before_oracle = len(proxy.authorized_fetch_packets)
                    oracle = Path(runtime) / 'oracle.git'
                    fixture.git('init', '--bare', str(oracle), at=fixture.root)
                    native = subprocess.run(
                        ['git', '--git-dir', oracle, 'fetch', '--no-tags', '--depth=1',
                         '--no-filter', '--keep', '--', url, revision],
                        cwd=fixture.cwd, env=env, capture_output=True, timeout=20,
                    )
                    if transport in ('https', 'netrc', 'user-agent'):
                        self.assertEqual(native.returncode, 0, native.stderr)
                        if transport == 'netrc':
                            self.assertFalse(operations.exists())
                    elif transport == 'ambiguous-https':
                        self.assertEqual(native.returncode, 0, native.stderr)
                        selected = fixture.git('--git-dir', str(oracle), 'rev-parse', 'FETCH_HEAD')
                        self.assertEqual(selected, tag)
                    elif native.returncode:
                        native_error = native.stderr.splitlines()[-1]
                        native_fetches = len(proxy.authorized_fetch_packets) - before_oracle
                fetches_before = len(proxy.authorized_fetch_packets)
                upstream_before = len(fixture.requests)
                if transport == 'authenticated':
                    repository_usage = 'credential-context'
                else:
                    repository_usage = 'native' if transport in native_cases else 'absent'
                run = subprocess.run(
                    [OPTIONS.binary, source, revision,
                     '--backend=' + ('auto' if fallback else 'copy'), '--write-policy=all',
                     '--', sys.executable, '-I', '-S', '-c', code, repository_usage],
                    cwd=fixture.cwd, env=env, pass_fds=(metrics.fileno(),),
                    capture_output=True, timeout=20,
                )
                if native_error is not None:
                    self.assertNotEqual(run.returncode, 0)
                    self.assertIn(native_error, run.stderr)
                    self.assertEqual(len(proxy.authorized_fetch_packets) - fetches_before, native_fetches)
                    if transport == 'auth-rejected':
                        self.assertEqual(operations.read_text().splitlines(), ['get', 'erase', 'challenge'])
                    elif auth_quit:
                        self.assertEqual(operations.read_text().splitlines(), ['get'])
                    continue
                self.assertEqual(run.returncode, 0, run.stderr)
                metrics.seek(0)
                report = json.load(metrics)
                if transport == 'session-id':
                    requests = proxy.authorized_fetch_packets[fetches_before:]
                    self.assertEqual(len(requests), 1, requests)
                    self.assertIn(b'session-id=', requests[0])
                if transport in native_cases:
                    self.assertEqual(report['remote_fetch_operations'], 1, report)
                    self.assertEqual(report['remote_eager_verified_pack_capacity'], 0, report)
                else:
                    expected_fetches = 3 if fallback else 1
                    self.assertEqual(report['remote_session_fetch_operations'], expected_fetches, report)
                    self.assertEqual(report['remote_native_pack_bytes'], 0, report)
                    self.assertGreater(report['remote_eager_verified_pack_capacity'], 32, report)
                self.assertEqual(report['spool_bytes'], 0, report)
                if transport == 'ambiguous-https':
                    requests = proxy.authorized_fetch_packets[fetches_before:]
                    self.assertEqual(len(requests), 1, requests)
                    self.assertIn(f'want {selected}\n'.encode(), requests[0])
                if transport in ('https', 'authenticated', 'branch-https', 'head-https'):
                    requests = proxy.authorized_fetch_packets[fetches_before:]
                    self.assertEqual(len(requests), 1, requests)
                    self.assertIn(b'deepen 1\n', requests[0])
                    self.assertNotIn(b'filter ', requests[0])
                if transport == 'https':
                    posts = [
                        request for request in fixture.requests[upstream_before:]
                        if request['path'].endswith('/git-upload-pack')
                    ]
                    self.assertEqual([request['wants'] for request in posts], [[fixture.commit]])
                if transport == 'head-https':
                    self.assertEqual(alias_wants, ['/server.git//git-upload-pack'])
                if transport == 'auto-https':
                    requests = proxy.authorized_fetch_packets[fetches_before:]
                    self.assertEqual(sum(b'filter tree:0\n' in packet for packet in requests), 2)
                    self.assertEqual(sum(b'filter ' not in packet for packet in requests), 1)
                if transport == 'auto-ssh':
                    self.assertEqual(starts.read_text().splitlines(), ['start'])
                if transport in ('authenticated', 'scoped-auth', 'path-auth'):
                    self.assertEqual(operations.read_text().splitlines(), ['get', 'store'])
                if transport == 'netrc':
                    self.assertFalse(operations.exists())
        self.assertEqual(proxy.unauthenticated_wants, {fixture.commit.encode()})

    def test_eager_https_shows_actual_pack_progress(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, backend, None)
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)

        commit = fixture.git('commit-tree', fixture.tree, '-p', fixture.commit, data=b'child\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        objects = fixture.git('ls-tree', '-r', '-t', '--object-only', fixture.tree).splitlines()
        pack = subprocess.check_output(
            ['git', '-C', fixture.repo, 'pack-objects', '--stdout'],
            input=('\n'.join((commit, fixture.tree, *objects)) + '\n').encode(), env=fixture.env,
        )
        def packet(data):
            return f'{len(data) + 4:04x}'.encode() + data

        def sideband(data):
            return packet(b'\x01' + data)

        release = fixture.root / 'release-pack-trailer'
        pack_prefix = packet(b'packfile\n') + sideband(pack[:-20])
        shallow_prefix = packet(b'shallow-info\n')
        proxy.split_pack = SimpleNamespace(
            wants={commit.encode()},
            prefix=shallow_prefix + packet(f'shallow {commit}\n'.encode()) + b'0001' + pack_prefix,
            tail=sideband(pack[-20:]) + b'0000',
            started=fixture.root / 'prefix-sent',
            release=release,
        )
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        termios.tcsetwinsize(slave, (24, 100))
        url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        try:
            command = [OPTIONS.binary, url, commit, '--backend=copy', '--write-policy=all',
                       '--', 'cat', 'deep/file']
            process = subprocess.Popen(
                command,
                cwd=fixture.cwd, env=dict(fixture.env, TERM='xterm-256color'),
                stdout=subprocess.PIPE, stderr=slave,
            )
            fixture.children.append(process)
        finally:
            os.close(slave)
        transcript = bytearray()
        try:
            deadline = time.monotonic() + 4
            while time.monotonic() < deadline:
                if b'Receiving objects' in transcript and re.search(rb'[KM]iB/s', transcript):
                    break
                if process.poll() is not None:
                    break
                if select.select([master], [], [], .05)[0]:
                    try:
                        transcript.extend(os.read(master, 65536))
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
        finally:
            release.touch()
        output, _ = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, transcript)
        self.assertEqual(output, b'expected\n')
        self.assertIn(b'Receiving objects', transcript)
        self.assertRegex(transcript, rb'[KM]iB/s')
        proxy.split_pack.prefix = pack_prefix
        oracle = fixture.root / 'native-shallow-oracle.git'
        fixture.git('init', '--bare', str(oracle), at=fixture.root)
        native = subprocess.run(
            ['git', '--git-dir', oracle, 'fetch', '--no-tags', '--depth=1',
             '--no-filter', '--keep', '--', url, commit],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=20,
        )
        self.assertNotEqual(native.returncode, 0)
        self.assertIn(b'did not send all necessary objects', native.stderr)
        incomplete = subprocess.run(
            command, cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=20,
        )
        self.assertNotEqual(incomplete.returncode, 0)
        self.assertEqual(incomplete.stdout, b'')
        self.assertIn(b'shallow pack omitted required commit', incomplete.stderr)

    def test_post_authentication_and_multiplexed_fetches(self):
        with ExitStack() as cleanup:
            self.post_authentication_and_multiplexed_fetches(cleanup)

    def post_authentication_and_multiplexed_fetches(self, cleanup):
        self.assertTrue(OPTIONS.require_nfs or OPTIONS.require_fuse)
        fixture = Fixture()
        cleanup.callback(fixture.close)
        token = base64.b64encode(b'fixture:example-password').decode()
        backend = fixture.https()
        cleanup.callback(fixture.server.shutdown)
        proxy = Multiplexer(fixture, backend, 'Basic ' + token)
        proxy.authenticate_wants_only = True
        url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        cleanup.callback(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        cleanup.callback(proxy.shutdown)
        cleanup.callback(fixture.resume.set)
        operations = fixture.root / 'credential-operations'
        fixture.env.update(
            GIT_CONFIG_COUNT='1',
            GIT_CONFIG_KEY_0='credential.helper',
            GIT_CONFIG_VALUE_0=(
                f'!f() {{ echo "$1" >>{shlex.quote(str(operations))}; '
                f'if test "$1" = erase && grep -Fxq {REJECTION_INPUT}; then '
                f'echo challenge >>{shlex.quote(str(operations))}; fi; '
                "if git rev-parse --absolute-git-dir >/dev/null 2>&1; "
                "then echo username=fixture; else echo username=other-fixture; fi; "
                "echo password=example-password; }; f"
            ),
        )
        fixture.stalled_oids.add(fixture.other)
        gate = fixture.root / 'send-independent'
        independent = fixture.root / 'independent-done'
        code = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import time
objects = Path.cwd().parent / 'remote-repository' / 'objects'
assert objects.is_dir() and not any(path.is_file() for path in objects.rglob('*'))
with ThreadPoolExecutor(1) as pool:
    held = pool.submit(Path('other').read_bytes)
    while not Path({str(gate)!r}).exists():
        time.sleep(.01)
    assert Path('deep/file').read_bytes() == b'expected\\n'
    Path({str(independent)!r}).touch()
    assert held.result() == b'unrelated payload\\n'
"""
        backend_flag = '--backend=' + ('nfs' if OPTIONS.require_nfs else 'fuse')
        read_fd, write_fd = os.pipe()
        env = dict(fixture.env, GIT_ON_TEST_METRICS_FD=str(write_fd))
        with io.open(read_fd, 'rb') as metrics:
            try:
                process = subprocess.Popen(
                    [OPTIONS.binary, url,
                     'main', backend_flag, '--write-policy=none', '--', sys.executable, '-I', '-S', '-c', code],
                    cwd=fixture.cwd, env=env, pass_fds=(write_fd,), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                fixture.children.append(process)
            finally:
                os.close(write_fd)
            try:
                deadline = time.monotonic() + 15
                while not fixture.stalled.wait(.02) and process.poll() is None and time.monotonic() < deadline:
                    self.assertNotIn(
                        fixture.other.encode(), proxy.unauthenticated_wants,
                        'the HTTP/2 exact fetch was rejected with HTTP 401',
                    )
                if not fixture.stalled.is_set() and process.poll() is not None:
                    _, errors = process.communicate()
                    self.fail(f'the first fetch failed before reaching the server: {errors!r}')
                self.assertTrue(fixture.stalled.is_set(), 'the first fetch never reached the server')
                gate.touch()
                deadline = time.monotonic() + 8
                while not independent.exists() and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertTrue(independent.exists(), 'another fetch waited for the first stream')
            finally:
                fixture.resume.set()
            _, errors = process.communicate(timeout=15)
            self.assertEqual(process.returncode, 0, errors)
            report = json.load(metrics)
        self.assertFalse(proxy.unexpected_credential)
        self.assertEqual(proxy.unauthenticated_wants, {fixture.commit.encode()})
        self.assertEqual(report['remote_native_pack_bytes'], 0, report)
        self.assertEqual(report['remote_http2_fetch_operations'], 5, report)
        self.assertGreaterEqual(report['remote_http2_peak_requests'], 2, report)
        connections = {
            connection for connection, wants in proxy.records
            if set(wants) & {oid.encode() for oid in (
                fixture.commit, fixture.tree, fixture.other, fixture.blob
            )}
        }
        self.assertEqual(len(connections), 1, proxy.records)
        self.assertEqual(
            Counter(oid for request in fixture.requests for oid in request['wants']),
            Counter([fixture.commit, fixture.tree, fixture.subtree, fixture.blob, fixture.other]),
        )
        with self.subTest(credentials='approved'):
            self.assertEqual(operations.read_text().splitlines(), ['get', 'store'])

        operations.write_text('')
        env = dict(fixture.env, GIT_CONFIG_VALUE_0=fixture.env['GIT_CONFIG_VALUE_0'].replace(
            'password=example-password', 'password=declined-by-fixture',
        ))
        denied = subprocess.run(
            [OPTIONS.binary, url,
             'main', backend_flag, '--write-policy=none', '--', 'echo', 'command-ran'],
            cwd=fixture.cwd, env=env, capture_output=True, timeout=15,
        )
        with self.subTest(credentials='rejected'):
            self.assertEqual(denied.returncode, 125, denied.stderr)
            self.assertEqual(denied.stdout, b'')
            self.assertIn(b'Git HTTPS authentication failed', denied.stderr)
            self.assertEqual(operations.read_text().splitlines(), ['get', 'erase', 'challenge'])

        for key, wants_only in ((f'credential.{url}.helper', True), ('credential.helper', False)):
            with self.subTest(credential=key, wants_only=wants_only):
                operations.write_text('')
                proxy.authenticate_wants_only = wants_only
                proxy.unauthenticated_wants.clear()
                result = subprocess.run(
                    [OPTIONS.binary, url, 'main', backend_flag, '--write-policy=none', '--', 'cat', 'deep/file'],
                    cwd=fixture.cwd, env=dict(fixture.env, GIT_CONFIG_KEY_0=key),
                    capture_output=True, timeout=15,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'expected\n')
                self.assertIn('store', operations.read_text().splitlines())
                if not wants_only:
                    self.assertFalse(proxy.unauthenticated_wants)

        proxy.authenticate_wants_only = True
        hook_dir = fixture.root / 'branch-hooks'
        hook_dir.mkdir()
        hook = hook_dir / 'reference-transaction'
        hook.write_text(
            '#!/bin/sh\n'
            'if test "$1" = prepared && grep -q " refs/remotes/origin/main$"; then\n'
            '  echo "fixture refused tracking ref" >&2\n'
            '  exit 1\n'
            'fi\n'
        )
        hook.chmod(0o700)
        env = dict(
            fixture.env, GIT_CONFIG_COUNT='4', GIT_CONFIG_KEY_0=f'credential.{url}.helper',
            GIT_CONFIG_KEY_1='remote.origin.url', GIT_CONFIG_VALUE_1=url,
            GIT_CONFIG_KEY_2='remote.origin.fetch',
            GIT_CONFIG_VALUE_2='+refs/heads/*:refs/remotes/origin/*',
            GIT_CONFIG_KEY_3='core.hooksPath', GIT_CONFIG_VALUE_3=str(hook_dir),
        )
        result = subprocess.run(
            [OPTIONS.binary, 'origin', 'main', backend_flag, '--write-policy=none',
             '--', 'echo', 'command-ran'],
            cwd=fixture.cwd, env=env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 125, result)
        self.assertEqual(result.stdout, b'')
        self.assertIn(b'fixture refused tracking ref', result.stderr)


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0], *ARGS])
