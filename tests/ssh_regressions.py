#!/usr/bin/env python3
"""Focused SSH review regressions using disposable real OpenSSH fixtures."""
import argparse
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
import sys
import time
import unittest
import uuid

SPEC = importlib.util.spec_from_file_location('git_on_remote_fixtures', Path(__file__).with_name('remote.py'))
remote = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(remote)


PARSER = argparse.ArgumentParser(add_help=False)
PARSER.add_argument('--before-boundary-probe', type=Path,
                    help='Authentic pre-boundary-fix source probe for focused red-before replay')
PARSER.add_argument('--before-client-probe', type=Path,
                    help='Authentic 87a11d5 source probe for changed-client red-before replay')
PARSER.add_argument('--before-client-binary', type=Path,
                    help='Authentic 87a11d5 binary for changed-client recovery red-before replay')
SSH_OPTIONS, remote.ARGS = PARSER.parse_known_args(remote.ARGS)
for artifact in ('before_boundary_probe','before_client_probe','before_client_binary'):
    selected=getattr(SSH_OPTIONS,artifact)
    if selected is not None: setattr(SSH_OPTIONS,artifact,selected.resolve())
if (SSH_OPTIONS.before_client_probe is None) != (SSH_OPTIONS.before_client_binary is None):
    PARSER.error('client replay requires both --before-client-probe and --before-client-binary')


