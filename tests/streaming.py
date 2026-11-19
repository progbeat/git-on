"""Transport-neutral prefetch and publication on either real native mount."""
import base64
from contextlib import ExitStack
import hashlib
import os
from pathlib import Path
import re
import shlex
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import zlib

from http2 import ARGS, OPTIONS, Fixture, Multiplexer


def await_file(path, timeout, process=None):
    deadline = time.monotonic() + timeout
    while not path.exists() and time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            break
        time.sleep(.01)
    return path.exists()


def packets(stream):
    while header := stream.read(4):
        length = int(header, 16)
        yield header + stream.read(max(length - 4, 0))
        if length == 0:
            return


def ssh_upload_pack(root):
    """Proxy ordinary Git v2 exchanges and hold one synthetic pack mid-packet."""
    repository = root / 'server.git'
    assert shlex.split(os.environ['SSH_ORIGINAL_COMMAND']) == ['git-upload-pack', str(repository)]
    wants = set((root / 'wants').read_bytes().splitlines())
    with subprocess.Popen(['git', 'upload-pack', repository], stdin=subprocess.PIPE, stdout=subprocess.PIPE) as git:
        try:
            for packet in packets(git.stdout):
                sys.stdout.buffer.write(packet)
            sys.stdout.buffer.flush()
            while request := b''.join(packets(sys.stdin.buffer)):
                actual = set(re.findall(rb'want ([0-9a-f]{40})', request))
                with (root / 'ssh-wants').open('ab') as log:
                    log.write(b' '.join(sorted(actual)) + b'\n')
                if actual == wants:
                    sys.stdout.buffer.write((root / 'prefix').read_bytes())
                    sys.stdout.buffer.flush()
                    (root / 'pack-started').touch()
                    assert await_file(root / 'release-pack', 15)
                    sys.stdout.buffer.write((root / 'tail').read_bytes())
                else:
                    git.stdin.write(request)
                    git.stdin.flush()
                    for packet in packets(git.stdout):
                        sys.stdout.buffer.write(packet)
                sys.stdout.buffer.flush()
        finally:
            git.stdin.close()


class SplitPack:
    def __init__(self, root, wanted, contents, base_oid, base_data):
        self.wants = {oid.encode() for oid in wanted}
        self.started = root / 'pack-started'
        self.release = root / 'release-pack'
        # Encode the second blob as a REF_DELTA. Its base is a file in a
        # directory the command will only visit after consuming the forecast.
        entries = [bytes([0x30 | len(data)]) + zlib.compress(data)
                   for data in [*contents, base_data]]
        delta = bytes([len(base_data), len(contents[1]), len(contents[1])]) + contents[1]
        assert len(delta) < 16 and all(len(data) < 16 for data in [*contents, base_data])
        entries[1] = bytes([0x70 | len(delta)]) + bytes.fromhex(base_oid) + zlib.compress(delta)
        header = b'PACK' + struct.pack('>II', 2, len(entries))
        pack = header + b''.join(entries)
        # Open one sideband pkt-line, then pause it immediately after the
        # first blob. Several objects and the PACK trailer are still unsent.
        self.prefix = b'000dpackfile\n' + f'{len(pack) + 25:04x}'.encode() + b'\1' + header + entries[0]
        self.tail = b''.join(entries[1:]) + hashlib.sha1(pack).digest() + b'0000'


def fixture_in_short_tmp():
    # OpenSSH's Unix socket can outgrow macOS's default TMPDIR.
    with patch.object(tempfile, 'tempdir', '/tmp'):
        return Fixture()


def mount_flag():
    assert OPTIONS.require_fuse or OPTIONS.require_nfs
    return '--backend=' + ('nfs' if OPTIONS.require_nfs else 'fuse')


