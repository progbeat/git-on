#!/usr/bin/env python3
"""Real OpenSSH authentication and interactive-shell job-control regressions.

Run with --binary /path/to/git-on; no FUSE is needed because the disposable
unsigned commit fails native verification after authentication.
"""
import contextlib
import importlib.util
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import signal
import time
import unittest

SPEC = importlib.util.spec_from_file_location(
    'git_on_ssh_regressions', Path(__file__).with_name('ssh_regressions.py'))
ssh_regressions = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ssh_regressions)
remote = ssh_regressions.remote


def children(pid):
    # /proc/PID/task/PID/children depends on CONFIG_CHECKPOINT_RESTORE; use
    # ordinary proc stat fields, which are also available on lean CI kernels.
    found = []
    for path in Path('/proc').iterdir():
        if not path.name.isdigit():
            continue
        try:
            parent = (path / 'stat').read_text().rpartition(')')[2].split()[1]
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        if parent == str(pid):
            found.append(int(path.name))
    return found


def state(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rpartition(')')[2].split()[0]
    except FileNotFoundError:
        return None


class Shell:
    prompt = b'git-on-job-shell> '

    def __init__(self, fixture):
        self.groups = set()
        self.pending = bytearray()
        self.transcript = bytearray()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            env = dict(fixture.env, PS1=self.prompt.decode(), PS2='continuation> ', TERM='dumb')
            remote.exec_fixture_child(
                [shutil.which('bash'), '--noprofile', '--norc', '-i'], fixture.cwd, env)
        self.expect(self.prompt)

    def send(self, data):
        os.write(self.fd, data)

    def read(self):
        if select.select([self.fd], [], [], .02)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                data = b''
            if not data:
                raise AssertionError('interactive shell ended: ' + repr(bytes(self.transcript)))
            self.pending.extend(data)
            self.transcript.extend(data)

    def expect(self, marker, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            end = self.pending.find(marker)
            if end >= 0:
                end += len(marker)
                value = bytes(self.pending[:end])
                del self.pending[:end]
                return value
            self.read()
        raise AssertionError('shell did not produce ' + repr(marker) + ': ' + repr(bytes(self.transcript)))

    def wait_for(self, predicate, message, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            self.read()
        raise AssertionError(message + ': ' + repr(bytes(self.transcript)))

    def close(self):
        # Native Git has its own process group, so killing the shell alone
        # cannot clean up a failed regression (including the old-head hang).
        for group in self.groups:
            for sig in (signal.SIGCONT, signal.SIGTERM):
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(group, sig)
        with contextlib.suppress(OSError):
            self.send(b'exit\n')
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            child, _ = os.waitpid(self.pid, os.WNOHANG)
            if child:
                break
            time.sleep(.01)
        else:
            for group in self.groups:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(group, signal.SIGKILL)
            with contextlib.suppress(ProcessLookupError):
                os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        os.close(self.fd)


class NativeJobControlTests(unittest.TestCase):
    def setUp(self):
        self.f = remote.Fixture()
        self.addCleanup(self.f.close)
        self.url = self.f.ssh(encrypted=True)
        # Exercise OpenSSH's actual controlling-terminal passphrase reader.
        for key in ('SSH_ASKPASS', 'SSH_ASKPASS_REQUIRE', 'DISPLAY'):
            self.f.env.pop(key, None)
        self.shell = Shell(self.f)
        self.addCleanup(self.shell.close)

    def begin_authentication(self):
        shell = self.shell
        argv = [str(remote.OPTIONS.binary), self.url, 'HEAD', '--verify', '--',
                '/bin/sh', '-c', 'echo native-job-command-was-run; exit 17']
        shell.send((shlex.join(argv) + '\n').encode())
        shell.expect(b"identity': ")
        runner, = children(shell.pid)
        git, = children(runner)
        self.runner, self.native_group = runner, os.getpgid(git)
        shell.groups.update((runner, self.native_group))
        self.assertEqual(os.getpgid(runner), runner)
        self.assertEqual(self.native_group, git)
        self.assertNotEqual(self.native_group, runner)
        self.assertEqual(os.getsid(git), shell.pid)
        self.assertEqual(os.tcgetpgrp(shell.fd), self.native_group)
        transports = children(git)
        self.assertTrue(transports, 'real native Git did not own an SSH transport')
        self.transport_pids = transports
        self.assertTrue(any(Path(f'/proc/{pid}/exe').resolve().name == 'ssh' for pid in transports))

    def stop_authentication(self):
        self.begin_authentication()
        self.suspend_authentication()

    def suspend_authentication(self):
        shell = self.shell
        shell.send(b'\x1a')
        # This assertion fails on e427: only Git/SSH stop, while git-on keeps
        # waiting and bash cannot display another prompt or accept `fg`.
        output = shell.expect(shell.prompt, timeout=5)
        self.assertIn(b'Stopped', output)
        self.assertEqual(state(self.runner), 'T')
        self.assertEqual(state(self.native_group), 'T')
        self.assertEqual(os.tcgetpgrp(shell.fd), shell.pid)

    def foreground(self):
        shell = self.shell
        shell.send(b'fg\n')
        shell.expect(b"identity': ")
        shell.wait_for(lambda: os.tcgetpgrp(shell.fd) == self.native_group,
                       'fg did not give native Git the terminal')
        self.assertNotEqual(state(self.runner), 'T')
        self.assertNotEqual(state(self.native_group), 'T')

    def assert_finished(self, expected):
        shell = self.shell
        output = shell.expect(shell.prompt)
        shell.send(b'printf "native-job-status=%s\\n" "$?"\n')
        shell.expect(f'native-job-status={expected}\r\n'.encode())
        shell.expect(shell.prompt)
        self.assertEqual(os.tcgetpgrp(shell.fd), shell.pid)
        self.assertEqual(list(self.f.root.glob('git-on-*')), [], 'runner leaked recovery state')
        self.assertEqual(list(self.f.root.glob('*/ssh-control')), [], 'runner leaked SSH socket')
        self.assertTrue(all(state(pid) in (None, 'Z') for pid in self.transport_pids),
                        'owned authentication transport survived completion')
        if expected == 125:
            self.assertIn(b'native Git verify-commit failed', output)
            self.assertEqual((self.f.root / 'sshd.log').read_text().count('Accepted publickey'), 1)
        # The command line is echoed by bash, so check for the executed output
        # on its own line rather than matching the submitted argv.
        self.assertNotIn(b'\r\nnative-job-command-was-run\r\n', shell.transcript)

    def test_ctrl_z_during_authentication_fg_resumes_and_cleans_up(self):
        self.stop_authentication()
        self.foreground()
        # Repeated suspension must preserve the same native group and permit
        # another fg, without leaving a stale consumed stop notification.
        self.suspend_authentication()
        self.foreground()
        self.shell.send(b'fixture-password\n')
        self.assert_finished(125)

    def test_ctrl_z_during_authentication_bg_then_fg_preserves_terminal(self):
        self.stop_authentication()
        shell = self.shell
        shell.send(b'bg\n')
        shell.expect(shell.prompt)
        shell.wait_for(lambda: state(self.runner) == 'T' and state(self.native_group) == 'T',
                       'background authentication did not stop on terminal input')
        self.assertEqual(os.tcgetpgrp(shell.fd), shell.pid, 'bg stole the shell terminal')
        self.foreground()
        shell.send(b'fixture-password\n')
        self.assert_finished(125)

    def test_ctrl_z_then_fg_authentication_interrupt_still_cleans_up(self):
        self.stop_authentication()
        self.foreground()
        self.shell.send(b'\x03')
        self.assert_finished(130)


def replay_baseline():
    """Confirm the original stopped-native/running-runner hang specifically.

    Setup, authentication, and group assertions must succeed before the red
    replay starts. An unrelated test/setup error never counts as reproduction.
    """
    case = NativeJobControlTests('test_ctrl_z_during_authentication_fg_resumes_and_cleans_up')
    try:
        case.setUp()
        case.begin_authentication()
        shell = case.shell
        shell.send(b'\x1a')
        shell.wait_for(lambda: state(case.native_group) == 'T',
                       'baseline native Git did not stop after terminal Ctrl-Z')
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            shell.read()
            case.assertNotIn(shell.prompt, shell.pending,
                             'baseline unexpectedly returned a shell prompt after Ctrl-Z')
            case.assertNotIn(state(case.runner), (None, 'T', 'Z'),
                             'baseline runner did not exhibit the running-runner defect')
            case.assertEqual(state(case.native_group), 'T')
            case.assertEqual(os.tcgetpgrp(shell.fd), case.native_group,
                             'baseline unexpectedly restored the shell terminal')
        print('Observed pre-fix Ctrl-Z hang: native Git stopped, git-on running, '
              'native group owns terminal, no shell prompt.', flush=True)
    finally:
        case.doCleanups()


if __name__ == '__main__':
    if remote.OPTIONS.before_binary is not None:
        current = remote.OPTIONS.binary
        remote.OPTIONS.binary = remote.OPTIONS.before_binary
        try:
            suite = unittest.TestSuite([unittest.FunctionTestCase(replay_baseline)])
            replay = unittest.TextTestRunner(verbosity=2).run(suite)
            if not replay.wasSuccessful():
                raise SystemExit(1)
        finally:
            remote.OPTIONS.binary = current
    unittest.main(argv=[__file__, *remote.ARGS])
