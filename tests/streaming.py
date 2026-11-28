"""Transport-neutral prefetch and publication on either real native mount."""
from contextlib import ExitStack
import hashlib
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import zlib

from http2 import ARGS, OPTIONS, Fixture
from support.run_storage import run_directories


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


def ssh_upload_pack(root, exit_status=0):
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
                if actual == wants and exit_status:
                    return exit_status
        finally:
            git.stdin.close()


def ssh_branch_pack(root):
    repository = root / 'server.git'
    assert shlex.split(os.environ['SSH_ORIGINAL_COMMAND']) == ['git-upload-pack', str(repository)]
    command = ['git', 'upload-pack', str(repository)]
    requested = root / 'branch-query'
    released = root / 'release-branch-query'
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as git:
        try:
            for packet in packets(git.stdout):
                sys.stdout.buffer.write(packet)
            sys.stdout.buffer.flush()
            while request := b''.join(packets(sys.stdin.buffer)):
                if b'command=ls-refs\n' in request:
                    requested.touch()
                    assert await_file(released, 12)
                git.stdin.write(request)
                git.stdin.flush()
                for packet in packets(git.stdout):
                    sys.stdout.buffer.write(packet)
                sys.stdout.buffer.flush()
        finally:
            git.stdin.close()


class SplitPack:
    def __init__(self, root, wanted, contents, base_oid, base_data, bad_trailer=False):
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
        if bad_trailer:
            # Keep a second want pending so the process waits for the error.
            entries = entries[:1]
        header = b'PACK' + struct.pack('>II', 2, len(entries))
        pack = header + b''.join(entries)
        # Open one sideband pkt-line, then pause it immediately after the
        # first blob. Several objects and the PACK trailer are still unsent.
        self.prefix = b'000dpackfile\n' + f'{len(pack) + 25:04x}'.encode() + b'\1' + header + entries[0]
        checksum = bytearray(hashlib.sha1(pack).digest())
        if bad_trailer:
            checksum[0] ^= 1
        self.tail = b''.join(entries[1:]) + bytes(checksum) + b'0000'


def fixture_in_short_tmp():
    # OpenSSH's Unix socket can outgrow macOS's default TMPDIR.
    with patch.object(tempfile, 'tempdir', '/tmp'):
        return Fixture()


def mount_flag():
    assert OPTIONS.require_fuse or OPTIONS.require_nfs
    return '--backend=' + ('nfs' if OPTIONS.require_nfs else 'fuse')


