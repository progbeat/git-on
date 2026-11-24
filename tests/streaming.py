"""Transport-neutral prefetch and publication on either real native mount."""
import base64
from contextlib import ExitStack
import hashlib
import os
from pathlib import Path
from random import Random
import re
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
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
                    wait_for_eof = root / 'wait-for-request-eof'
                    if wait_for_eof.exists():
                        assert select.select([0], [], [], 4)[0], 'expected EOF before the pack trailer'
                        assert not os.read(0, 1), 'expected SSH request EOF'
                        (root / 'received-request-eof').touch()
                    else:
                        assert await_file(root / 'release-pack', 15)
                    sys.stdout.buffer.write((root / 'tail').read_bytes())
                    if wait_for_eof.exists():
                        sys.stdout.buffer.flush()
                        raise SystemExit(int(wait_for_eof.read_text()))
                else:
                    git.stdin.write(request)
                    git.stdin.flush()
                    for packet in packets(git.stdout):
                        sys.stdout.buffer.write(packet)
                sys.stdout.buffer.flush()
        finally:
            git.stdin.close()


def ssh_branch_pack(root):
    repository = root / 'server.git'
    assert shlex.split(os.environ['SSH_ORIGINAL_COMMAND']) == ['git-upload-pack', str(repository)]
    command = ['git', 'upload-pack', str(repository)]
    requested = root / 'branch-query'
    released = root / 'release-branch-query'
    if os.environ.get('GIT_PROTOCOL') == 'version=0':
        (root / 'legacy-advertised').touch()
        assert await_file(requested, 12)
        os.execvp('git', command)
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


def ssh_eof_pack(root, failed):
    repository = root / 'server.git'
    closing = set((root / 'closing').read_bytes().splitlines())
    channel = os.environ.get('GIT_ON_FIXTURE_CHANNEL')
    expire = root / 'expire-spares'
    startup = channel and not expire.exists()
    try:
        (root / 'initial-channel').mkdir()
    except FileExistsError:
        initial = False
    else:
        initial = True
        # Induce the normal slow-start path with negotiated spare channels.
        if startup:
            assert await_file(root / 'spare-advertised', 8)
        else:
            time.sleep(.35)
    with subprocess.Popen(['git', 'upload-pack', repository], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE) as git:
        try:
            for packet in packets(git.stdout):
                sys.stdout.buffer.write(packet)
            sys.stdout.buffer.flush()
            if startup:
                (root / f'advertised-{channel}').touch()
                if not initial:
                    (root / 'spare-advertised').touch()
            while True:
                if startup:
                    while not select.select([0], [], [], .02)[0]:
                        if expire.exists():
                            return 0
                request = b''.join(packets(sys.stdin.buffer))
                if not request:
                    break
                wants = re.findall(rb'want ([0-9a-f]{40})', request)
                wanted = wants[0]
                if channel:
                    (root / f'want-{wanted.decode()}-{channel}').touch()
                time.sleep(.1)
                if wanted in closing and select.select([0], [], [], 1.2)[0]:
                    assert not os.read(0, 1), 'extra input on a terminal request'
                    (root / wanted.decode()).touch()
                git.stdin.write(request)
                git.stdin.flush()
                for packet in packets(git.stdout):
                    sys.stdout.buffer.write(packet)
                sys.stdout.buffer.flush()
                if wanted == failed:
                    return 49
        finally:
            git.stdin.close()
    return 0


def read_after_ssh_expiry(root):
    deadline = time.monotonic() + 8
    while len(channels := list(root.glob('advertised-*'))) < 2:
        assert time.monotonic() < deadline, 'both startup channels must advertise'
        time.sleep(.02)
    (root / 'expire-spares').touch()
    # The local OpenSSH clients have consumed the server's normal exit. A
    # zombie is also eligible for git-on's non-reaping child check.
    pids = ','.join(path.name.split('-')[1] for path in channels)
    while True:
        status = subprocess.run(['ps', '-o', 'stat=', '-p', pids], capture_output=True, text=True)
        assert status.returncode in (0, 1) and not status.stderr, status.stderr
        states = status.stdout.split()
        if all(state.startswith('Z') for state in states):
            break
        assert time.monotonic() < deadline, (pids, states)
        time.sleep(.02)
    os.execvp('cat', ['cat', 'deep/file'])


