"""Exercise the HTTP/2 demand path on either real native mount."""
import base64
from collections import Counter
import io
import json
import os
import pty
import re
import select
import shlex
import shutil
import subprocess
import sys
import termios
import time
from types import SimpleNamespace
import unittest


from remote import ARGS, OPTIONS, Fixture
from support.http2_server import Multiplexer
from support.git_invocations import git_invocations, record_git_invocations


def tested_backends():
    mounts = [name for name, enabled in (("fuse", OPTIONS.require_fuse), ("nfs", OPTIONS.require_nfs))
              if enabled]
    return mounts or ["copy"]


class Http2Tests(unittest.TestCase):
    def test_reports_actual_pack_progress_while_the_stream_is_open(self):
        for backend in tested_backends():
            with self.subTest(backend=backend):
                self.check_pack_progress(backend)

    def check_pack_progress(self, backend):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        upstream = fixture.http1_upstream()
        token = base64.b64encode(b"fixture:example-password").decode()
        server = Multiplexer(fixture, upstream, "Basic " + token)
        fixture.env.update(
            GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="credential.helper",
            GIT_CONFIG_VALUE_0="!f() { echo username=fixture; echo password=example-password; }; f",
        )
        server.start(self.addCleanup)
        pack = subprocess.check_output(["git", "-C", fixture.repo, "pack-objects", "--stdout", "--revs"],
                                       input=(fixture.commit + "\n").encode(), env=fixture.env)
        total = int.from_bytes(pack[8:12], "big")
        server.split_pack = SimpleNamespace(
            wants={fixture.commit.encode()},
            prefix=b"000dpackfile\n" + f"{len(pack) + 5:04x}".encode() + b"\x01" + pack[:12],
            tail=pack[12:] + b"0000", started=fixture.root / "pack-started",
            release=fixture.root / "release-pack",
        )
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        self.addCleanup(os.close, slave)
        termios.tcsetwinsize(slave, (24, 100))
        process = subprocess.Popen(
            [OPTIONS.binary, server.url, fixture.commit, f"--backend={backend}", "--write-policy=all",
             "--", "cat", "deep/file"],
            cwd=fixture.cwd, env=dict(fixture.env, TERM="xterm"), stdout=subprocess.PIPE, stderr=slave,
        )
        fixture.children.append(process)
        transcript = bytearray()
        try:
            deadline = time.monotonic() + 8
            while b"MiB/s" not in transcript or f"0/{total}".encode() not in transcript:
                self.assertLess(time.monotonic(), deadline, transcript)
                self.assertIsNone(process.poll(), transcript)
                if select.select([master], [], [], .1)[0]:
                    transcript.extend(os.read(master, 65536))
        finally:
            server.split_pack.release.touch()
        output, _ = process.communicate(timeout=15)
        self.assertEqual(process.returncode, 0, transcript)
        self.assertEqual(output, b"expected\n")
        self.assertEqual(server.unauthenticated_wants, {fixture.commit.encode()})

    def test_default_origin_streams_like_url_without_mutating_credentials_or_refs(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        credentials = fixture.root / "credentials"
        valid = b"username=fixture\npassword=example-password\n"
        credentials.write_bytes(valid)
        path = shlex.quote(str(credentials))
        helper = fixture.root / "credential-helper"
        helper.write_text(
            '#!/bin/sh\ncase "$1" in\n'
            f'get) if [ "$DENIED" = 1 ]; then printf "username=fixture\\npassword=wrong\\n"; '
            f'else cat {path}; fi ;;\n'
            f'store|erase) printf modified > {path} ;;\nesac\n'
        )
        helper.chmod(0o700)
        token = base64.b64encode(b"fixture:example-password").decode()
        url = fixture.https(authorization="Basic " + token)
        fixture.git("init", "--quiet", at=fixture.cwd)
        fixture.git("remote", "add", "origin", url, at=fixture.cwd)
        hooks = fixture.root / "hooks"
        hooks.mkdir()
        hook = hooks / "reference-transaction"
        journal = fixture.root / "hook-journal"
        hook.write_text(f'#!/bin/sh\nprintf "%s\\n" "$1" >> {shlex.quote(str(journal))}\n')
        hook.chmod(0o700)
        fixture.git("config", "core.hooksPath", str(hooks), at=fixture.cwd)
        config = fixture.cwd / ".git/config"
        before = config.read_bytes()
        env, invocations = record_git_invocations(fixture.root, fixture.env)
        trace_path = fixture.root / "file-events"
        trace = (["strace", "-f", "-s", "512", "-o", trace_path, "-e", "trace=%file"]
                 if sys.platform == "linux" and shutil.which("strace") else [])
        for source, revision, denied in ((url, "main", False), ("origin", "main", False),
                                         ("origin", "main", True), (url, fixture.commit, False)):
            with self.subTest(source=source, revision=revision, denied=denied):
                invocations.write_text("")
                fixture.requests.clear()
                result = subprocess.run(
                    [*trace, OPTIONS.binary, source, revision, "--backend=copy",
                     "--write-policy=all", "--", "sh", "-c",
                     'printf "%s\\n" "$GIT_ON_TREE_OID"; cat deep/file'],
                    cwd=fixture.cwd, env=dict(env, DENIED=str(int(denied)), GIT_CONFIG_COUNT="1",
                                              GIT_CONFIG_KEY_0="credential.helper",
                                              GIT_CONFIG_VALUE_0="!exec " + shlex.quote(str(helper))),
                    capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 125 if denied else 0, result.stderr)
                self.assertEqual(result.stdout, b"" if denied else f"{fixture.tree}\nexpected\n".encode())
                calls = git_invocations(invocations)
                self.assertEqual(sum(call[-2:] == ["credential", "fill"] for call in calls), 1, calls)
                self.assertFalse(any({"fetch", "index-pack", "init"} & set(call) for call in calls), calls)
                if trace:
                    created = [line for line in trace_path.read_text().splitlines()
                               if ("O_CREAT" in line or "mkdir" in line)
                               and re.search(r'/(config|HEAD|objects|refs|FETCH_HEAD|[^"]+[.](pack|idx))"', line)]
                    self.assertEqual(created, [])
                self.assertEqual(credentials.read_bytes(), valid)
                self.assertEqual(config.read_bytes(), before)
                self.assertFalse(journal.exists())
                self.assertEqual(fixture.git("for-each-ref", at=fixture.cwd), "")
                if not denied:
                    self.assertEqual([r["wants"] for r in fixture.requests if r["wants"]], [[fixture.commit]])
                    if revision == fixture.commit:
                        posts = [r for r in fixture.requests if r["path"].endswith("/git-upload-pack")]
                        self.assertEqual(len(posts), 1, posts)

    def test_url_matched_authorization_header_is_sent_once(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        token = base64.b64encode(b"fixture:example-password").decode()
        value = "Basic " + token
        server = fixture.https_server(authorization=value)
        fixture.git("init", "--quiet", at=fixture.cwd)
        fixture.git("config", f"http.{server.url}.extraHeader", f"Authorization: {value}", at=fixture.cwd)
        fixture.git("config", "http.https://other.invalid/.extraHeader", "Authorization: unused", at=fixture.cwd)
        env, invocations = record_git_invocations(fixture.root, fixture.env)
        for backend in tested_backends():
            with self.subTest(backend=backend):
                server.authorization_counts.clear()
                result = subprocess.run(
                    [OPTIONS.binary, server.url, "main", f"--backend={backend}", "--write-policy=all",
                     "--", "cat", "deep/file"],
                    cwd=fixture.cwd, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b"expected\n")
                self.assertTrue(server.authorization_counts)
                self.assertEqual(set(server.authorization_counts), {1})
                self.assertFalse(any("credential" in call for call in git_invocations(invocations)))

    def test_eager_pack_publishes_parsed_trees_before_the_first_lookup(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        url = fixture.https()
        process = fixture.probe(url, fixture.commit, eager=True)
        report = fixture.line(process)
        self.assertEqual(report["tree"], fixture.tree)
        self.assertEqual(report["metrics"]["tree_cache_entries"], 2)
        self.assertEqual(report["metrics"]["tree_reads"], 0)
        posts = [r["wants"] for r in fixture.requests if r["path"].endswith("/git-upload-pack")]
        self.assertEqual(posts, [[fixture.commit]])
        contents, _ = fixture.operation(process, "read deep/file")
        self.assertEqual(contents["data"], "expected\n")
        fixture.finish(process)

    def test_uses_effective_transport_settings(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        server = fixture.https_server()
        http_settings = [
            ("http.sslCAInfo", fixture.env["GIT_SSL_CAINFO"]),
            ("http.userAgent", "git-on-fixture"),
            ("http.postBuffer", "1048576"),
        ]
        for settings, allowed in (
            ([("protocol.version", "1"), ("protocol.version", "2")], True),
            ([("protocol.version", "1")], False),
            ([("fetch.fsckObjects", "false")], True),
            ([("transfer.fsckObjects", "true"), ("transfer.fsckObjects", "false")], True),
            ([("fetch.fsckObjects", "true")], False),
            (http_settings, True),
        ):
            with self.subTest(settings=settings):
                env = dict(fixture.env, GIT_CONFIG_COUNT=str(len(settings)), GIT_TRACE_PACKET="0")
                check_http = settings == http_settings
                if check_http:
                    env.pop("GIT_SSL_CAINFO")
                    server.user_agents.clear()
                for index, (key, value) in enumerate(settings):
                    env[f"GIT_CONFIG_KEY_{index}"] = key
                    env[f"GIT_CONFIG_VALUE_{index}"] = value
                result = subprocess.run(
                    [OPTIONS.binary, server.url, fixture.commit, "--backend=copy", "--write-policy=all",
                     "--", "cat", "deep/file"],
                    cwd=fixture.cwd, env=env, capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0 if allowed else 125, result.stderr)
                self.assertEqual(result.stdout, b"expected\n" if allowed else b"")
                if check_http:
                    self.assertEqual(server.user_agents, {"git-on-fixture"})

    def test_initial_discovery_redirect_uses_the_new_repository_path(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        token = base64.b64encode(b"fixture:example-password").decode()
        server = Multiplexer(fixture, fixture.http1_upstream(), "Basic " + token)
        server.start(self.addCleanup)
        server.redirect_discovery = True
        url = server.url.replace("/server.git", "/old.git").replace("https://", "https://fixture@", 1)
        helper = "!f() { grep -qx 'path=server.git' && printf 'password=example-password\\n'; }; f"
        for policy in ("initial", "true", "false"):
            with self.subTest(policy=policy):
                result = subprocess.run(
                    [OPTIONS.binary, url, fixture.commit, "--backend=copy", "--write-policy=all",
                     "--", "cat", "deep/file"],
                    cwd=fixture.cwd, env=dict(fixture.env, GIT_CONFIG_COUNT="3",
                                              GIT_CONFIG_KEY_0="http.followRedirects",
                                              GIT_CONFIG_VALUE_0=policy,
                                              GIT_CONFIG_KEY_1="credential.useHttpPath",
                                              GIT_CONFIG_VALUE_1="true",
                                              GIT_CONFIG_KEY_2="credential.helper",
                                              GIT_CONFIG_VALUE_2=helper),
                    capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 125 if policy == "false" else 0, result.stderr)
                self.assertEqual(result.stdout, b"" if policy == "false" else b"expected\n")

    def test_plain_https_url_reads_netrc_after_relative_redirect(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        token = base64.b64encode(b"fixture:example-password").decode()
        server = fixture.https_server(authorization="Basic " + token)
        server.redirect_discovery = True
        url = server.url.replace("/server.git", "/old.git")
        home = fixture.root / "home"
        home.mkdir()
        netrc = home / ".netrc"
        contents = "machine 127.0.0.1 login fixture password example-password\n"
        netrc.write_text(contents)
        netrc.chmod(0o600)
        env = dict(fixture.env, HOME=str(home), GIT_TERMINAL_PROMPT="0")
        env.pop("NETRC", None)
        env.pop("CURL_HOME", None)
        result = subprocess.run(
            [OPTIONS.binary, url, fixture.commit, "--backend=copy", "--write-policy=all",
             "--", "cat", "deep/file"],
            cwd=fixture.cwd, env=env, capture_output=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"expected\n")
        self.assertEqual(netrc.read_text(), contents)

    def test_url_credentials_reach_the_credential_lookup(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        token = base64.b64encode(b"fixture:example-password").decode()
        url = fixture.https(authorization="Basic " + token)
        env, invocations = record_git_invocations(fixture.root, fixture.env)
        for userinfo, helper, named in (
            ("fixture", "!f() { echo password=example-password; }; f", False),
            ("fixture:example%2Dpassword", "!false", False),
            ("fixture:example%2Dpassword", "!false", True),
        ):
            with self.subTest(explicit_password=":" in userinfo, named=named):
                source = url.replace("https://", f"https://{userinfo}@", 1)
                config = dict(env, GIT_CONFIG_COUNT="2",
                              GIT_CONFIG_KEY_0="credential.helper", GIT_CONFIG_VALUE_0=helper,
                              GIT_CONFIG_KEY_1="http.userAgent", GIT_CONFIG_VALUE_1="git-on-fixture")
                if named:
                    key = f"remote.{source}.url"
                    config.update(GIT_CONFIG_COUNT="5",
                                  GIT_CONFIG_KEY_2=key, GIT_CONFIG_VALUE_2="file:///nonexistent/",
                                  GIT_CONFIG_KEY_3=key, GIT_CONFIG_VALUE_3="",
                                  GIT_CONFIG_KEY_4=key, GIT_CONFIG_VALUE_4=source)
                elif ":" in userinfo:
                    include = fixture.root / "unexpected-rewrite.conf"
                    include.write_text(f'[url "file:///nonexistent/"]\n\tinsteadOf = {source}\n')
                    config.update(GIT_CONFIG_COUNT="3",
                                  GIT_CONFIG_KEY_2=f"includeIf.hasconfig:remote.*.url:{source}.path",
                                  GIT_CONFIG_VALUE_2=str(include))
                result = subprocess.run(
                    [OPTIONS.binary, source, fixture.commit, "--backend=copy",
                     "--write-policy=all", "--", "cat", "deep/file"],
                    cwd=fixture.cwd,
                    env=config,
                    capture_output=True, timeout=20,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, b"expected\n")
        self.assertFalse(any("example%2Dpassword" in arg or "example-password" in arg
                             for call in git_invocations(invocations) for arg in call))

    @unittest.skipUnless(OPTIONS.require_nfs or OPTIONS.require_fuse, "requires a native mount")
    def test_post_authentication_and_multiplexed_fetches(self):
        fixture = Fixture()
        self.addCleanup(fixture.close)
        token = base64.b64encode(b'fixture:example-password').decode()
        backend = fixture.http1_upstream()
        self.addCleanup(fixture.server.shutdown)
        proxy = Multiplexer(fixture, backend, 'Basic ' + token)
        proxy.start(self.addCleanup)
        self.addCleanup(fixture.resume.set)
        fixture.env.update(
            GIT_CONFIG_COUNT='1',
            GIT_CONFIG_KEY_0='credential.helper',
            GIT_CONFIG_VALUE_0="!f() { echo username=fixture; echo password=example-password; }; f",
        )
        fixture.stalled_oids.add(fixture.other)
        gate = fixture.root / 'send-independent'
        independent = fixture.root / 'independent-done'
        code = f"""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import time
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
                    [OPTIONS.binary, f'https://127.0.0.1:{proxy.server_address[1]}/server.git',
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


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0], *ARGS])