class StreamingTests(unittest.TestCase):
    def ssh_copy_fixture(self, exit_status=0):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        url = fixture.ssh()
        root = fixture.root
        pack = subprocess.check_output(
            ['git', '-C', fixture.repo, 'pack-objects', '--stdout', '--revs'],
            input=(fixture.commit + '\n').encode(), env=fixture.env,
        )
        (root / 'wants').write_text(fixture.commit + '\n')
        (root / 'prefix').write_bytes(
            b'000dpackfile\n' + f'{len(pack) + 5:04x}'.encode() + b'\x01' + pack[:12]
        )
        (root / 'tail').write_bytes(pack[12:] + b'0000')
        authorized = root / 'authorized'
        shim = shlex.join([sys.executable, str(Path(__file__).resolve()), '--ssh-pack',
                           str(root), str(exit_status)])
        authorized.write_text(f'command="{shim}" ' + authorized.read_text())
        return fixture, url

    def test_ssh_copy_requires_a_successful_transfer_exit(self):
        fixture, url = self.ssh_copy_fixture(exit_status=23)
        (fixture.root / 'release-pack').touch()
        result = subprocess.run(
            [OPTIONS.binary, url, fixture.commit, '--backend=copy', '--write-policy=all',
             '--', 'cat', 'deep/file'],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertEqual(result.stdout, b'')
        self.assertIn(b'OpenSSH upload-pack failed with exit status: 23', result.stderr)
        self.assertFalse(run_directories(fixture.root))

    def test_interrupted_ssh_copy_has_no_persistent_connection(self):
        fixture, url = self.ssh_copy_fixture()
        root = fixture.root
        diagnostics = root / 'copy-stderr'
        with diagnostics.open('wb') as errors:
            process = subprocess.Popen(
                [OPTIONS.binary, url, fixture.commit, '--backend=copy', '--write-policy=all', '--',
                 'touch', str(root / 'child-ran')],
                cwd=fixture.cwd, env=fixture.env, stdout=subprocess.DEVNULL, stderr=errors,
            )
        fixture.children.append(process)
        self.addCleanup((root / 'release-pack').touch)
        self.assertTrue(await_file(root / 'pack-started', 10, process),
                        diagnostics.read_text(errors='replace'))
        directory, = run_directories(root)
        socket = directory / 'ssh-control'
        persistent = socket.exists()
        # The previous implementation can leave its fixture connection open.
        # Close it before reporting a failed expectation.
        if persistent:
            self.addCleanup(lambda: subprocess.run(
                ['ssh', '-F', '/dev/null', '-S', str(socket), '-O', 'exit', 'git-on-control'],
                capture_output=True, timeout=5,
            ))
        process.kill()
        self.assertEqual(process.wait(timeout=5), -signal.SIGKILL)
        (root / 'release-pack').touch()
        cleanup = subprocess.run([OPTIONS.binary, '--cleanup', directory],
                                 capture_output=True, timeout=10)
        self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
        self.assertFalse(run_directories(root))
        self.assertFalse((root / 'child-ran').exists())
        self.assertFalse(persistent, 'SSH copy started an unnecessary persistent connection')

    def ssh_branch_fixture(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        url = fixture.ssh(protocol_v2=True)
        authorized = fixture.root / 'authorized'
        shim = shlex.join([sys.executable, str(Path(__file__).resolve()), '--ssh-branch', str(fixture.root)])
        authorized.write_text(f'command="{shim}" ' + authorized.read_text())
        return fixture, url

    def test_ssh_v0_server_is_rejected_before_launch(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        source = fixture.ssh(protocol_v2=False)
        result = subprocess.run(
            [OPTIONS.binary, source, 'main', mount_flag(), '--write-policy=none', '--', 'cat', 'deep/file'],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=10,
        )
        self.assertEqual(result.returncode, 125, result.stderr)
        self.assertEqual(result.stdout, b'')
        self.assertIn(b'Git protocol v2', result.stderr)

    def test_ssh_branch_respects_explicit_protocol_v2(self):
        fixture, url = self.ssh_branch_fixture()
        (fixture.root / 'release-branch-query').touch()
        fixture.env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='protocol.version', GIT_CONFIG_VALUE_0='2')
        result = subprocess.run(
            [OPTIONS.binary, url, 'main', mount_flag(), '--write-policy=none', '--', 'cat', 'deep/file'],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')
        self.assertTrue((fixture.root / 'branch-query').is_file())

    def test_directory_dfs_prefetch_is_separate_and_publishes_early(self):
        self.assertTrue(OPTIONS.require_fuse or OPTIONS.require_nfs)
        for transport, tail in [
            ('https', 'valid'), ('ssh', 'valid'), ('https', 'bad'), ('https', 'bad after exit'),
            ('https', 'unused after exit'),
        ]:
            bad_trailer = tail.startswith('bad')
            with self.subTest(transport=transport, tail=tail), ExitStack() as stack:
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
                split = SplitPack(fixture.root, oids[1:5], data[1:5], oids[-1], data[-1], bad_trailer)
                if transport == 'https':
                    proxy = fixture.https_server()
                    proxy.split_pack = split
                    url = proxy.url
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
if {tail == 'unused after exit'}:
    Path({str(ready)!r}).touch()
    raise SystemExit(0)
assert Path('b/first').read_bytes() == {data[1]!r}
assert Path('b/first').read_bytes() == {data[1]!r}
Path({str(ready)!r}).touch()
if {tail == 'bad after exit'}:
    raise SystemExit(0)
while not Path({str(split.release)!r}).exists():
    assert time.monotonic() < deadline, 'the test did not release the pack'
    time.sleep(.01)
try:
    assert Path('b/second').read_bytes() == {data[2]!r}
except OSError:
    assert {bad_trailer}
if not {bad_trailer}:
    assert Path('hidden/no').read_bytes() == {data[-1]!r}
"""
                process = subprocess.Popen(
                    [OPTIONS.binary, url, commit, mount_flag(), '--write-policy=none', '--',
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
                    if tail == 'bad after exit':
                        self.assertTrue(early)
                        with self.assertRaises(subprocess.TimeoutExpired):
                            process.wait(timeout=1.2)
                    elif tail == 'unused after exit':
                        self.assertTrue(early)
                        self.assertEqual(process.wait(timeout=4), 0)
                    elif not bad_trailer:
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
                self.assertTrue(early, f'the command waited for the rest of the pkt-line: {errors!r}')
                if tail == 'unused after exit':
                    continue
                if bad_trailer:
                    self.assertEqual(process.returncode, 125, errors)
                    self.assertIn(b'pack checksum', errors.lower())
                    continue
                self.assertEqual(process.returncode, 0, errors)
                wants = requests()
                if transport == 'ssh':
                    self.assertTrue(all(wants), 'exact SSH startup made a separate ref query')
                self.assertIn({oids[0].encode()}, wants)
                self.assertEqual(wants.count(split.wants), 1)
                self.assertIn({hidden.encode()}, wants)
                self.assertNotIn(oids[-1].encode(), set().union(*wants))

    def test_late_read_does_not_launch_a_prompt(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        server = fixture.https_server()
        ready, proceed, called = (fixture.root / name for name in ('ready', 'proceed', 'prompt'))
        askpass = fixture.root / 'askpass'
        askpass.write_text(f'#!/bin/sh\n: > {shlex.quote(str(called))}\nexit 1\n')
        askpass.chmod(0o700)
        code = f"""
from pathlib import Path
from time import monotonic, sleep
Path({str(ready)!r}).touch()
deadline = monotonic() + 8
while not Path({str(proceed)!r}).exists():
    assert monotonic() < deadline
    sleep(.01)
try:
    Path('other').read_bytes()
except OSError:
    pass
else:
    raise AssertionError('the server rejected this read')
"""
        process = subprocess.Popen(
            [OPTIONS.binary, server.url, fixture.commit, mount_flag(), '--write-policy=none',
             '--', sys.executable, '-I', '-S', '-c', code],
            cwd=fixture.cwd, env=dict(fixture.env, GIT_ASKPASS=str(askpass)),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        fixture.children.append(process)
        self.assertTrue(await_file(ready, 8, process))
        server.authorization = 'Basic ignored-by-this-fixture'
        proceed.touch()
        _, errors = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 125, errors)
        self.assertIn(fixture.other.encode(), server.unauthenticated_wants)
        self.assertFalse(called.exists())

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
        shim = fixture.root / 'slow-upload-pack'
        shim.write_text(f'#!/bin/sh\nsleep 0.4\nexec git-upload-pack {shlex.quote(str(fixture.repo))}\n')
        shim.chmod(0o700)
        authorized = fixture.root / 'authorized'
        authorized.write_text(f'command="{shlex.quote(str(shim))}" ' + authorized.read_text())
        script = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
contents = {contents!r}
assert sorted(path.name for path in Path('.').iterdir()) == list(contents)
with ThreadPoolExecutor(12) as pool:
    assert list(pool.map(lambda name: Path(name).read_bytes(), contents)) == list(contents.values())
"""
        result = subprocess.run(
            [OPTIONS.binary, url, 'main', mount_flag(), '--write-policy=none', '--',
             sys.executable, '-I', '-S', '-c', script],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=25,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((fixture.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def test_speculative_spill_does_not_block_an_independent_stream(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        size = 256 * 1024
        large = fixture.git('hash-object', '-w', '--stdin', data=b'x' * size)
        # NFS can admit more outstanding lookups than the former 16-stream
        # dispatcher. FUSE already serves at most eight callbacks at once.
        waiters = 16 if OPTIONS.require_nfs else 1
        priming = []
        for index in range(8):
            oid = fixture.git('hash-object', '-w', '--stdin', data=bytes([index + 1]) * size)
            priming.append(f'100644 blob {oid}\t{index}\n')
        prime = fixture.git('mktree', data=''.join(priming).encode())
        hidden = fixture.git('mktree', data=f'100644 blob {fixture.other}\tfile\n'.encode())
        waiting = []
        for index in range(waiters):
            oid = fixture.git('hash-object', '-w', '--stdin', data=bytes([65 + index]) * size)
            subtree = fixture.git('mktree', data=f'100644 blob {oid}\tfile\n'.encode())
            waiting.append(f'040000 tree {subtree}\twait-{index:02}\n')
        tree = fixture.git('mktree', data=(
            f'100644 blob {fixture.blob}\ta\n100644 blob {large}\tb\n'
            f'040000 tree {hidden}\thidden\n040000 tree {prime}\tprime\n' + ''.join(waiting)
        ).encode())
        commit = fixture.git('commit-tree', tree, data=b'large speculative blob\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        url = fixture.https()
        gate = fixture.root / 'spill-gate'
        gate.mkdir()
        read = fixture.root / 'independent-read'
        script = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from time import monotonic, sleep

def await_stage(path):
    deadline = monotonic() + 8
    while not path.exists():
        assert monotonic() < deadline
        sleep(.01)

def count_waiting():
    path = Path({str(gate / 'waiting')!r})
    return path.stat().st_size if path.exists() else 0

def exact_large_read(index):
    assert Path(f'wait-{{index:02}}/file').read_bytes() == bytes([65 + index]) * {size}

with ThreadPoolExecutor({waiters}) as executor:
    assert sorted(path.name for path in Path('.').iterdir()) == [
        'a', 'b', 'hidden', 'prime', *(f'wait-{{index:02}}' for index in range({waiters})),
    ]
    for index in range({waiters}):
        assert Path(f'wait-{{index:02}}').is_dir()
    assert Path('hidden').is_dir()
    for index in range(8):
        with open('prime/' + str(index), 'rb') as blob:
            assert blob.read(1) == bytes([index + 1])
    assert Path('a').read_bytes() == b'expected\\n'
    await_stage(Path({str(gate / 'started')!r}))
    waiting = []
    for index in range({waiters}):
        waiting.append(executor.submit(exact_large_read, index))
        deadline = monotonic() + 6
        while count_waiting() <= index:
            assert monotonic() < deadline, (index, count_waiting())
            sleep(.01)
    assert Path('hidden/file').read_bytes() == b'unrelated payload\\n'
    Path({str(read)!r}).touch()
    for result in waiting:
        result.result(timeout=10)
assert Path('b').stat().st_size == {size}
assert not list(Path.cwd().parent.rglob('*.pack'))
"""
        process = subprocess.Popen(
            [OPTIONS.binary, url, commit, mount_flag(), '--write-policy=none', '--',
             sys.executable, '-I', '-S', '-c', script],
            cwd=fixture.cwd, env=dict(fixture.env, GIT_ON_TEST_SPILL_GATE=str(gate)),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        fixture.children.append(process)
        try:
            started = await_file(gate / 'started', 8, process)
            independent = started and await_file(read, 8, process)
        finally:
            (gate / 'release').touch()
        _, errors = process.communicate(timeout=15)
        self.assertTrue(started, (errors, fixture.requests))
        self.assertTrue(independent, (errors, (gate / 'waiting').read_bytes() if (gate / 'waiting').exists() else b''))
        self.assertEqual(process.returncode, 0, errors)
        self.assertEqual(sum(large in request['wants'] for request in fixture.requests), 1)

    def test_remote_reads_retry_after_a_partial_spill_write_fails(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        entries = []
        for index in range(9):
            blob = fixture.git('hash-object', '-w', '--stdin', data=bytes([index]) * (128 * 1024))
            tree = fixture.git('mktree', data=f'100644 blob {blob}\tfile\n'.encode())
            entries.append(f'040000 tree {tree}\t{index}\n')
        root = fixture.git('mktree', data=''.join(entries).encode())
        commit = fixture.git('commit-tree', root, data=b'retry after eviction failure\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        gate = fixture.root / 'spill-gate'
        gate.mkdir()
        (gate / 'release').touch()
        (gate / 'fail').touch()
        code = """
import errno
import sys
from pathlib import Path

def read(index):
    assert Path(f'{index}/file').read_bytes() == bytes([index]) * (128 * 1024)

for index in range(8):
    read(index)
try:
    read(8)
except OSError as error:
    assert error.errno == errno.EIO, error
else:
    raise AssertionError('the first eviction should have failed')
Path(sys.argv[1]).unlink()
read(0)
read(8)
"""
        result = subprocess.run(
            [OPTIONS.binary, fixture.https(), commit, mount_flag(), '--write-policy=none',
             '--', sys.executable, '-I', '-S', '-c', code, str(gate / 'fail')],
            cwd=fixture.cwd, env=dict(fixture.env, GIT_ON_TEST_SPILL_GATE=str(gate)),
            capture_output=True, timeout=25,
        )
        self.assertEqual(result.returncode, 0, (result.stderr, fixture.requests))
        self.assertTrue((gate / 'started').exists())
        self.assertEqual(run_directories(fixture.root), [])


if __name__ == '__main__':
    if sys.argv[1:2] == ['--ssh-pack']:
        sys.exit(ssh_upload_pack(Path(sys.argv[2]), int(sys.argv[3]) if len(sys.argv) > 3 else 0))
    elif sys.argv[1:2] == ['--ssh-branch']:
        ssh_branch_pack(Path(sys.argv[2]))
    else:
        unittest.main(argv=[sys.argv[0], *ARGS])
