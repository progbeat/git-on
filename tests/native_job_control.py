#!/usr/bin/env python3
"""Real SSH/HTTPS authentication and interactive-shell job-control regressions.

Run with --binary /path/to/git-on; no FUSE is needed. SSH cases use an unsigned
commit that fails verification; HTTPS exercises explicit-all materialization.
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
import sys
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
        # After Ctrl-C, OpenSSH can still be restoring its passphrase reader.
        # Check the existing transport-lifetime contract before sending another
        # command, whose first byte that reader could otherwise consume.
        shell.wait_for(lambda: all(state(pid) in (None,'Z') for pid in self.transport_pids),
                       'owned authentication transport survived completion')
        shell.send(b'printf "native-job-status=%s\\n" "$?"\n')
        shell.expect(f'native-job-status={expected}\r\n'.encode())
        shell.expect(shell.prompt)
        self.assertEqual(os.tcgetpgrp(shell.fd), shell.pid)
        self.assertEqual(list(self.f.root.glob('git-on-*')), [], 'runner leaked recovery state')
        self.assertEqual(list(self.f.root.glob('*/ssh-control')), [], 'runner leaked SSH socket')
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


class HttpsCredentialTerminalTests(unittest.TestCase):
    def test_repeated_tty_credentials_keep_foreground_and_finish_materialization(self):
        import base64
        f=remote.Fixture(); self.addCleanup(f.close)
        # Fork the shell before starting the fixture's HTTPS server thread.
        shell=Shell(f)
        def close_shell():
            try: shell.close()
            finally:
                if hasattr(f,'server'): f.server.shutdown()
        self.addCleanup(close_shell)
        url=f.https(authorization='Basic '+base64.b64encode(b'fixture:fixture-token').decode())
        helper=f.root/'credential-helper'
        pidfile=f.root/'credential-pid'
        helper.write_text('#!'+sys.executable+'\n'+
                          'import os,sys\nfrom pathlib import Path\n'
                          'sys.stdin.read()\nif sys.argv[1]=="get":\n'
                          ' Path('+repr(str(pidfile))+').write_text(str(os.getpid()))\n'
                          ' fd=os.open("/dev/tty",os.O_RDWR)\n'
                          ' os.write(fd,b"https-helper-token: ")\n'
                          ' token=bytearray()\n'
                          ' while not token.endswith(b"\\n"): token.extend(os.read(fd,1))\n'
                          ' os.close(fd)\n'
                          ' sys.stdout.write("username=fixture\\npassword="+token.decode().strip()+"\\n")\n')
        helper.chmod(0o700)
        # Hide the FUSE helper, retaining actual HTTPS and an actual terminal.
        programs=f.root/'programs'; programs.mkdir()
        (programs/'git').symlink_to(shutil.which('git'))
        environment=dict(PATH=str(programs),GIT_CONFIG_COUNT='1',GIT_SSL_CAINFO=f.env['GIT_SSL_CAINFO'],
                         GIT_CONFIG_KEY_0='credential.helper',GIT_CONFIG_VALUE_0=str(helper))
        command=[shutil.which('env'),*[key+'='+value for key,value in environment.items()],
                 str(remote.OPTIONS.binary),url,'HEAD','--write-policy=all','--',
                 sys.executable,'-I','-S','-c',
                 "from pathlib import Path; assert Path('deep/file').read_bytes()==b'expected\\n'; print('https-helper-command-ok')"]
        shell.send((shlex.join(command)+'\n').encode())
        prompts=0; deadline=time.monotonic()+20
        while b'\r\nhttps-helper-command-ok\r\n' not in shell.pending:
            self.assertLess(time.monotonic(),deadline,'HTTPS credential flow did not finish: '+repr(bytes(shell.transcript)))
            if b'https-helper-token: ' in shell.pending:
                shell.expect(b'https-helper-token: ')
                runner,=children(shell.pid)
                helper_pid=int(pidfile.read_text())
                group=os.getpgid(helper_pid)
                shell.groups={runner,group}
                if os.tcgetpgrp(shell.fd)!=group:
                    shell.wait_for(lambda: state(helper_pid)=='T','background helper did not stop on tty input')
                self.assertEqual(os.tcgetpgrp(shell.fd),group,
                                 'credential helper is in a background group; tty input would stop it')
                shell.send(b'fixture-token\n'); prompts+=1
            else: shell.read()
        self.assertGreaterEqual(prompts,2,'fixture did not exercise authentication after initial fetch')
        shell.expect(shell.prompt)
        shell.groups.clear()
        shell.send(b'printf "https-helper-status=%s\\n" "$?"\n')
        shell.expect(b'https-helper-status=0\r\n'); shell.expect(shell.prompt)
        self.assertEqual(os.tcgetpgrp(shell.fd),shell.pid)
        self.assertEqual(list(f.root.glob('git-on-*')),[],'HTTPS run leaked temporary storage')


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