def pack_entry(kind, data):
    size = len(data)
    header = bytearray([(kind << 4) | (size & 15)])
    size >>= 4
    while size:
        header[-1] |= 128
        header.append(size & 127)
        size >>= 7
    return bytes(header) + zlib.compress(data)


def split_pack_packets(head, tail):
    chunks = [head[index:index+60000] for index in range(0, len(head), 60000)]
    framed = b''.join(f'{len(chunk)+5:04x}'.encode() + b'\1' + chunk for chunk in chunks[:-1])
    last = chunks[-1]
    assert len(last) + len(tail) + 5 <= 65520
    return framed + f'{len(last)+len(tail)+5:04x}'.encode() + b'\1' + last, tail + b'0000'


class SplitPack:
    def __init__(self, root, wanted, contents, base_oid, base_data, hold_trailer=False):
        self.wants = {oid.encode() for oid in wanted}
        self.started = root / 'pack-started'
        self.release = root / 'release-pack'
        # Encode the second blob as a REF_DELTA. Its base is a file in a
        # directory the command will only visit after consuming the forecast.
        entries = [pack_entry(3, data) for data in [*contents, base_data]]
        delta = bytes([len(base_data), len(contents[1]), len(contents[1])]) + contents[1]
        assert len(delta) < 16
        entries[1] = bytes([0x70 | len(delta)]) + bytes.fromhex(base_oid) + zlib.compress(delta)
        header = b'PACK' + struct.pack('>II', 2, len(entries))
        pack = header + b''.join(entries)
        # Open one sideband pkt-line, then pause it immediately after the
        # first blob. Several objects and the PACK trailer are still unsent.
        head = pack if hold_trailer else header + entries[0]
        tail = (b'' if hold_trailer else b''.join(entries[1:])) + hashlib.sha1(pack).digest()
        prefix, self.tail = split_pack_packets(head, tail)
        self.prefix = b'000dpackfile\n' + prefix


def fixture_in_short_tmp():
    # OpenSSH's Unix socket can outgrow macOS's default TMPDIR.
    with patch.object(tempfile, 'tempdir', '/tmp'):
        return Fixture()


def mount_flag():
    assert OPTIONS.require_fuse or OPTIONS.require_nfs
    return '--backend=' + ('nfs' if OPTIONS.require_nfs else 'fuse')


