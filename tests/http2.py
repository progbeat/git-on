"""Exercise the HTTP/2 demand path on either real native mount."""
import base64
from collections import Counter
from contextlib import ExitStack
import errno
import io
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest
from types import SimpleNamespace


from remote import ARGS, OPTIONS, Fixture
from support.git_invocations import git_invocations, record_git_invocations
from support.http2_server import Multiplexer


class Http2Tests(unittest.TestCase):
    def https_fixture(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        backend = fixture.http1_upstream()
        self.addCleanup(fixture.server.shutdown)
        return fixture, backend

    def test_https_reads_credentials_and_streams_default_origin_without_a_repository(self):
        fixture, backend = self.https_fixture()
        token = base64.b64encode(b'fixture:example-password').decode()
        proxy = Multiplexer(fixture, backend, 'Basic ' + token)
        forward = proxy.forward

        def offer_bearer_then_basic(*args):
            status, headers, body = forward(*args)
            headers = [(name, 'Bearer realm="fixture", ' + value) if name == 'www-authenticate'
                       else (name, value) for name, value in headers]
            return status, headers, body

        proxy.forward = offer_bearer_then_basic
        proxy.start(self.addCleanup)
        url = proxy.url
        fixture.git('init', '--quiet', at=fixture.cwd)
        fixture.git('remote', 'add', 'origin', url, at=fixture.cwd)
        fixture.git('config', 'remote.backup.proxy', 'http://127.0.0.1:1', at=fixture.cwd)
        fixture.git('config', 'http.https://unrelated.invalid/.sslCAInfo',
                    'unused.pem', at=fixture.cwd)
        config = fixture.cwd / '.git/config'
        original_config = config.read_bytes()
        credentials = fixture.root / 'persistent-credentials'
        original_credentials = b'username=fixture\npassword=example-password\n'
        credentials.write_bytes(original_credentials)
        helper = fixture.root / 'read credential'
        quoted = shlex.quote(str(credentials))
        helper.write_text(
            '#!/bin/sh\ncase "$1" in\n'
            f'get) if test "$FIXTURE_DENIED" = 1; then '
            f'printf "%s\\n" username=fixture password=wrong; else cat {quoted}; fi ;;\n'
            f'store) printf changed > {quoted} ;;\n'
            f'erase) rm -f -- {quoted} ;;\n'
            'esac\n'
        )
        helper.chmod(0o700)
        environment, invocations = record_git_invocations(fixture.root, fixture.env)
        trace_file = fixture.root / 'origin-fetch-files'
        trace = (['strace', '-f', '-s', '512', '-o', trace_file, '-e', 'trace=%file']
                 if sys.platform == 'linux' and shutil.which('strace') else [])
        expected = fixture.tree.encode() + b'\nexpected\n'
        for source, denied in ((url, False), ('origin', False), (url, True)):
            with self.subTest(source=source, denied=denied), tempfile.TemporaryFile() as metrics:
                invocations.write_text('')
                fixture.requests.clear()
                credentials.write_bytes(original_credentials)
                env = dict(
                    environment,
                    GIT_CONFIG_COUNT='2',
                    GIT_CONFIG_KEY_0=f'credential.{url}.helper',
                    GIT_CONFIG_VALUE_0='!exec ' + shlex.quote(str(helper)),
                    GIT_CONFIG_KEY_1='credential.useHttpPath', GIT_CONFIG_VALUE_1='true',
                    HTTPS_PROXY='http://127.0.0.1:1', https_proxy='http://127.0.0.1:1',
                    GIT_TERMINAL_PROMPT='0', GIT_ASKPASS='/bin/false', FIXTURE_DENIED=str(int(denied)),
                    GIT_ON_TEST_METRICS_FD=str(metrics.fileno()),
                )
                run = subprocess.run(
                    [*trace, OPTIONS.binary, source, 'main', '--backend=copy', '--write-policy=all',
                     '--', 'sh', '-c', 'printf "%s\\n" "$GIT_ON_TREE_OID"; cat deep/file'],
                    cwd=fixture.cwd, env=env, capture_output=True, timeout=20, pass_fds=(metrics.fileno(),),
                )
                self.assertEqual(run.returncode, 125 if denied else 0, run.stderr)
                self.assertEqual(run.stdout, b'' if denied else expected)
                calls = git_invocations(invocations)
                self.assertEqual(sum(args[-2:] == ['credential', 'fill'] for args in calls), 1)
                self.assertFalse(any({'fetch', 'init', 'index-pack'} & set(args) for args in calls), calls)
                if trace:
                    files = trace_file.read_text().splitlines()
                    pack_paths = ('remote-repository', 'received-pack-', 'objects/pack',
                                  'pack-staging', 'FETCH_HEAD')
                    writes = [line for line in files if any(path in line for path in pack_paths)
                              or ('O_CREAT' in line and re.search(r'/(config|HEAD|[^"]+[.](pack|idx))"', line))]
                    self.assertEqual(writes, [], writes)
                self.assertEqual(credentials.read_bytes(), original_credentials)
                self.assertEqual(config.read_bytes(), original_config)
                if not denied:
                    metrics.seek(0)
                    report = json.load(metrics)
                    self.assertEqual(report['remote_repositories_created'], 0, report)
                    self.assertEqual(report['remote_http2_fetch_operations'], 1, report)
                    self.assertEqual([entry['wants'] for entry in fixture.requests if entry['wants']],
                                     [[fixture.commit]])

    def test_copy_rejects_http1(self):
        fixture, url = self.https_fixture()
        result = subprocess.run(
            [OPTIONS.binary, url, 'HEAD', '--backend=copy', '--write-policy=all', '--', 'false'],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertIn(b'requires HTTP/2', result.stderr)
        self.assertFalse(any(request['wants'] for request in fixture.requests), fixture.requests)

    def test_copy_materializes_one_shallow_pack_over_https_and_ssh(self):
        fixture, upstream = self.https_fixture()
        link = fixture.git('hash-object', '-w', '--stdin', data=b'deep/file')
        entries = fixture.git('ls-tree', fixture.tree).encode() + f'\n120000 blob {link}\tlink\n'.encode()
        tree = fixture.git('mktree', data=entries)
        commit = fixture.git('commit-tree', tree, data=b'eager copy fixture\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        fixture.git('config', 'uploadpack.allowFilter', 'false')
        server = Multiplexer(fixture, upstream, None)
        server.gzip_pack = True
        server.start(self.addCleanup)
        ssh = fixture.ssh(max_sessions=1)
        for url, revision in ((server.url, 'HEAD'), (ssh, 'refs/heads/main')):
            with self.subTest(url=url), tempfile.TemporaryFile() as metrics:
                env = dict(fixture.env, GIT_ON_TEST_METRICS_FD=str(metrics.fileno()))
                result = subprocess.run(
                    [OPTIONS.binary, url, revision, '--backend=copy', '--write-policy=all',
                     '--', 'sh', '-c', 'cat deep/file other; readlink link; printf "%s\n" "$GIT_ON_TREE_OID"'],
                    cwd=fixture.cwd, env=env, capture_output=True, timeout=20, pass_fds=(metrics.fileno(),),
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b'expected\nunrelated payload\ndeep/file\n' + tree.encode() + b'\n')
                metrics.seek(0)
                report = json.load(metrics)
                self.assertEqual(report['remote_session_fetch_operations'], 1, report)
                self.assertEqual(report['remote_fetch_operations'], 0, report)
                self.assertEqual(report['remote_repositories_created'], 0, report)
                self.assertEqual(report['remote_native_pack_bytes'], 0, report)
                self.assertEqual(report['spool_bytes'], 0, report)
        [request] = server.authorized_fetch_packets
        self.assertIn(b'deepen 1\n', request)
        self.assertNotIn(b'filter ', request)

    @unittest.skipUnless(OPTIONS.require_nfs, 'requires the macOS mount-to-copy fallback')
    def test_auto_copy_fallback_keeps_the_selected_snapshot(self):
        fixture, upstream = self.https_fixture()
        server = Multiplexer(fixture, upstream, None)
        forward = server.forward
        moved = fixture.git('commit-tree', fixture.subtree, '-p', fixture.commit, data=b'moved main\n')
        requests = []

        def move_ref_during_fetch(method, path, headers, body):
            if b'command=ls-refs' in body:
                requests.append(body)
            if f'want {fixture.tree}\n'.encode() in body:
                fixture.git('update-ref', 'refs/heads/main', moved, fixture.commit)
            return forward(method, path, headers, body)

        server.forward = move_ref_during_fetch
        server.start(self.addCleanup)
        socket_log = fixture.root / 'failed-nfs-endpoint'
        command = (
            "import os, sys; from pathlib import Path; "
            "socket = Path(os.fsdecode(Path(sys.argv[1]).read_bytes())); "
            "assert not socket.parent.exists(), socket; "
            "sys.stdout.buffer.write(Path('deep/file').read_bytes())"
        )
        result = subprocess.run(
            [OPTIONS.binary, server.url, 'main', '--backend=auto', '--write-policy=all',
             '--', sys.executable, '-c', command, socket_log],
            cwd=fixture.cwd, env=dict(fixture.env, GIT_ON_TEST_NFS_MOUNT_ERROR=str(socket_log)),
            capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')
        self.assertEqual(fixture.git('rev-parse', 'main'), moved)
        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(sum(b'filter tree:0\n' in packet for packet in server.authorized_fetch_packets), 2)
        [full_copy] = [packet for packet in server.authorized_fetch_packets if b'deepen 1\n' in packet
                       and b'filter ' not in packet]
        self.assertIn(f'want {fixture.commit}\n'.encode(), full_copy)

    def test_eager_https_shows_actual_pack_progress(self):
        fixture, backend = self.https_fixture()
        proxy = Multiplexer(fixture, backend, None)
        proxy.start(self.addCleanup)

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
        url = proxy.url
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
        self.assertRegex(transcript, rb'\d+/\d+')
        self.assertIn('━'.encode(), transcript)
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
        backend = fixture.http1_upstream()
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
                "echo username=fixture; "
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
assert not (Path.cwd().parent / 'remote-repository').exists()
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
            self.assertEqual(operations.read_text().splitlines(), ['get'])

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
            self.assertEqual(operations.read_text().splitlines(), ['get'])

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
                self.assertEqual(set(operations.read_text().splitlines()), {'get'})
                if not wants_only:
                    self.assertFalse(proxy.unauthenticated_wants)


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0], *ARGS])
