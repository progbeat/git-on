#!/usr/bin/env python3
"""Focused SSH review regressions using disposable real OpenSSH fixtures."""
import importlib.util
import contextlib
import json
import os
from pathlib import Path
import pty
import select
import re
import signal
import shlex
import shutil
import socket
import subprocess
import time
import unittest
import uuid

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

    def shutdown_selected_client_outside_path(self, selection, recovery):
        f = remote.Fixture()
        self.addCleanup(f.close)
        url = f.ssh()
        ssh = str(Path(shutil.which('ssh')).resolve())
        git = str(Path(shutil.which('git')).resolve())
        command = shlex.quote(ssh) + f.env.pop('GIT_SSH_COMMAND')[3:]
        if selection == 'GIT_SSH':
            executable = f.root / 'selected ssh wrapper'
            executable.write_text('#!/bin/sh\nexec ' + command + ' "$@"\n')
            executable.chmod(0o700)
            f.env['GIT_SSH'] = str(executable)
        elif selection == 'core.sshCommand':
            f.env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='core.sshCommand', GIT_CONFIG_VALUE_0=command)
        else:
            f.env['GIT_SSH_COMMAND'] = command
        programs = f.root / 'git-only-path'
        programs.mkdir()
        (programs / 'git').symlink_to(git)
        f.env['PATH'] = str(programs)
        self.assertIsNone(shutil.which('ssh', path=f.env['PATH']))
        process = f.probe(url)
        f.line(process)
        f.operation(process, 'read deep/file')
        sockets = list(f.root.glob('*/ssh-control'))
        self.assertEqual(len(sockets), 1)
        directory = sockets[0].parent
        checked = subprocess.run([ssh, '-F', '/dev/null', '-S', str(sockets[0]), '-O', 'check', 'git-on-control'],
                                 capture_output=True, check=True, timeout=5)
        pid = int(re.search(rb'pid=(\d+)', checked.stderr).group(1))
        descriptor = os.pidfd_open(pid)
        try:
            self.assertFalse(select.select([descriptor], [], [], 0)[0])
            if recovery:
                process.kill()
                process.communicate(timeout=10)
                self.assertEqual(process.returncode, -signal.SIGKILL)
                # This fixture exercises validated unmounted stale recovery;
                # actual SIGKILL mount behavior belongs to --require-fuse CI.
                (directory / '.lock').touch()
                (directory / '.state').write_text(json.dumps(dict(
                    format='git-on-temp-v1', id=str(uuid.uuid4()), keep=False,
                    git_dir=None, tree_oid=None, pin_ref=None)))
                cleanup = subprocess.run([str(remote.OPTIONS.binary.resolve()), '--cleanup', str(directory)],
                                         env=f.env, capture_output=True, timeout=10)
                self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
            else:
                f.finish(process)
            self.assertFalse(directory.exists())
            self.assertTrue(select.select([descriptor], [], [], 2)[0],
                            'owned master survived shutdown with the selected SSH client outside PATH')
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=10)
            if not select.select([descriptor], [], [], 0)[0]:
                with contextlib.suppress(ProcessLookupError):
                    signal.pidfd_send_signal(descriptor, signal.SIGTERM)
            os.close(descriptor)

    def test_selected_ssh_client_outside_path_closes_master_on_drop(self):
        for selection in ('GIT_SSH_COMMAND', 'GIT_SSH', 'core.sshCommand'):
            with self.subTest(selection=selection):
                self.shutdown_selected_client_outside_path(selection, False)

    def test_selected_ssh_client_outside_path_closes_master_on_stale_recovery(self):
        for selection in ('GIT_SSH_COMMAND', 'GIT_SSH', 'core.sshCommand'):
            with self.subTest(selection=selection):
                self.shutdown_selected_client_outside_path(selection, True)

    def test_changed_client_and_invalid_recovery_record_preserve_owned_connection(self):
        f = self.f
        url = f.ssh()
        ssh = str(Path(shutil.which('ssh')).resolve())
        command = shlex.quote(ssh) + f.env.pop('GIT_SSH_COMMAND')[3:]
        executable = f.root / 'selected ssh wrapper'
        executable.write_text('#!/bin/sh\nexec ' + command + ' "$@"\n')
        executable.chmod(0o700)
        f.env['GIT_SSH'] = str(executable)
        process = f.probe(url)
        f.line(process)
        directory = next(f.root.glob('*/ssh-control')).parent
        record_path = directory / 'ssh-control-owner'
        record = json.loads(record_path.read_text())
        self.assertNotIn(str(f.root / 'identity'), record_path.read_text())
        control = [ssh, '-F', '/dev/null', '-S', str(directory / 'ssh-control'), '-O', 'check', 'git-on-control']
        checked = subprocess.run(control, capture_output=True, check=True, timeout=5)
        descriptor = os.pidfd_open(int(re.search(rb'pid=(\d+)', checked.stderr).group(1)))
        backup = f.root / 'original selected client'
        executable.rename(backup)
        executable.write_text('#!/bin/sh\nexit 0\n')
        executable.chmod(0o700)
        try:
            process.stdin.write(b'exit\n'); process.stdin.flush()
            _, errors = process.communicate(timeout=10)
            self.assertNotEqual(process.returncode, 0)
            self.assertIn(b'probe recovery retained', errors)
            self.assertTrue(directory.exists())
            self.assertFalse(select.select([descriptor], [], [], 0)[0])
            pin = ['git', '--git-dir=' + str(directory / 'remote-repository'), 'show-ref', '--verify', 'refs/git-on/probe']
            self.assertEqual(subprocess.run(pin, capture_output=True, timeout=5).returncode, 0)
            (directory / '.lock').touch()
            (directory / '.state').write_text(json.dumps(dict(
                format='git-on-temp-v1', id='probe', keep=False,
                git_dir=str(directory / 'remote-repository'), tree_oid=f.tree, pin_ref='refs/git-on/probe')))
            executable.unlink(); backup.rename(executable)
            for invalid in ('relative', 'unknown-field', 'invalid-type', 'directory', 'symlink', 'in-place'):
                with self.subTest(record=invalid):
                    changed = json.loads(json.dumps(record))
                    if invalid == 'relative': changed['client']['path'] = list(b'ssh')
                    elif invalid == 'unknown-field': changed['client']['command'] = 'do not execute'
                    elif invalid == 'invalid-type': changed['client']['path'] = 'not byte provenance'
                    elif invalid == 'in-place':
                        original = executable.read_bytes()
                        metadata = executable.stat()
                        executable.write_bytes(original + b'# changed\n')
                    else:
                        executable.rename(backup)
                        if invalid == 'directory': executable.mkdir()
                        else: executable.symlink_to(backup)
                    record_path.write_text(json.dumps(changed))
                    cleanup = subprocess.run([str(remote.OPTIONS.binary.resolve()), '--cleanup', str(directory)],
                                             env=f.env, capture_output=True, timeout=10)
                    self.assertEqual(cleanup.returncode, 125, cleanup.stderr)
                    self.assertTrue(directory.exists())
                    self.assertFalse(select.select([descriptor], [], [], 0)[0])
                    self.assertEqual(subprocess.run(control, capture_output=True, timeout=5).returncode, 0)
                    self.assertEqual(subprocess.run(pin, capture_output=True, timeout=5).returncode, 0)
                    if invalid in ('directory', 'symlink'):
                        if invalid == 'directory': executable.rmdir()
                        else: executable.unlink()
                        backup.rename(executable)
                    elif invalid == 'in-place':
                        executable.write_bytes(original)
                        os.utime(executable, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
            record_path.write_text(json.dumps(record))
            cleanup = subprocess.run([str(remote.OPTIONS.binary.resolve()), '--cleanup', str(directory)],
                                     env=f.env, capture_output=True, timeout=10)
            self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
            self.assertFalse(directory.exists())
            self.assertTrue(select.select([descriptor], [], [], 2)[0])
        finally:
            if process.poll() is None: process.kill(); process.communicate(timeout=10)
            if not select.select([descriptor], [], [], 0)[0]:
                with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(descriptor, signal.SIGTERM)
            os.close(descriptor)

    def test_sigkilled_master_stale_socket_does_not_block_recovery(self):
        f = self.f
        process = f.probe(f.ssh())
        f.line(process)
        directory = next(f.root.glob('*/ssh-control')).parent
        checked = subprocess.run(['ssh', '-F', '/dev/null', '-S', str(directory / 'ssh-control'), '-O', 'check', 'git-on-control'],
                                 capture_output=True, check=True, timeout=5)
        descriptor = os.pidfd_open(int(re.search(rb'pid=(\d+)', checked.stderr).group(1)))
        try:
            signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            self.assertTrue(select.select([descriptor], [], [], 2)[0])
            self.assertTrue((directory / 'ssh-control').exists(), 'SIGKILL fixture did not leave its socket node')
            process.kill(); process.communicate(timeout=10)
            (directory / '.lock').touch()
            (directory / '.state').write_text(json.dumps(dict(
                format='git-on-temp-v1', id='probe', keep=False,
                git_dir=str(directory / 'remote-repository'), tree_oid=f.tree, pin_ref='refs/git-on/probe')))
            cleanup = subprocess.run([str(remote.OPTIONS.binary.resolve()), '--cleanup', str(directory)],
                                     env=f.env, capture_output=True, timeout=10)
            self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
            self.assertFalse(directory.exists())
        finally:
            if process.poll() is None: process.kill(); process.communicate(timeout=10)
            if not select.select([descriptor], [], [], 0)[0]:
                with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(descriptor, signal.SIGTERM)
            os.close(descriptor)

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
            binary = str(remote.OPTIONS.binary)
            remote.exec_fixture_child([binary, 'ssh://fixture@example.invalid/repo', 'HEAD', '--', 'true'], f.cwd, f.env)
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

    def test_stale_cleanup_never_follows_control_socket_symlink(self):
        f = self.f
        url = f.ssh()
        user_host, port_and_path = url.removeprefix('ssh://').split(':', 1)
        port = port_and_path.split('/', 1)[0]
        unrelated = f.root / 'unrelated-master'
        command = shlex.split(f.env['GIT_SSH_COMMAND']) + [
            '-p', port, '-M', '-S', str(unrelated), '-o', 'ControlPersist=yes', '-fN', user_host]
        remote.run(command, env=f.env)
        self.addCleanup(lambda: subprocess.run(
            ['ssh', '-F', '/dev/null', '-S', str(unrelated), '-O', 'exit', 'unused'],
            capture_output=True, timeout=5))
        check = ['ssh', '-F', '/dev/null', '-S', str(unrelated), '-O', 'check', 'unused']
        self.assertEqual(subprocess.run(check, capture_output=True, timeout=5).returncode, 0)
        stale = f.root / 'git-on-stale'
        stale.mkdir(mode=0o700)
        (stale / '.lock').touch()
        (stale / '.state').write_text(json.dumps(dict(
            format='git-on-temp-v1', id=str(uuid.uuid4()), keep=False,
            git_dir=None, tree_oid=None, pin_ref=None)))
        (stale / 'ssh-control').symlink_to(unrelated)
        cleanup = subprocess.run([str(remote.OPTIONS.binary.resolve()), '--cleanup', str(stale)],
                                 env=f.env, capture_output=True, timeout=5)
        self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
        self.assertFalse(stale.exists(), 'validated stale run was not removed')
        self.assertEqual(subprocess.run(check, capture_output=True, timeout=5).returncode, 0,
                         'stale cleanup stopped a connection outside its run directory')

    def test_native_config_fifo_cancellation_stops_git_and_removes_state(self):
        f = self.f
        fifo = f.root / 'git-config-fifo'
        os.mkfifo(fifo)
        f.env['GIT_CONFIG_GLOBAL'] = str(fifo)
        process = subprocess.Popen([
            str(remote.OPTIONS.binary.resolve()), 'file://' + str(f.repo), 'HEAD', '--', 'true'],
            cwd=f.cwd, env=f.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True)
        f.children.append(process)
        native_children = []
        try:
            deadline = time.monotonic() + 5
            while not list(f.root.glob('git-on-*/.state')) and process.poll() is None and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertTrue(list(f.root.glob('git-on-*/.state')), 'runner state was never created')
            deadline = time.monotonic() + 5
            while not native_children and process.poll() is None and time.monotonic() < deadline:
                for entry in Path('/proc').iterdir():
                    if not entry.name.isdigit():
                        continue
                    try:
                        parent = (entry / 'stat').read_text().rpartition(')')[2].split()[1]
                        if parent == str(process.pid) and (entry / 'cmdline').read_bytes().startswith(b'git\x00config\x00'):
                            native_children.append(int(entry.name))
                    except (FileNotFoundError, PermissionError, ProcessLookupError):
                        pass
                if not native_children:
                    time.sleep(.01)
            self.assertTrue(native_children, 'real Git config reader did not start')
            process.send_signal(signal.SIGTERM)
            try:
                _, errors = process.communicate(timeout=2)
            except subprocess.TimeoutExpired:
                self.fail('effective native Git config capture ignored SIGTERM while reading a FIFO')
            self.assertEqual(process.returncode, -signal.SIGTERM, errors)
            self.assertEqual(list(f.root.glob('git-on-*')), [])
            for pid in native_children:
                proc = Path('/proc') / str(pid)
                self.assertTrue(not proc.exists() or (proc / 'stat').read_text().rpartition(')')[2].split()[0] == 'Z',
                                'owned Git config reader survived cancellation')
        finally:
            if process.poll() is None:
                for pid in native_children:
                    with contextlib.suppress(FileNotFoundError, ProcessLookupError):
                        parent = (Path('/proc') / str(pid) / 'stat').read_text().rpartition(')')[2].split()[1]
                        if parent == str(process.pid):
                            os.killpg(pid, signal.SIGKILL)
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=5)

    def test_stale_cleanup_never_stops_replaced_or_unrecorded_real_socket(self):
        f = self.f
        url = f.ssh()
        user_host, port_and_path = url.removeprefix('ssh://').split(':', 1)
        port = port_and_path.split('/', 1)[0]
        for owner_mode in ('replaced', 'missing', 'malformed'):
            with self.subTest(owner_mode=owner_mode):
                unrelated = f.root / ('unrelated-master-' + owner_mode)
                remote.run(shlex.split(f.env['GIT_SSH_COMMAND']) + [
                    '-p', port, '-M', '-S', str(unrelated), '-o', 'ControlPersist=yes', '-fN', user_host], env=f.env)
                self.addCleanup(lambda path=unrelated: subprocess.run(
                    ['ssh', '-F', '/dev/null', '-S', str(path), '-O', 'exit', 'unused'],
                    capture_output=True, timeout=5))
                stale = f.root / ('git-on-stale-' + owner_mode)
                stale.mkdir(mode=0o700)
                (stale / '.lock').touch()
                (stale / '.state').write_text(json.dumps(dict(
                    format='git-on-temp-v1', id=str(uuid.uuid4()), keep=False,
                    git_dir=None, tree_oid=None, pin_ref=None)))
                target = stale / 'ssh-control'
                if owner_mode == 'replaced':
                    with socket.socket(socket.AF_UNIX) as original:
                        original.bind(str(target))
                    metadata = target.stat()
                    (stale / 'ssh-control-owner').write_text(json.dumps(dict(
                        format='git-on-ssh-control-v1', device=metadata.st_dev, inode=metadata.st_ino)))
                    target.unlink()
                elif owner_mode == 'malformed':
                    (stale / 'ssh-control-owner').write_text('invalid recovery identity')
                os.link(unrelated, target)
                cleanup = subprocess.run([str(remote.OPTIONS.binary.resolve()), '--cleanup', str(stale)],
                                         env=f.env, capture_output=True, timeout=5)
                self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
                self.assertFalse(stale.exists(), 'validated stale run was not removed')
                check = subprocess.run(['ssh', '-F', '/dev/null', '-S', str(unrelated), '-O', 'check', 'unused'],
                                       capture_output=True, timeout=5)
                self.assertEqual(check.returncode, 0, 'cleanup stopped a replaced/unrecorded external socket')


if __name__ == '__main__':
    unittest.main(argv=[__file__, *remote.ARGS])
