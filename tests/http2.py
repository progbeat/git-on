"""Exercise the HTTP/2 demand path on either real native mount."""
from concurrent.futures import ThreadPoolExecutor
import base64
from collections import Counter
import http.client
import io
import json
import os
import re
import socket
import socketserver
import ssl
import subprocess
import sys
import threading
import time
import unittest
from urllib.parse import urlsplit

from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import DataReceived, RequestReceived, StreamEnded, StreamReset

from remote import ARGS, OPTIONS, Fixture


class Multiplexer(socketserver.ThreadingTCPServer):
    daemon_threads = True

    def __init__(self, fixture, backend, authorization):
        self.backend = urlsplit(backend)
        self.authorization = authorization
        self.upstream_tls = ssl.create_default_context(cafile=fixture.env['GIT_SSL_CAINFO'])
        self.records = []
        self.split_pack = None
        self.unauthenticated_wants = set()
        self.unexpected_credential = False
        super().__init__(('127.0.0.1', 0), MultiplexedRequests)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(fixture.root / 'tls.crt', fixture.root / 'tls.key')
        tls.set_alpn_protocols(['h2', 'http/1.1'])
        self.socket = tls.wrap_socket(self.socket, server_side=True)

    def forward(self, method, path, headers, body):
        authorization = next((value for key, value in headers.items()
                              if key.lower() == 'authorization'), None)
        if method == 'POST' and self.authorization is not None and authorization != self.authorization:
            self.unauthenticated_wants.update(re.findall(rb'want ([0-9a-f]{40})', body))
            if authorization is not None:
                self.unexpected_credential = True
            return 401, [('www-authenticate', 'Basic realm="git-on-fixture"'), ('content-length', '0')], b''
        upstream = http.client.HTTPSConnection(
            self.backend.hostname, self.backend.port, context=self.upstream_tls, timeout=30,
        )
        try:
            upstream.request(method, path, body=body, headers=headers)
            response = upstream.getresponse()
            return response.status, response.getheaders(), response.read()
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
                for stream, (body, offset) in list(sending.items()):
                    offset = self.send_body(h2, stream, body, offset, end=True)
                    if offset == len(body):
                        sending.pop(stream)
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
                        offset = self.send_body(h2, stream, split.tail, position - prefix_end, end=True)
                        if offset == len(split.tail):
                            split_positions.pop(stream)
                        else:
                            split_positions[stream] = prefix_end + offset
                outgoing = h2.data_to_send()
                if outgoing:
                    # The short receive timeout polls upstream futures. A
                    # large TLS write can take longer under backpressure.
                    self.request.settimeout(5)
                    try:
                        self.request.sendall(outgoing)
                    finally:
                        self.request.settimeout(.02)

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
        self.request.sendall(f'HTTP/1.0 {status} Git\r\n{fields}\r\n'.encode() + body)


class Http2Tests(unittest.TestCase):
    def test_post_authentication_and_multiplexed_fetches(self):
        self.assertTrue(OPTIONS.require_nfs or OPTIONS.require_fuse)
        fixture = Fixture()
        self.addCleanup(fixture.close)
        token = base64.b64encode(b'fixture:example-password').decode()
        backend = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, backend, 'Basic ' + token)
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)
        self.addCleanup(fixture.resume.set)
        fixture.env.update(
            GIT_CONFIG_COUNT='1',
            GIT_CONFIG_KEY_0='credential.helper',
            GIT_CONFIG_VALUE_0=(
                "!f() { if git rev-parse --absolute-git-dir >/dev/null 2>&1; "
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
                    [OPTIONS.binary, f'https://127.0.0.1:{proxy.server_address[1]}/server.git',
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


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0], *ARGS])