class StreamingTests(unittest.TestCase):
    def sparse_ssh_fixture(self, failed=False, expire=False):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        url = fixture.ssh(protocol_v2=True, max_sessions=2 if expire else 5,
                          accept_env=('GIT_ON_FIXTURE_CHANNEL',) if expire else ())
        if expire:
            wrapper = fixture.root / 'ssh'
            wrapper.write_text(
                '#!/bin/sh\nexport GIT_ON_FIXTURE_CHANNEL=$$\n'
                'exec /usr/bin/ssh -o SendEnv=GIT_ON_FIXTURE_CHANNEL "$@"\n'
            )
            wrapper.chmod(0o700)
            fixture.env['GIT_SSH_COMMAND'] = str(wrapper) + fixture.env['GIT_SSH_COMMAND'][3:]
        closing = [fixture.subtree, fixture.blob]
        (fixture.root / 'closing').write_text('\n'.join(closing))
        shim = [sys.executable, str(Path(__file__).resolve()), '--ssh-eof',
                str(fixture.root), fixture.blob if failed else '']
        fixture.force_upload_pack('exec ' + shlex.join(shim) + '\n')
        return fixture, url, closing

    def run_sparse_ssh(self, fixture, url, command):
        return subprocess.run(
            [OPTIONS.binary, url, fixture.commit, mount_flag(), '--write-policy=none',
             '--', *command],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=15,
        )

    def test_sparse_ssh_closes_used_channels_while_they_send_packs(self):
        for failed in (False, True):
            with self.subTest(failed=failed):
                fixture, url, closing = self.sparse_ssh_fixture(failed)
                command = ['cat', 'deep/file'] if failed else ['cat', 'deep/file', 'other']
                result = self.run_sparse_ssh(fixture, url, command)
                self.assertEqual(result.stdout, b'expected\n' if failed else b'expected\nunrelated payload\n',
                                 result.stderr)
                self.assertEqual(result.returncode, 125 if failed else 0, result.stderr)
                if failed:
                    self.assertIn(b'SSH transport exited with exit status: 49', result.stderr)
                for oid in closing:
                    self.assertTrue((fixture.root / oid).is_file(),
                                    f'SSH stdin remained open while pack for {oid} was in flight')
                self.assertEqual((fixture.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def test_first_cold_ssh_directory_enumeration_keeps_channels_reusable(self):
        fixture, url, _ = self.sparse_ssh_fixture()
        result = self.run_sparse_ssh(fixture, url, ['ls', '-1', 'deep'])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'file\n')
        self.assertFalse((fixture.root / fixture.subtree).exists(), 'SSH stdin closed during readdir')

    def test_expired_ssh_standbys_are_replaced_through_the_same_master(self):
        fixture, url, _ = self.sparse_ssh_fixture(expire=True)
        result = self.run_sparse_ssh(fixture, url, [
            sys.executable, str(Path(__file__).resolve()), '--ssh-after-expiry', str(fixture.root),
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')
        expired = {path.name.rsplit('-', 1)[1] for path in fixture.root.glob('advertised-*')}
        [request] = fixture.root.glob(f'want-{fixture.subtree}-*')
        self.assertNotIn(request.name.rsplit('-', 1)[1], expired)
        self.assertEqual((fixture.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def ssh_branch_fixture(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        url = fixture.ssh(protocol_v2=True)
        authorized = fixture.root / 'authorized'
        shim = shlex.join([sys.executable, str(Path(__file__).resolve()), '--ssh-branch', str(fixture.root)])
        authorized.write_text(f'command="{shim}" ' + authorized.read_text())
        return fixture, url

    def test_ssh_branch_can_use_a_late_advertisement_while_ls_refs_is_held(self):
        fixture, url = self.ssh_branch_fixture()
        process = subprocess.Popen(
            [OPTIONS.binary, url, 'main', mount_flag(), '--write-policy=none', '--', 'cat', 'deep/file'],
            cwd=fixture.cwd, env=fixture.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        fixture.children.append(process)
        try:
            self.assertTrue(await_file(fixture.root / 'branch-query', 8, process))
            try:
                output, errors = process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                self.fail('the named revision waited for the held ls-refs reply')
        finally:
            (fixture.root / 'release-branch-query').touch()
        self.assertEqual(process.returncode, 0, errors)
        self.assertEqual(output, b'expected\n')
        self.assertEqual((fixture.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def test_ssh_branch_respects_explicit_protocol_v2(self):
        fixture, url = self.ssh_branch_fixture()
        (fixture.root / 'release-branch-query').touch()
        fixture.git('init', '--quiet', at=fixture.cwd)
        remotes = fixture.cwd / '.git' / 'remotes'
        remotes.mkdir()
        (remotes / 'fixture').write_text('URL: fixture-source\n')
        fixture.env.update(
            GIT_CONFIG_COUNT='3',
            GIT_CONFIG_KEY_0='protocol.version', GIT_CONFIG_VALUE_0='2',
            GIT_CONFIG_KEY_1=f'url.{url}.insteadOf', GIT_CONFIG_VALUE_1='fixture-source',
            GIT_CONFIG_KEY_2='url.ssh://127.0.0.1:1/unused.insteadOf', GIT_CONFIG_VALUE_2=url,
        )
        self.assertEqual(fixture.git('ls-remote', '--get-url', 'fixture', at=fixture.cwd), url)
        result = subprocess.run(
            [OPTIONS.binary, 'fixture', 'main', mount_flag(), '--write-policy=none', '--', 'cat', 'deep/file'],
            cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b'expected\n')
        self.assertTrue((fixture.root / 'branch-query').is_file())
        self.assertFalse((fixture.root / 'legacy-advertised').exists())

    def test_directory_dfs_prefetch_is_separate_and_publishes_early(self):
        self.assertTrue(OPTIONS.require_fuse or OPTIONS.require_nfs)
        for transport in ('https', 'ssh'):
            with self.subTest(transport=transport), ExitStack() as stack:
                fixture = fixture_in_short_tmp()
                stack.callback(fixture.close)
                names = ['a', 'b/first', 'b/second', 'd', 'e', 'f', 'g', 'h', 'i',
                         'j', 'k', 'hidden/no']
                data = [f'payload-{index}\n'.encode() for index, _ in enumerate(names)]
                data[1] = hashlib.shake_256(b'prefetched pack fixture').digest(2 * 1024 * 1024)
                first_digest = hashlib.sha256(data[1]).hexdigest()
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
                    proxy = Multiplexer(fixture, upstream, None)
                    proxy.split_pack = split
                    stack.callback(proxy.server_close)
                    threading.Thread(target=proxy.serve_forever, daemon=True).start()
                    stack.callback(proxy.shutdown)
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
import hashlib
import os
import time
os.listdir('.')
os.listdir('b')
assert Path('a').read_bytes() == {data[0]!r}
deadline = time.monotonic() + 12
while not Path({str(split.started)!r}).exists():
    assert time.monotonic() < deadline, 'prefetch did not start'
    time.sleep(.01)
assert hashlib.sha256(Path('b/first').read_bytes()).hexdigest() == {first_digest!r}
assert hashlib.sha256(Path('b/first').read_bytes()).hexdigest() == {first_digest!r}
assert not list(Path.cwd().parent.glob('raw-parts-*/*')), 'received pack was staged on disk'
assert not (Path.cwd().parent / 'remote-repository').exists(), 'streamed metadata used an on-disk Git repository'
Path({str(ready)!r}).touch()
while not Path({str(split.release)!r}).exists():
    assert time.monotonic() < deadline, 'the test did not release the pack'
    time.sleep(.01)
assert Path('b/second').read_bytes() == {data[2]!r}
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
                    log = fixture.root / 'ssh-wants'
                    return [set(line.split()) for line in log.read_bytes().splitlines()] if log.exists() else []

                try:
                    started = await_file(split.started, 12, process)
                    early = started and await_file(ready, 5, process)
                    refill = {oid.encode() for oid in oids[5:9]}
                    deadline = time.monotonic() + 5
                    while early and refill not in requests() and time.monotonic() < deadline:
                        time.sleep(.01)
                    before_release = requests()
                finally:
                    split.release.touch()
                _, errors = process.communicate(timeout=15)
                self.assertTrue(started, f'separate four-object prefetch did not reach the server: {errors!r}')
                self.assertTrue(early, f'the first file waited for the rest of its pkt-line: {errors!r}')
                self.assertEqual(process.returncode, 0, errors)
                self.assertIn(refill, before_release)
                fetched = set().union(*before_release)
                self.assertNotIn(hidden.encode(), fetched)
                self.assertFalse(fetched & {oid.encode() for oid in oids[9:-1]})
                wants = requests()
                if transport == 'ssh':
                    self.assertTrue(all(wants), 'exact SSH startup made a separate ref query')
                self.assertIn({oids[0].encode()}, wants)
                self.assertEqual(wants.count(split.wants), 1)
                self.assertIn({hidden.encode()}, wants)
                self.assertNotIn(oids[-1].encode(), set().union(*wants))

    def test_a_base_read_joins_the_pack_that_supplied_an_early_delta(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        upstream = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        basic = base64.b64encode(b'fixture:example-password').decode()
        proxy = Multiplexer(fixture, upstream, 'Basic ' + basic)
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)
        split = SplitPack(fixture.root, [fixture.blob], [b'out/\n', b'expected\n'],
                          fixture.other, b'unrelated payload\n', hold_trailer=True)
        proxy.split_pack = split
        self.addCleanup(split.release.touch)
        fixture.env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='credential.helper',
                           GIT_CONFIG_VALUE_0="!f() { echo username=fixture; echo password=example-password; }; f")
        url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        starting = fixture.root / 'starting-base'
        done = fixture.root / 'base-read'
        script = f"""
from pathlib import Path
assert Path('deep/file').read_bytes() == b'expected\\n'
Path({str(starting)!r}).touch()
assert Path('other').read_bytes() == b'unrelated payload\\n'
Path({str(done)!r}).touch()
"""
        process = subprocess.Popen(
            [OPTIONS.binary, url, fixture.commit, mount_flag(), '--write-policy=none',
             '--', sys.executable, '-I', '-S', '-c', script],
            cwd=fixture.cwd, env=fixture.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        fixture.children.append(process)
        try:
            early = await_file(starting, 8, process)
            read_before_trailer = early and await_file(done, .5, process)
        finally:
            split.release.touch()
        _, errors = process.communicate(timeout=15)
        self.assertTrue(early, f'the verified delta was not readable before the trailer: {errors!r}')
        self.assertFalse(read_before_trailer, 'the base was fetched independently of its held pack')
        self.assertEqual(process.returncode, 0, errors)
        self.assertNotIn(fixture.other.encode(), {oid for _, wants in proxy.records for oid in wants})

    def test_successful_child_exit_waits_for_the_pack_used_by_its_read(self):
        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        upstream = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, upstream, None)
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)
        url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'
        pack = subprocess.check_output(['git', '-C', fixture.repo, 'pack-objects', '--stdout'],
                                       input=(fixture.blob + '\n').encode(), env=fixture.env)
        prefix, tail = split_pack_packets(pack[:-20], pack[-20:])
        for interrupted in (False, True):
            with self.subTest(interrupted=interrupted):
                split = SimpleNamespace(wants={fixture.blob.encode()}, prefix=b'000dpackfile\n' + prefix,
                                        tail=tail, started=fixture.root / f'started-{interrupted}',
                                        release=fixture.root / f'release-{interrupted}')
                proxy.split_pack = split
                self.addCleanup(split.release.touch)
                pidfile = fixture.root / f'child-{interrupted}'
                script = f"""
import os
from pathlib import Path
assert Path('deep/file').read_bytes() == b'expected\\n'
pidfile = Path({str(pidfile)!r})
pending = pidfile.with_suffix('.new')
pending.write_text(str(os.getpid()))
pending.replace(pidfile)
"""
                process = subprocess.Popen(
                    [OPTIONS.binary, url, fixture.commit, mount_flag(), '--write-policy=none',
                     '--', sys.executable, '-I', '-S', '-c', script],
                    cwd=fixture.cwd, env=fixture.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                fixture.children.append(process)
                try:
                    self.assertTrue(await_file(pidfile, 8, process), 'the child did not complete its read')
                    pid = int(pidfile.read_text())
                    deadline = time.monotonic() + 5
                    while True:
                        try:
                            os.kill(pid, 0)
                        except ProcessLookupError:
                            break
                        self.assertLess(time.monotonic(), deadline, 'the child did not exit')
                        time.sleep(.01)
                    with self.assertRaises(subprocess.TimeoutExpired):
                        process.wait(timeout=.3)
                finally:
                    if interrupted:
                        split.tail = None
                    split.release.touch()
                _, errors = process.communicate(timeout=15)
                self.assertEqual(process.returncode, 125 if interrupted else 0, errors)
                if interrupted:
                    self.assertIn(b'execution incomplete', errors)

    def test_ssh_used_pack_can_finish_after_command_exit_sends_eof(self):
        for remote_exit in (0, 49):
            with self.subTest(remote_exit=remote_exit):
                fixture = fixture_in_short_tmp()
                try:
                    url = fixture.ssh(protocol_v2=True)
                    pack = subprocess.check_output(
                        ['git', '-C', fixture.repo, 'pack-objects', '--stdout'],
                        input=(fixture.blob + '\n').encode(), env=fixture.env,
                    )
                    prefix, tail = split_pack_packets(pack[:-20], pack[-20:])
                    (fixture.root / 'wants').write_text(fixture.blob + '\n')
                    (fixture.root / 'prefix').write_bytes(b'000dpackfile\n' + prefix)
                    (fixture.root / 'tail').write_bytes(tail)
                    (fixture.root / 'wait-for-request-eof').write_text(str(remote_exit))
                    shim = shlex.join([sys.executable, str(Path(__file__).resolve()),
                                       '--ssh-pack', str(fixture.root)])
                    authorized = fixture.root / 'authorized'
                    authorized.write_text(f'command="{shim}" ' + authorized.read_text())
                    command = 'import os; os.listdir("."); print(open("deep/file").read(), end="")'
                    result = subprocess.run(
                        [OPTIONS.binary, url, fixture.commit, mount_flag(),
                         '--write-policy=none', '--', sys.executable, '-I', '-S', '-c', command],
                        cwd=fixture.cwd, env=fixture.env, capture_output=True, timeout=18,
                    )
                    self.assertEqual(result.stdout, b'expected\n', result.stderr)
                    self.assertTrue((fixture.root / 'received-request-eof').exists(), result.stderr)
                    self.assertEqual(result.returncode, 125 if remote_exit else 0, result.stderr)
                    if remote_exit:
                        self.assertIn(b'SSH transport exited with exit status: 49', result.stderr)
                finally:
                    fixture.close()

    def test_remote_copy_up_before_pack_trailer_fits_two_checkouts(self):
        if not OPTIONS.require_fuse:
            self.skipTest('requires a real Linux FUSE mount and a private tmpfs quota')
        from filesystem_quota import compare_on_tmpfs

        fixture = fixture_in_short_tmp()
        self.addCleanup(fixture.close)
        data = Random(41).randbytes(3 * 1024 * 1024)
        blob = fixture.git('hash-object', '-w', '--stdin', data=data)
        entries = [f'100644 blob {blob}\tlarge\n']
        for index in range(9):
            oid = fixture.git('hash-object', '-w', '--stdin', data=bytes([index]) * (64 * 1024 + 1))
            entries.append(f'100644 blob {oid}\tz{index}\n')
        tree = fixture.git('mktree', data=''.join(entries).encode())
        commit = fixture.git('commit-tree', tree, data=b'copy before the trailer\n')
        fixture.git('update-ref', 'refs/heads/main', commit)
        pack = subprocess.check_output(['git', '-C', fixture.repo, 'pack-objects', '--stdout'],
                                       input=(blob + '\n').encode(), env=fixture.env)
        prefix, tail = split_pack_packets(pack[:-20], pack[-20:])
        split = SimpleNamespace(wants={blob.encode()}, prefix=b'000dpackfile\n' + prefix, tail=tail,
                                started=fixture.root / 'pack-started', release=fixture.root / 'release-pack')
        upstream = fixture.https()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, upstream, None)
        proxy.split_pack = split
        self.addCleanup(proxy.server_close)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        self.addCleanup(proxy.shutdown)
        self.addCleanup(split.release.touch)
        url = f'https://127.0.0.1:{proxy.server_address[1]}/server.git'

        script = f"""
import hashlib, os, time
from pathlib import Path
mounted = not Path('.git').exists()
with open('large', 'r+b', buffering=0) as file:
    assert file.read(128) == {data[:128]!r}
    for index in range(9):
        with open(f'z{{index}}', 'rb') as filler:
            assert filler.read(1) == bytes([index])
    if mounted:
        root = Path.cwd().parent
        deadline = time.monotonic() + 5
        while True:
            spilled = next(root.glob('raw-objects-*/{blob}'), None)
            if spilled and spilled.stat().st_size == {len(data)}:
                break
            assert time.monotonic() < deadline, 'source spill did not complete'
            time.sleep(.01)
    assert os.pwrite(file.fileno(), b'W', 0) == 1
    os.fsync(file.fileno())
    file.seek(0)
    assert hashlib.sha256(file.read()).hexdigest() == {hashlib.sha256(b'W' + data[1:]).hexdigest()!r}
    if mounted:
        assert os.path.ismount('.') and Path({str(split.started)!r}).exists()
        assert not list(root.glob('raw-parts-*/*')), 'compressed pack was staged'
        assert not (root / 'remote-repository').exists(), 'native Git pack storage was used'
        with spilled.open('rb') as source:
            assert source.read(128) == {data[:128]!r}
        release = Path({str(split.release)!r})
        assert not release.exists()
        release.touch()
print('updated private copy')
"""
        compare_on_tmpfs(self, fixture.root, OPTIONS.binary.resolve(), fixture.cwd, fixture.env['PATH'],
                         [sys.executable, '-I', '-S', '-c', script], size_mib=11, source=url,
                         checkout_copies=2, tls_ca=fixture.env['GIT_SSL_CAINFO'], revision=commit,
                         runner_size_mib=7)
        self.assertTrue(split.release.exists(), 'write did not finish before the trailer')

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



if __name__ == '__main__':
    if sys.argv[1:2] == ['--ssh-pack']:
        ssh_upload_pack(Path(sys.argv[2]))
    elif sys.argv[1:2] == ['--ssh-branch']:
        ssh_branch_pack(Path(sys.argv[2]))
    elif sys.argv[1:2] == ['--ssh-eof']:
        sys.exit(ssh_eof_pack(Path(sys.argv[2]), sys.argv[3].encode()))
    elif sys.argv[1:2] == ['--ssh-after-expiry']:
        read_after_ssh_expiry(Path(sys.argv[2]))
    else:
        unittest.main(argv=[sys.argv[0], *ARGS])
