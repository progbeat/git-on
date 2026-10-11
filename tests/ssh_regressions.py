#!/usr/bin/env python3
"""Focused SSH review regressions using disposable real OpenSSH fixtures."""
import importlib.util
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import time
import unittest

SPEC = importlib.util.spec_from_file_location('git_on_remote_fixtures', Path(__file__).with_name('remote.py'))
remote = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(remote)


class SshRegressionTests(unittest.TestCase):
    def setUp(self):
        self.f = remote.Fixture()
        self.addCleanup(self.f.close)

    def loaded_read(self, url):
        f = self.f
        process = f.probe(url)
        f.line(process)
        data, _ = f.operation(process, 'read deep/file')
        self.assertEqual(data['data'], 'expected\n')
        f.operation(process, 'read other')
        f.finish(process)
        self.assertEqual((f.root / 'sshd.log').read_text().count('Accepted publickey'), 1)
        self.assertEqual(list(f.root.glob('*/ssh-control')), [])

    def test_unused_git_ssh_respects_command_and_configuration_precedence(self):
        f = self.f
        url = f.ssh()
        command = f.env['GIT_SSH_COMMAND']
        f.env['GIT_SSH'] = '/does/not/exist/git-on-ignored-ssh'
        oracle = subprocess.run(['git', 'ls-remote', url], cwd=f.cwd, env=f.env, capture_output=True, timeout=10)
        self.assertEqual(oracle.returncode, 0, oracle.stderr)
        for configured in (False, True):
            with self.subTest(configured=configured):
                if configured:
                    del f.env['GIT_SSH_COMMAND']
                    # Git's command-line configuration is higher priority than GIT_SSH.
                    f.env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='core.sshCommand', GIT_CONFIG_VALUE_0=command)
                process = f.probe(url)
                f.line(process)
                f.operation(process, 'read deep/file')
                f.finish(process)

    def test_transparent_git_ssh_executable_path_with_spaces(self):
        f = self.f
        url = f.ssh(encrypted=True)
        wrapper = f.root / 'forwarding ssh wrapper'
        wrapper.write_text('#!/bin/sh\nexec ' + f.env.pop('GIT_SSH_COMMAND') + ' "$@"\n')
        wrapper.chmod(0o700)
        f.env['GIT_SSH'] = str(wrapper)
        self.loaded_read(url)
        self.assertEqual((f.root / 'prompts').read_text().splitlines(), ['prompt'])

    def test_short_control_options_cannot_override_owned_socket_or_master(self):
        f = self.f
        url = f.ssh(encrypted=True)
        unrelated = f.root / 'unrelated-control'
        f.env['GIT_SSH_COMMAND'] += f' -vvM -S {unrelated} -o ControlPath={unrelated} -o ControlMaster=yes -o ControlPersist=1'
        process = f.probe(url)
        f.line(process)
        # The selected key and owned master must outlive caller's one-second idle setting.
        time.sleep(1.3)
        f.operation(process, 'read deep/file')
        f.operation(process, 'read other')
        self.assertFalse(unrelated.exists())
        self.assertEqual((f.root / 'prompts').read_text().splitlines(), ['prompt'])
        self.assertEqual((f.root / 'sshd.log').read_text().count('Accepted publickey'), 1)
        f.finish(process)
        self.assertEqual(list(f.root.glob('*/ssh-control')), [])

    def test_connection_loss_with_short_and_long_overrides_never_reconnects(self):
        f = self.f
        url = f.ssh(encrypted=True)
        f.env['GIT_SSH_COMMAND'] += f' -M -S {f.root}/other -o ControlMaster=auto -o BatchMode=no -o ProxyCommand=none'
        process = f.probe(url)
        f.line(process)
        sockets = list(f.root.glob('*/ssh-control'))
        self.assertEqual(len(sockets), 1)
        remote.run(['ssh', '-F', '/dev/null', '-S', sockets[0], '-O', 'exit', 'git-on-control'])
        process.stdin.write(b'read deep/file\n')
        process.stdin.flush()
        _, errors = process.communicate(timeout=10)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn(b'remote fetch of exact object', errors)
        self.assertEqual((f.root / 'prompts').read_text().splitlines(), ['prompt'])
        self.assertEqual((f.root / 'sshd.log').read_text().count('Accepted publickey'), 1)

    def test_short_proxy_jump_option_remains_usable_for_lazy_reads(self):
        f = self.f
        url = f.ssh(encrypted=True)
        # OpenSSH -J none is a native direct-host configuration. Inserting a
        # ProxyCommand=false fallback guard wrongly conflicts even with -J none.
        f.env['GIT_SSH_COMMAND'] += ' -J none'
        self.loaded_read(url)
        self.assertEqual((f.root / 'prompts').read_text().splitlines(), ['prompt'])

    def test_terminal_interrupt_during_authentication_propagates_sigint(self):
        f = self.f
        wrapper = f.root / 'hanging-ssh'
        pid_file = f.root / 'transport-pid'
        wrapper.write_text(f'#!/bin/sh\necho $$ > "{pid_file}"\nexec sleep 300\n')
        wrapper.chmod(0o700)
        f.env['GIT_SSH_COMMAND'] = str(wrapper)
        pid, descriptor = pty.fork()
        if pid == 0:
            os.chdir(f.cwd)
            binary = str(remote.OPTIONS.binary.resolve())
            os.execve(binary, [binary, 'ssh://fixture@example.invalid/repo', 'HEAD', '--', 'true'], f.env)
        reaped = False
        transport_group = None
        output = b''
        try:
            deadline = time.monotonic() + 5
            while not pid_file.exists() and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertTrue(pid_file.exists(), 'owned transport did not start')
            transport_group = os.getpgid(int(pid_file.read_text()))
            os.write(descriptor, b'\x03')
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                ready, _, _ = select.select([descriptor], [], [], .05)
                if ready:
                    try:
                        output += os.read(descriptor, 65536)
                    except OSError:
                        pass
                child, status = os.waitpid(pid, os.WNOHANG)
                if child:
                    reaped = True
                    break
            self.assertTrue(reaped, 'terminal interrupt did not stop runner: ' + repr(output))
            self.assertEqual(os.waitstatus_to_exitcode(status), -signal.SIGINT, output)
            self.assertEqual(list(f.root.glob('git-on-*')), [])
            transport = Path('/proc/' + pid_file.read_text().strip())
            self.assertTrue(not transport.exists() or (transport / 'stat').read_text().split()[2] == 'Z')
        finally:
            if not reaped:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            if transport_group is not None:
                try:
                    os.killpg(transport_group, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            os.close(descriptor)

    def test_unsupported_shell_and_variant_modes_fail_before_authentication(self):
        f = self.f
        url = f.ssh()
        original = f.env['GIT_SSH_COMMAND']
        for command in ('env EXAMPLE=1 ' + original, original + '; true'):
            with self.subTest(command=command):
                f.env['GIT_SSH_COMMAND'] = command
                process = f.probe(url)
                _, errors = process.communicate(timeout=10)
                self.assertNotEqual(process.returncode, 0)
                self.assertIn(b'unsupported', errors)
                self.assertNotIn('Accepted publickey', (f.root / 'sshd.log').read_text())
        f.env['GIT_SSH_COMMAND'] = original
        f.env['GIT_SSH_VARIANT'] = 'plink'
        process = f.probe(url)
        _, errors = process.communicate(timeout=10)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn(b'non-OpenSSH variants are unsupported', errors)


if __name__ == '__main__':
    unittest.main(argv=[__file__, *remote.ARGS])
