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
        # Exercise OpenSSH authentication through the runner's terminal UI.
        for key in ('SSH_ASKPASS', 'SSH_ASKPASS_REQUIRE', 'DISPLAY'):
            self.f.env.pop(key, None)
        self.shell = Shell(self.f)
        self.addCleanup(self.shell.close)

    def begin_authentication(self, native_terminal=False):
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
        self.assertEqual(os.getsid(git), shell.pid if native_terminal else git)
        self.assertEqual(os.tcgetpgrp(shell.fd), self.native_group if native_terminal else runner)
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
        shell.wait_for(lambda: state(self.native_group)=='T',
                       'native Git did not stop with the invoking job')
        self.assertEqual(os.tcgetpgrp(shell.fd), shell.pid)

    def foreground(self):
        shell = self.shell
        shell.send(b'fg\n')
        shell.expect(b"identity': ")
        shell.wait_for(lambda: os.tcgetpgrp(shell.fd) == self.runner,
                       'fg did not give git-on the terminal')
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

    def test_forced_askpass_failure_does_not_fall_back_to_terminal(self):
        import subprocess
        f=self.f;shell=self.shell
        tools=f.root/'eager-auth-tools';tools.mkdir()
        for name in ('git','ssh'):(tools/name).symlink_to(shutil.which(name))
        for helper in ('empty','default'):
            with self.subTest(helper=helper):
                env=dict(f.env,SSH_ASKPASS_REQUIRE='force')
                env.pop('SSH_ASKPASS',None)
                if helper=='empty':env['SSH_ASKPASS']=''
                native=subprocess.run(['git','ls-remote',self.url,'HEAD'],env=env,
                                      capture_output=True,timeout=15)
                self.assertNotEqual(native.returncode,0,native.stdout)
                for backend in (('lazy','materialized') if helper=='empty' else ('lazy',)):
                    runner_file=f.root/f'forced-askpass-{helper}-{backend}'
                    argv=[shutil.which('env'),'-u','SSH_ASKPASS','SSH_ASKPASS_REQUIRE=force']
                    if helper=='empty':argv.append('SSH_ASKPASS=')
                    if backend=='materialized':argv.append('PATH='+str(tools))
                    argv.extend(['/bin/sh','-c','printf "%s" "$$" > "$1"; shift; exec "$@"',
                                 'fixture',str(runner_file),str(remote.OPTIONS.binary),self.url,
                                 'HEAD','--verify',*(['--write-policy=all'] if backend=='materialized' else []),'--','/bin/true'])
                    shell.send((shlex.join(argv)+'; printf "ASKPASS-STATUS:%d\\n" "$?"\n').encode())
                    shell.wait_for(runner_file.exists,'runner did not start')
                    shell.groups.add(int(runner_file.read_text()))
                    try:output=shell.expect(shell.prompt,timeout=8)
                    except AssertionError:
                        with contextlib.suppress(ProcessLookupError):os.killpg(int(runner_file.read_text()),signal.SIGTERM)
                        shell.expect(shell.prompt)
                        shell.groups.clear()
                        raise
                    shell.groups.clear()
                    self.assertIn(b'\r\nASKPASS-STATUS:125\r\n',output)
                    self.assertNotIn(b"identity': ",output)
                    self.assertEqual(list(f.root.glob('git-on-*')),[])
                    if backend=='materialized':self.assertIn(b'Permission denied',output)


