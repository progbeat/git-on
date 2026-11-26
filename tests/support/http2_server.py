"""TLS HTTP/2 Git fixture fronting the local git-http-backend."""

from concurrent.futures import ThreadPoolExecutor
import gzip
import http.client
import re
import socket
import socketserver
import ssl
import threading
from urllib.parse import urlsplit

from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import DataReceived, RequestReceived, StreamEnded, StreamReset

REJECTION_CHALLENGE = 'Basic realm="git-on-rejected"'


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
        super().__init__(('127.0.0.1', 0), MultiplexedRequests)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(fixture.root / 'tls.crt', fixture.root / 'tls.key')
        tls.set_alpn_protocols(['h2'])
        self.socket = tls.wrap_socket(self.socket, server_side=True)

    @property
    def url(self):
        return f'https://127.0.0.1:{self.server_address[1]}/server.git'

    def start(self, cleanup):
        cleanup(self.server_close)
        threading.Thread(target=self.serve_forever, daemon=True).start()
        cleanup(self.shutdown)

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
                        if split.tail is None:
                            return
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