class SshRegressionTests(unittest.TestCase):
    def setUp(self):
        self.f = remote.Fixture()
        self.addCleanup(self.f.close)

    def loaded_read(self, url, fixture=None):
        f = self.f if fixture is None else fixture
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

    def client_rewrite_shutdown(self, mode, before=False):
        f=remote.Fixture(); self.addCleanup(f.close)
        url=f.ssh(); ssh=str(Path(shutil.which('ssh')).resolve())
        command=shlex.quote(ssh)+f.env.pop('GIT_SSH_COMMAND')[3:]
        executable=f.root/'selected ssh wrapper'
        original=('#!/bin/sh\nexec '+command+' "$@"\n').encode()
        executable.write_bytes(original); executable.chmod(0o700); metadata=executable.stat()
        f.env['GIT_SSH']=str(executable)
        marker=f.root/'changed-client-executed'
        replacement=('#!/bin/sh\n: > '+shlex.quote(str(marker))+'\nexit 0\n').encode()
        self.assertLess(len(replacement)+2,len(original))
        replacement+=b'#'+b' '*(len(original)-len(replacement)-2)+b'\n'
        armed=f.root/'armed'; payload=f.root/'replacement'; payload.write_bytes(replacement)
        if mode=='spawn-race':
            library=f.root/'shutdown-race.so'
            remote.run(['cc','-shared','-fPIC','-Wall','-Wextra','-Werror',
                        Path(__file__).parent/'support/shutdown_exec_race.c','-ldl','-o',library])
            f.env.update(LD_PRELOAD=str(library),GIT_ON_REVIEW_CLIENT_PATH=str(executable),
                         GIT_ON_REVIEW_CLIENT_ARMED=str(armed),GIT_ON_REVIEW_CLIENT_REPLACEMENT=str(payload))
        probe=SSH_OPTIONS.before_client_probe if before else remote.OPTIONS.probe
        binary=SSH_OPTIONS.before_client_binary if before else remote.OPTIONS.binary
        process=subprocess.Popen([str(probe),url,'HEAD'],cwd=f.cwd,env=f.env,stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        f.children.append(process); f.line(process)
        self.assertFalse(marker.exists(),'fixture changed the client before shutdown')
        directory=next(f.root.glob('*/ssh-control')).parent
        control=[ssh,'-F','/dev/null','-S',str(directory/'ssh-control'),'-O','check','git-on-control']
        checked=subprocess.run(control,capture_output=True,check=True,timeout=5)
        descriptor=os.pidfd_open(int(re.search(rb'pid=(\d+)',checked.stderr).group(1)))
        pin=['git','--git-dir='+str(directory/'remote-repository'),'show-ref','--verify','refs/git-on/probe']
        try:
            if mode=='recovery':
                process.kill(); process.communicate(timeout=10)
                self.assertEqual(process.returncode,-signal.SIGKILL)
            if mode=='spawn-race': armed.touch()
            else:
                executable.write_bytes(replacement)
                os.utime(executable,ns=(metadata.st_atime_ns,metadata.st_mtime_ns))
                current=executable.stat()
                self.assertEqual((current.st_dev,current.st_ino,current.st_size,current.st_mtime_ns),
                                 (metadata.st_dev,metadata.st_ino,metadata.st_size,metadata.st_mtime_ns))
            (directory/'.lock').touch()
            (directory/'.state').write_text(json.dumps(dict(format='git-on-temp-v1',id='probe',keep=False,
                       git_dir=str(directory/'remote-repository'),tree_oid=f.tree,pin_ref='refs/git-on/probe')))
            if mode=='recovery':
                result=subprocess.run([str(binary),'--cleanup',str(directory)],env=f.env,capture_output=True,timeout=10)
                status,errors=result.returncode,result.stderr
            else:
                process.stdin.write(b'exit\n'); process.stdin.flush()
                _,errors=process.communicate(timeout=10); status=process.returncode
            if before:
                self.assertTrue(marker.exists(),'authentic baseline did not execute the changed client')
                if mode=='spawn-race': self.assertFalse(armed.exists(),'baseline did not reach the armed spawn race')
                print(f'RED 87a11d5 client {mode}: changed equal-length client executed after metadata validation',file=sys.stderr)
            else:
                self.assertFalse(marker.exists(),'shutdown executed changed client bytes')
            if mode=='spawn-race' and not before:
                self.assertEqual(status,0,errors)
                self.assertTrue(armed.exists(),'shutdown attempted to execute the selected pathname')
                self.assertEqual(executable.read_bytes(),original)
                self.assertFalse(directory.exists())
                self.assertTrue(select.select([descriptor],[],[],2)[0])
            else:
                self.assertNotEqual(status,0,errors)
                if not before: self.assertIn(b'executable changed',errors)
                self.assertTrue(directory.exists())
                self.assertFalse(select.select([descriptor],[],[],0)[0])
                self.assertEqual(subprocess.run(control,capture_output=True,timeout=5).returncode,0)
                self.assertEqual(subprocess.run(pin,capture_output=True,timeout=5).returncode,0)
                executable.write_bytes(original); os.utime(executable,ns=(metadata.st_atime_ns,metadata.st_mtime_ns))
                cleanup=subprocess.run([str(binary),'--cleanup',str(directory)],env=f.env,capture_output=True,timeout=10)
                self.assertEqual(cleanup.returncode,0,cleanup.stderr)
                self.assertFalse(directory.exists())
                self.assertTrue(select.select([descriptor],[],[],2)[0])
        finally:
            if process.poll() is None: process.kill(); process.communicate(timeout=10)
            if not select.select([descriptor],[],[],0)[0]:
                with contextlib.suppress(ProcessLookupError): signal.pidfd_send_signal(descriptor,signal.SIGTERM)
            os.close(descriptor)

    def test_equal_length_restored_mtime_client_change_retains_live_master_pin_and_run(self):
        for mode in ('drop','recovery'):
            with self.subTest(mode=mode):
                if SSH_OPTIONS.before_client_probe is not None: self.client_rewrite_shutdown(mode,before=True)
                self.client_rewrite_shutdown(mode)

    def test_shutdown_never_executes_selected_path_after_validation(self):
        if SSH_OPTIONS.before_client_probe is not None: self.client_rewrite_shutdown('spawn-race',before=True)
        self.client_rewrite_shutdown('spawn-race')

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
            for invalid in ('relative', 'unknown-field', 'invalid-type', 'missing-digest', 'bad-digest', 'directory', 'symlink', 'in-place'):
                with self.subTest(record=invalid):
                    changed = json.loads(json.dumps(record))
                    if invalid == 'relative': changed['client']['path'] = list(b'ssh')
                    elif invalid == 'unknown-field': changed['client']['command'] = 'do not execute'
                    elif invalid == 'invalid-type': changed['client']['path'] = 'not byte provenance'
                    elif invalid == 'missing-digest': changed['client'].pop('content_sha256')
                    elif invalid == 'bad-digest': changed['client']['content_sha256'][0] ^= 255
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

    def test_owned_socket_removed_during_unlink_does_not_block_recovery(self):
        f = self.f
        library = f.root / 'socket-unlink-race.so'
        remote.run(['cc', '-shared', '-fPIC', '-Wall', '-Wextra', '-Werror',
                    Path(__file__).parent / 'support/ssh_socket_unlink_race.c', '-ldl', '-o', library])
        for mode in ('removed', 'denied'):
            with self.subTest(mode=mode):
                directory = f.root / ('git-on-stale-' + mode)
                directory.mkdir(mode=0o700)
                (directory / '.lock').touch()
                (directory / '.state').write_text(json.dumps(dict(
                    format='git-on-temp-v1', id='socket-unlink-race', keep=False,
                    git_dir=None, tree_oid=None, pin_ref=None)))
                control = directory / 'ssh-control'
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as endpoint:
                    endpoint.bind(str(control))
                identity = control.stat()
                (directory / 'ssh-control-owner').write_text(json.dumps(dict(
                    format='git-on-ssh-control-v1', device=identity.st_dev,
                    inode=identity.st_ino, client=None)))
                marker = f.root / ('unlink-interleaving-' + mode)
                env = dict(f.env, LD_PRELOAD=str(library), GIT_ON_REVIEW_UNLINK_PATH=str(control),
                           GIT_ON_REVIEW_UNLINK_MARKER=str(marker), GIT_ON_REVIEW_UNLINK_MODE=mode)
                result = subprocess.run([str(remote.OPTIONS.binary), '--cleanup', str(directory)],
                                        env=env, capture_output=True, timeout=10)
                self.assertTrue(marker.exists(), 'controlled unlink interleaving was not reached')
                if mode == 'removed':
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertFalse(directory.exists(), 'already removed owned socket blocked recovery')
                else:
                    self.assertEqual(result.returncode, 125, result.stderr)
                    self.assertIn(b'remove disconnected owned SSH control socket', result.stderr)
                    self.assertTrue(control.exists(), 'failed unlink discarded the owned socket')
                    self.assertTrue((directory / '.state').exists(), 'failed unlink discarded recovery state')
                    cleanup = subprocess.run([str(remote.OPTIONS.binary), '--cleanup', str(directory)],
                                             env=f.env, capture_output=True, timeout=10)
                    self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
                    self.assertFalse(directory.exists())

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

    def boundary_connection_loss(self, encrypted, master, before_probe=None, user_host=False):
        f = remote.Fixture()
        self.addCleanup(f.close)
        url = f.ssh(encrypted=encrypted)
        if user_host:
            f.env['GIT_SSH_COMMAND'] += self.user_host_command(f, url)
        else:
            f.env['GIT_SSH_COMMAND'] += ' -oControlPath="$GIT_ON_SSH_CONTROL_PATH" ' + master
        if before_probe is None:
            process = f.probe(url)
        else:
            process = subprocess.Popen([str(before_probe), url, 'HEAD'], cwd=f.cwd, env=f.env,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, bufsize=0)
            f.children.append(process)
        f.line(process)
        sockets = list(f.root.glob('*/ssh-control'))
        self.assertEqual(len(sockets), 1)
        control = ['ssh', '-F', '/dev/null', '-S', str(sockets[0]), '-O', 'exit', 'git-on-control']
        initial_log = (f.root / 'sshd.log').read_text()
        try:
            remote.run(control, timeout=5)
            deadline = time.monotonic() + 5
            while sockets[0].exists() and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertFalse(sockets[0].exists(), 'owned master did not release its socket')
            # Send exit too: the broken implementation can read successfully
            # after creating a replacement master instead of returning an error.
            process.stdin.write(b'read deep/file\nexit\n')
            process.stdin.flush()
            output, errors = process.communicate(timeout=10)
            return dict(initial_log=initial_log, log=(f.root / 'sshd.log').read_text(),
                        output=output, errors=errors, status=process.returncode,
                        sockets=list(f.root.glob('*/ssh-control')),
                        prompts=(f.root / 'prompts').read_text().splitlines() if encrypted else [])
        finally:
            # A red-before replay can leave a newly created master whose socket
            # no longer matches the original identity.
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=5)
            subprocess.run(control, capture_output=True, timeout=5)

    def test_user_control_path_expansion_cannot_hide_master_options_after_connection_loss(self):
        # The run's socket path is intentionally visible to the selected SSH
        # command. Matching that value must never act as an argv delimiter.
        if SSH_OPTIONS.before_boundary_probe is not None:
            for master in ('-M', '-vvM'):
                with self.subTest(before=True, master=master):
                    # An unlocked fixture authenticates any replacement TCP
                    # connection. With an encrypted identity the old code can
                    # already fail during startup, before the forced loss.
                    observed = self.boundary_connection_loss(False, master,
                                                              SSH_OPTIONS.before_boundary_probe)
                    before = observed['initial_log'].count('Connection from ')
                    after = observed['log'].count('Connection from ')
                    self.assertGreater(after, before,
                                       'old probe did not reproduce a TCP reconnect after owned master loss')
                    self.assertGreater(observed['log'].count('Accepted publickey'),
                                       observed['initial_log'].count('Accepted publickey'),
                                       'old probe did not authenticate its replacement connection')
                    self.assertEqual(observed['status'], 0, observed['errors'])
                    self.assertIn('expected\n', [json.loads(line).get('data')
                                                 for line in observed['output'].splitlines()])
                    print(f'RED pre-fix SSH boundary {master}: forced master loss caused '
                          f'TCP connections {before} -> {after} and a successful lazy read', file=sys.stderr)
        for encrypted in (False, True):
            for master in ('-M', '-vvM'):
                with self.subTest(encrypted=encrypted, master=master):
                    observed = self.boundary_connection_loss(encrypted, master)
                    self.assertEqual(observed['log'].count('Connection from '),
                                     observed['initial_log'].count('Connection from '),
                                     'lost owned master caused a replacement TCP connection:\n' + observed['log'])
                    self.assertEqual(observed['log'].count('Accepted publickey'), 1)
                    self.assertNotEqual(observed['status'], 0, observed['errors'])
                    self.assertIn(b'remote fetch of exact object', observed['errors'])
                    self.assertEqual(observed['sockets'], [])
                    if encrypted:
                        self.assertEqual(observed['prompts'], ['prompt'])

    @staticmethod
    def user_host_command(f, url):
        user_host, port_and_path = url.removeprefix('ssh://').split(':', 1)
        port = port_and_path.split('/', 1)[0]
        # A remote shell comment swallows everything Git/git-on append after
        # the caller's host. Without rejecting that host, -O proxy is ineffective.
        upload_pack = 'git-upload-pack ' + shlex.quote(str(f.repo)) + ' #'
        return ' -p ' + port + ' ' + shlex.quote(user_host) + ' ' + shlex.quote(upload_pack)

    def test_user_ssh_host_and_remote_command_cannot_swallow_owned_control_options(self):
        if SSH_OPTIONS.before_boundary_probe is not None:
            observed = self.boundary_connection_loss(False, '', SSH_OPTIONS.before_boundary_probe,
                                                      user_host=True)
            before = observed['initial_log'].count('Connection from ')
            after = observed['log'].count('Connection from ')
            self.assertGreater(after, before,
                               'old user-host probe did not reconnect after owned master loss')
            self.assertGreater(observed['log'].count('Accepted publickey'),
                               observed['initial_log'].count('Accepted publickey'))
            self.assertEqual(observed['status'], 0, observed['errors'])
            self.assertIn('expected\n', [json.loads(line).get('data')
                                         for line in observed['output'].splitlines()])
            print(f'RED pre-fix SSH user host: forced master loss caused TCP connections '
                  f'{before} -> {after} and a successful lazy read', file=sys.stderr)
        f = self.f
        url = f.ssh()
        f.env['GIT_SSH_COMMAND'] += self.user_host_command(f, url)
        process = f.probe(url)
        output, errors = process.communicate(timeout=10)
        self.assertNotEqual(process.returncode, 0, output)
        self.assertIn(b'Git supplies the host and remote command', errors)
        self.assertNotIn('Accepted publickey', (f.root / 'sshd.log').read_text())
        self.assertEqual(list(f.root.glob('*/ssh-control')), [])

    def test_ssh_tag_option_values_resembling_control_flags_remain_literal(self):
        capability = subprocess.run(['ssh', '-F', '/dev/null', '-G', '-P', '-M', 'git-on-tag-check'],
                                    capture_output=True, timeout=5)
        if capability.returncode:
            self.skipTest('installed OpenSSH does not support -P tags')
        for tag in ('-M', '-O'):
            with self.subTest(tag=tag):
                f = remote.Fixture()
                self.addCleanup(f.close)
                url = f.ssh()
                config = f.root / 'tagged_config'
                config.write_text('Host *\n    IdentityFile none\nMatch tagged ' + tag +
                                  '\n    IdentityFile ' + str(f.root / 'identity') + '\n')
                f.env['GIT_SSH_COMMAND'] = (f'ssh -F {config} -o IdentitiesOnly=yes '
                                          f'-o UserKnownHostsFile={f.root}/known_hosts '
                                          '-o StrictHostKeyChecking=yes -P ' + shlex.quote(tag))
                if SSH_OPTIONS.before_boundary_probe is not None:
                    before = subprocess.Popen([str(SSH_OPTIONS.before_boundary_probe), url, 'HEAD'],
                                              cwd=f.cwd, env=f.env, stdin=subprocess.PIPE,
                                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    f.children.append(before)
                    _, errors = before.communicate(timeout=10)
                    self.assertNotEqual(before.returncode, 0)
                    expected = (b'Permission denied (publickey)' if tag == '-M' else
                                b'SSH control commands (-O) are owned by the runner')
                    self.assertIn(expected, errors)
                    self.assertNotIn('Accepted publickey', (f.root / 'sshd.log').read_text())
                    self.assertEqual(list(f.root.glob('*/ssh-control')), [])
                    print(f'RED pre-fix SSH -P {tag}: literal option value was misparsed '
                          'and startup failed before authentication', file=sys.stderr)
                self.loaded_read(url, f)

    def test_short_proxy_jump_option_remains_usable_for_lazy_reads(self):
        f = self.f
        url = f.ssh(encrypted=True)
        # OpenSSH -J none is a native direct-host configuration. Inserting a
        # ProxyCommand=false fallback guard wrongly conflicts even with -J none.
        f.env['GIT_SSH_COMMAND'] += ' -J none'
        self.loaded_read(url)
        self.assertEqual((f.root / 'prompts').read_text().splitlines(), ['prompt'])

    def test_user_master_flag_clusters_cannot_become_option_terminator(self):
        f = self.f
        url = f.ssh()
        original = f.env['GIT_SSH_COMMAND']
        for flags in ('--', '-M-', '--M'):
            with self.subTest(flags=flags):
                f.env['GIT_SSH_COMMAND'] = original + ' ' + flags
                process = f.probe(url)
                _, errors = process.communicate(timeout=10)
                self.assertNotEqual(process.returncode, 0)
                self.assertIn(b'may not end option parsing before Git supplies its host', errors)
                self.assertNotIn('Accepted publickey', (f.root / 'sshd.log').read_text())
                self.assertEqual(list(f.root.glob('*/ssh-control')), [])

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