class StreamingTests(unittest.TestCase):
    def test_directory_dfs_prefetch_is_separate_and_publishes_early(self):
        self.assertTrue(OPTIONS.require_fuse or OPTIONS.require_nfs)
        for transport in ('https', 'ssh'):
            with self.subTest(transport=transport), ExitStack() as stack:
                fixture = fixture_in_short_tmp()
                stack.callback(fixture.close)
                names = ['a', 'b/first', 'b/second', 'd', 'e', 'f', 'g', 'h', 'i',
                         'j', 'k', 'hidden/no']
                data = [f'payload-{index}\n'.encode() for index, _ in enumerate(names)]
                oids = [fixture.git('hash-object', '-w', '--stdin', data=payload) for payload in data]
                b = fixture.git('mktree', data=f'100644 blob {oids[1]}\tfirst\n100644 blob {oids[2]}\tsecond\n'.encode())
                hidden = fixture.git('mktree', data=f'100644 blob {oids[-1]}\tno\n'.encode())
                files = ''.join(f'100644 blob {oid}\t{name}\n'
                                for name, oid in zip(names[3:-1], oids[3:-1]))
                root = fixture.git('mktree', data=(
                    f'100644 blob {oids[0]}\ta\n040000 tree {b}\tb\n'
                    f'{files}040000 tree {hidden}\thidden\n'
                ).encode())
                commit = fixture.git('commit-tree', root, data=b'separate prefetch\n')
                fixture.git('update-ref', 'refs/heads/main', commit)
                split = SplitPack(fixture.root, oids[1:5], data[1:5], oids[-1], data[-1])
                if transport == 'https':
                    upstream = fixture.https()
                    stack.callback(fixture.server.shutdown)
                    basic = base64.b64encode(b'fixture:example-password').decode()
                    proxy = Multiplexer(fixture, upstream, 'Basic ' + basic)
                    proxy.split_pack = split
                    stack.callback(proxy.server_close)
                    threading.Thread(target=proxy.serve_forever, daemon=True).start()
                    stack.callback(proxy.shutdown)
                    fixture.env.update(
                        GIT_CONFIG_COUNT='1',
                        GIT_CONFIG_KEY_0='credential.helper',
                        GIT_CONFIG_VALUE_0="!f() { echo username=fixture; echo password=example-password; }; f",
                    )
                    url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
                else:
                    url = fixture.ssh(protocol_v2=True)
                    (fixture.root / 'wants').write_bytes(b'\n'.join(sorted(split.wants)))
                    (fixture.root / 'prefix').write_bytes(split.prefix)
                    (fixture.root / 'tail').write_bytes(split.tail)
                    authorized = fixture.root / 'authorized'
                    shim = shlex.join([sys.executable, str(Path(__file__).resolve()), '--ssh-pack', str(fixture.root)])
                    authorized.write_text(f'command="{shim}" ' + authorized.read_text())
                stack.callback(split.release.touch)
                ready = fixture.root / 'first-read'
                script = f"""
from pathlib import Path
import os
import time
os.listdir('.')
os.listdir('b')
assert Path('a').read_bytes() == {data[0]!r}
deadline = time.monotonic() + 12
while not Path({str(split.started)!r}).exists():
    assert time.monotonic() < deadline, 'prefetch did not start'
    time.sleep(.01)
assert Path('b/first').read_bytes() == {data[1]!r}
assert Path('b/first').read_bytes() == {data[1]!r}
Path({str(ready)!r}).touch()
while not Path({str(split.release)!r}).exists():
    assert time.monotonic() < deadline, 'the test did not release the pack'
    time.sleep(.01)
assert Path('b/second').read_bytes() == {data[2]!r}
assert Path('hidden/no').read_bytes() == {data[-1]!r}
"""
                process = subprocess.Popen(
                    [OPTIONS.binary, url, 'HEAD', mount_flag(), '--write-policy=none', '--',
                     sys.executable, '-I', '-S', '-c', script],
                    cwd=fixture.cwd, env=fixture.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                fixture.children.append(process)
                def requests():
                    if transport == 'https':
                        return [set(group) for _, group in proxy.records]
                    return [set(line.split()) for line in (fixture.root / 'ssh-wants').read_bytes().splitlines()]

                try:
                    started = await_file(split.started, 12, process)
                    early = started and await_file(ready, 5, process)
                    refill = {oid.encode() for oid in oids[5:9]}
                    deadline = time.monotonic() + 5
                    while early and refill not in requests() and time.monotonic() < deadline:
                        time.sleep(.01)
                    self.assertIn(refill, requests())
                    fetched = set().union(*requests())
                    self.assertNotIn(hidden.encode(), fetched)
                    self.assertFalse(fetched & {oid.encode() for oid in oids[9:-1]})
                finally:
                    split.release.touch()
                _, errors = process.communicate(timeout=15)
                self.assertTrue(started, f'separate four-object prefetch did not reach the server: {errors!r}')
                self.assertTrue(early, f'the first file waited for the rest of its pkt-line: {errors!r}')
                self.assertEqual(process.returncode, 0, errors)
                wants = requests()
                self.assertIn({oids[0].encode()}, wants)
                self.assertEqual(wants.count(split.wants), 1)
                self.assertIn({hidden.encode()}, wants)
                self.assertNotIn(oids[-1].encode(), set().union(*wants))

    def test_ssh_channel_rejection_preserves_the_owned_master(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        contents = {f'file-{index:02}': f'payload {index}\n'.encode() for index in range(12)}
        rows = [
            f"100644 blob {fixture.git('hash-object', '-w', '--stdin', data=body)}\t{name}\n"
            for name, body in contents.items()
        ]
        tree = fixture.git('mktree', data=''.join(rows).encode())
        commit = fixture.git('commit-tree', tree, data=b'one SSH channel\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        url = fixture.ssh(protocol_v2=True, max_sessions=1)
        script = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
contents = {contents!r}
assert sorted(path.name for path in Path('.').iterdir()) == list(contents)
with ThreadPoolExecutor(12) as pool:
    assert list(pool.map(lambda name: Path(name).read_bytes(), contents)) == list(contents.values())
"""
        result = subprocess.run(
            [OPTIONS.binary, url, 'HEAD', mount_flag(), '--write-policy=none', '--',
             sys.executable, '-I', '-S', '-c', script],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=25,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((fixture.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def test_large_speculative_blob_uses_the_native_streaming_decoder(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        large = fixture.git('hash-object', '-w', '--stdin', data=b'x' * (9 * 1024 * 1024))
        tree = fixture.git('mktree', data=(
            f'100644 blob {fixture.blob}\ta\n100644 blob {large}\tb\n'
        ).encode())
        commit = fixture.git('commit-tree', tree, data=b'large speculative blob\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        url = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        trace = fixture.root / 'native.jsonl'
        fixture.env['GIT_TRACE2_EVENT'] = str(trace)
        script = f"""
import json, os, time
from pathlib import Path
assert sorted(os.listdir('.')) == ['a', 'b']
assert Path('a').read_bytes() == b'expected\\n'
trace = Path({str(trace)!r})
deadline = time.monotonic() + 8
while time.monotonic() < deadline:
    lines = trace.read_text().splitlines()
    events = []
    for line in lines:
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    starts = {{e['sid'] for e in events if e['event'] == 'start'
               and e['argv'][-3:] == ['cat-file', 'blob', {large!r}]}}
    if any(e['event'] == 'exit' and e['sid'] in starts and e['code'] == 0 for e in events):
        break
    time.sleep(.02)
else:
    raise AssertionError('the unused large blob did not use the native streaming decoder')
"""
        result = subprocess.run(
            [OPTIONS.binary, url, 'HEAD', mount_flag(), '--write-policy=none', '--',
             sys.executable, '-I', '-S', '-c', script],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sum(large in request['wants'] for request in fixture.requests), 1)


if __name__ == '__main__':
    if sys.argv[1:2] == ['--ssh-pack']:
        ssh_upload_pack(Path(sys.argv[2]))
    else:
        unittest.main(argv=[sys.argv[0], *ARGS])