class HttpsCredentialTerminalTests(unittest.TestCase):
    def test_configured_askpass_path_with_spaces_matches_native_git(self):
        import base64
        f=remote.Fixture();self.addCleanup(f.close)
        shell=Shell(f);self.addCleanup(shell.close)
        url=f.https(authorization='Basic '+base64.b64encode(b'fixture:fixture-token').decode())
        self.addCleanup(f.server.shutdown)
        helper=f.root/'ask pass'
        helper.write_text('#!'+sys.executable+'\nimport os,sys\n'
                          'assert os.environ["GIT_ASKPASS"]==sys.argv[0]\n'
                          'assert "GIT_ON_TERMINAL_DIRECTORY" not in os.environ\n'
                          'print("fixture" if "Username" in sys.argv[1] else "fixture-token")\n')
        helper.chmod(0o700)
        environment=dict(GIT_ASKPASS=str(helper),GIT_TERMINAL_PROMPT='0',
                         GIT_SSL_CAINFO=f.env['GIT_SSL_CAINFO'])
        native=remote.run(['git','ls-remote',url,'HEAD'],env=dict(f.env,**environment))
        self.assertIn(f.commit.encode(),native)
        tools=f.root/'tools';tools.mkdir();(tools/'git').symlink_to(shutil.which('git'))
        environment['PATH']=str(tools)
        command=[shutil.which('env'),*[key+'='+value for key,value in environment.items()],
                 str(remote.OPTIONS.binary),url,'HEAD','--write-policy=all','--','/bin/true']
        shell.send((shlex.join(command)+'; printf "ASKPASS-STATUS:%d\\n" "$?"\n').encode())
        output=shell.expect(shell.prompt,timeout=20)
        self.assertIn(b'\r\nASKPASS-STATUS:0\r\n',output)
        self.assertEqual(list(f.root.glob('git-on-*')),[])

    def test_repeated_managed_credentials_keep_runner_in_foreground(self):
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
                          'import os,sys,subprocess\nfrom pathlib import Path\n'
                          'sys.stdin.read()\nif sys.argv[1]=="get":\n'
                          ' Path('+repr(str(pidfile))+').write_text(str(os.getpid()))\n'
                          ' try: fd=os.open("/dev/tty",os.O_RDWR)\n'
                          ' except OSError: pass\n'
                          ' else: raise AssertionError("helper owns a terminal")\n'
                          ' token=subprocess.check_output([os.environ["GIT_ASKPASS"],"https-helper-token: "])\n'
                          ' sys.stdout.write("username=fixture\\npassword="+token.decode().strip()+"\\n")\n')
        helper.chmod(0o700)
        # Lazy HTTPS preparation authenticates separate native Git processes.
        # Hide the FUSE helper so the test can stop before mounting.
        programs=f.root/'programs'; programs.mkdir()
        (programs/'git').symlink_to(shutil.which('git'))
        environment=dict(PATH=str(programs),GIT_CONFIG_COUNT='1',GIT_SSL_CAINFO=f.env['GIT_SSL_CAINFO'],
                         GIT_CONFIG_KEY_0='credential.helper',GIT_CONFIG_VALUE_0=str(helper))
        command=[shutil.which('env'),*[key+'='+value for key,value in environment.items()],
                 str(remote.OPTIONS.binary),url,'HEAD','--backend=fuse','--','/bin/false']
        shell.send((shlex.join(command)+'; printf "https-helper-status=%d\\n" "$?"\n').encode())
        native_groups=set(); deadline=time.monotonic()+20
        while shell.prompt not in shell.pending:
            self.assertLess(time.monotonic(),deadline,'HTTPS credential flow did not finish: '+repr(bytes(shell.transcript)))
            if b'https-helper-token: ' in shell.pending:
                shell.expect(b'https-helper-token: ')
                runner,=children(shell.pid)
                helper_pid=int(pidfile.read_text())
                group=os.getpgid(helper_pid)
                shell.groups={runner,group}
                self.assertEqual(os.tcgetpgrp(shell.fd),runner,
                                 'credential helper took the terminal from git-on')
                native_groups.add(group)
                shell.send(b'fixture-token\n')
            else: shell.read()
        output=shell.expect(shell.prompt)
        shell.groups.clear()
        self.assertIn(b'FUSE unavailable',output)
        self.assertIn(b'\r\nhttps-helper-status=125\r\n',output)
        self.assertGreaterEqual(len(native_groups),2,'expected credentials in separate native processes')
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
        case.begin_authentication(native_terminal=True)
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
