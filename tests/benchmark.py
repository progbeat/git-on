#!/usr/bin/env python3
"""Real-FUSE git-on benchmarks. See tests/benchmark.md for measurement limits."""
from __future__ import annotations

import argparse
from collections import defaultdict
from functools import lru_cache
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import resource
import selectors
import shutil
import signal
import stat
import statistics
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
FORMAT = "git-on-benchmark-v1"
BLOCK = 65536
SMALL = tuple((f"file-{i}.txt", (f"active file {i}\n".encode() * 32)[:256]) for i in range(3))


def environment():
    # Do not inherit repository redirection, alternate objects, or injected Git config.
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", LC_ALL="C")
    return env


def git(repo, *args, data=None, strip=True):
    output = subprocess.check_output(["git", "-C", str(repo), *args], input=data,
                                   env=environment(), stderr=subprocess.PIPE)
    return output.strip() if strip else output


def large_block(index):
    return hashlib.shake_256(f"git-on-benchmark-v1:{index}".encode()).digest(BLOCK)


def verify_digit_leaves(repo, oid, count):
    # Full verification is deliberately in preparation, never in a timed run.
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(["git", "-C", str(repo), "ls-tree", "-r", oid],
                                   stdout=subprocess.PIPE, stderr=errors, env=environment())
        seen = 0
        try:
            for line in process.stdout:
                digits = f"{seen:06d}"
                blob_oid = hashlib.sha1(b"blob 7\0" + (digits + "\n").encode()).hexdigest()
                expected = f"100644 blob {blob_oid}\t{'/'.join(digits)}\n".encode()
                if line != expected:
                    raise RuntimeError(f"digit fixture mismatch at logical file {seen}")
                seen += 1
            process.stdout.close()
            if process.wait() or seen != count:
                raise RuntimeError(f"digit fixture has {seen} verified leaves, expected {count}")
        except BaseException:
            process.kill()
            process.wait()
            raise


def digit_fixture(repo, count):
    # fast-import writes a pack directly: no million-file working tree and no
    # million subprocesses. Every seven-byte blob is genuinely distinct.
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(["git", "-C", str(repo), "fast-import", "--quiet", "--done"],
                                   stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                   stderr=errors, env=environment())
        try:
            message = b"deterministic digit fixture\n"
            process.stdin.write((f"commit refs/bench/digits-{count}\n"
                                 "committer Benchmark <benchmark@example.invalid> 0 +0000\n"
                                 f"data {len(message)}\n").encode() + message + b"\n")
            for base in range(0, count, 1000):
                batch = []
                for number in range(base, min(base + 1000, count)):
                    digits = f"{number:06d}"
                    batch.append(f"M 100644 inline {'/'.join(digits)}\ndata 7\n{digits}\n")
                process.stdin.write("".join(batch).encode())
            process.stdin.write(b"done\n")
            process.stdin.close()
            status = process.wait()
        except BaseException:
            process.kill()
            process.wait()
            raise
        if status:
            errors.seek(0)
            raise RuntimeError(errors.read().decode(errors="replace"))
    oid = git(repo, "rev-parse", f"refs/bench/digits-{count}^{{tree}}").decode()
    verify_digit_leaves(repo, oid, count)
    return dict(suite="digits", digit_files=count, tree_oid=oid, digit_depth=6, full_tree_fanout=10,
                verified_all_leaf_paths_and_blob_ids=True,
                ref=f"refs/bench/digits-{count}^{{tree}}", logical_files=count,
                logical_blob_bytes=count * 7, content="six-digit number plus LF",
                storage="fast-import pack; distinct blobs and trees")


def build_fixtures(directory, sizes, large_mib, storage, digit_files=None):
    directory.mkdir(parents=True, exist_ok=False)
    repo = directory / "repository"
    repo.mkdir()
    git(repo, "init", "--quiet", "--object-format=sha1", "--template=")
    git(repo, "config", "gc.auto", "0")

    def blob(data):
        return git(repo, "hash-object", "-w", "--stdin", data=data).decode()

    def tree(entries):
        data = b"".join(f"{mode} {kind} {oid}\t{name}\n".encode()
                        for mode, kind, oid, name in entries)
        return git(repo, "mktree", data=data).decode()

    if sizes:
        unused_blob = blob(b"unused\n" * 36 + b"end\n")  # 256 bytes, shared by every unused path.
        active = tree([("100644", "blob", blob(content), name) for name, content in SMALL])
    if sizes or large_mib:
        ignore = blob(b"out/\n")

    @lru_cache(maxsize=None)
    def unused(count):
        if count <= 100:
            return tree([("100644", "blob", unused_blob, f"f{i:03d}") for i in range(count)])
        chunk = 100
        while chunk * 100 < count:
            chunk *= 100
        return tree([("040000", "tree", unused(min(chunk, count - i)), f"d{i // chunk:03d}")
                     for i in range(0, count, chunk)])

    fixtures = [digit_fixture(repo, digit_files)] if digit_files else []
    for count in sizes:
        oid = tree([("100644", "blob", ignore, ".gitignore"),
                    ("040000", "tree", active, "active"),
                    ("040000", "tree", unused(count), "unused")])
        ref = f"refs/bench/scale-{count}"
        git(repo, "update-ref", ref, oid)
        fixtures.append(dict(suite="scale", unused_files=count, tree_oid=oid, ref=ref,
                             logical_files=count + 4, logical_blob_bytes=count * 256 + 773))
    for mib in large_mib:
        size = mib * 1024 * 1024
        process = subprocess.Popen(["git", "-C", str(repo), "hash-object", "-w", "--stdin"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, env=environment())
        try:
            for index in range(size // BLOCK):
                process.stdin.write(large_block(index))
            process.stdin.close()
            process.stdin = None
            out, err = process.communicate()
        except BaseException:
            process.kill()
            process.wait()
            raise
        if process.returncode:
            raise RuntimeError(err.decode(errors="replace"))
        oid = tree([("100644", "blob", ignore, ".gitignore"),
                    ("100644", "blob", out.decode().strip(), "large.bin")])
        ref = f"refs/bench/large-{mib}"
        git(repo, "update-ref", ref, oid)
        fixtures.append(dict(suite="large", large_bytes=size, tree_oid=oid, ref=ref,
                             logical_files=2, logical_blob_bytes=size + 5))
    if storage == "packed":
        git(repo, "repack", "-a", "-d")
    manifest = dict(format=FORMAT, secondary_storage=storage, digit_storage="fast-import packed",
                    secondary_fanout=100, secondary_unused_blob_bytes=256,
                    fixture_bias="digits: distinct contents; scale: shared tree/blob objects; large: deterministic SHAKE256 blocks.",
                    git_version=git(repo, "--version").decode(), fixtures=fixtures,
                    object_storage=git(repo, "count-objects", "-v").decode())
    (directory / "fixtures.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


# The timestamp precedes benchmark imports and any access to the mounted tree.
# Python itself is executed from the host, with site/environment startup disabled.
CHILD = r'''
import time
started_ns = time.monotonic_ns()
import os, sys, json, hashlib
mode, count, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
small = [("active/file-%d.txt" % i, ("active file %d\n" % i).encode() * 32) for i in range(3)]
small = [(path, content[:256]) for path, content in small]
if mode == "large":
    offsets = [0, size - 4096]
    expected = []
    for offset in offsets:
        block = hashlib.shake_256(("git-on-benchmark-v1:%d" % (offset // 65536)).encode()).digest(65536)
        expected.append(block[offset % 65536:offset % 65536 + 4096])
read_bytes = 0
workload_ns = time.monotonic_ns()
if mode in ("scale", "repeat"):
    for _ in range(count):
        for path, expected in small:
            with open(path, "rb", buffering=0) as source:
                actual = source.read()
            if actual != expected:
                raise RuntimeError("incorrect snapshot bytes: " + path)
            read_bytes += len(actual)
else:
    fd = os.open("large.bin", os.O_RDONLY)
    try:
        for offset, expected_bytes in zip(offsets, expected):
            actual = os.pread(fd, 4096, offset)
            if actual != expected_bytes:
                raise RuntimeError("incorrect large-file range")
            read_bytes += len(actual)
    finally:
        os.close(fd)
finished_ns = time.monotonic_ns()
print(json.dumps(dict(started_ns=started_ns, workload_ns=workload_ns, finished_ns=finished_ns,
                     application_bytes_read=read_bytes)), flush=True)
'''


def require_fuse():
    if sys.platform != "linux":
        raise RuntimeError("benchmarks require Linux (FUSE and Linux wait4 RSS units)")
    try:
        if not stat.S_ISCHR(Path("/dev/fuse").stat().st_mode):
            raise OSError("not a character device")
        fd = os.open("/dev/fuse", os.O_RDWR)
        os.close(fd)
    except OSError as error:
        raise RuntimeError(f"real FUSE unavailable: /dev/fuse: {error}; no samples measured") from error
    if not (shutil.which("fusermount3") or shutil.which("fusermount")):
        raise RuntimeError("real FUSE unavailable: fusermount3/fusermount missing; no samples measured")


def signal_session(session, signum):
    # Popen creates a private session. The workload's process group may differ
    # from the runner's, so address only processes still in that same session.
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
            if int(fields[3]) == session:
                os.kill(int(entry.name), signum)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass


def measured_process(argv, cwd, env, stdout, stderr, timeout, expected_stdout=None):
    with stdout.open("wb") as out, stderr.open("wb") as err, selectors.DefaultSelector() as selector:
        # Do not call Popen.poll()/wait(): wait4 owns child reaping and rusage.
        harness_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        start = time.monotonic_ns()
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=err, start_new_session=True)
        os.set_blocking(process.stdout.fileno(), False)
        selector.register(process.stdout, selectors.EVENT_READ)
        deadline = time.monotonic() + timeout
        timed_out = interrupted = terminating = False
        reaped = eof = False
        received = bytearray()
        stdout_match_ns = None
        while not (reaped and eof):
            try:
                if not reaped:
                    pid, status, usage = os.wait4(process.pid, os.WNOHANG)
                    reaped = bool(pid)
                for key, _ in selector.select(0.01):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(process.stdout)
                        eof = True
                    else:
                        observed_ns = time.monotonic_ns()
                        out.write(chunk)
                        if expected_stdout is not None:
                            received.extend(chunk)
                            if received == expected_stdout and stdout_match_ns is None:
                                stdout_match_ns = observed_ns
                if time.monotonic() >= deadline and not (reaped and eof):
                    if not terminating:
                        timed_out = terminating = True
                        signal_session(process.pid, signal.SIGTERM)
                        deadline = time.monotonic() + 5
                    else:
                        signal_session(process.pid, signal.SIGKILL)
                        deadline = time.monotonic() + 1
            except KeyboardInterrupt:
                interrupted = terminating = True
                signal_session(process.pid, signal.SIGTERM)
                deadline = time.monotonic() + 5
        process.stdout.close()
        process.returncode = os.waitstatus_to_exitcode(status)
        return dict(started_ns=start, observed_exit_ns=time.monotonic_ns(),
                    returncode=process.returncode, timed_out=timed_out, interrupted=interrupted,
                    stdout_match_ns=stdout_match_ns,
                    wait4_peak_rss_kib=usage.ru_maxrss, user_cpu_s=usage.ru_utime,
                    system_cpu_s=usage.ru_stime, harness_peak_rss_kib_before_spawn=harness_rss)


def make_cases(fixtures, suites, repeat_count):
    cases = []
    for fixture in fixtures:
        if fixture["suite"] in suites:
            label = (f"scale-{fixture['unused_files']}" if fixture["suite"] == "scale"
                     else f"digits-{fixture['digit_files']}" if fixture["suite"] == "digits"
                     else f"large-{fixture['large_bytes']}")
            cases.append(dict(fixture, case=label, count=1))
    if "repeat" in suites:
        base = next((f for f in fixtures if f["suite"] == "scale" and f["unused_files"] == 0), None)
        if base is None:
            raise ValueError("repeat suite requires a prepared scale fixture with --sizes including 0")
        cases.append(dict(base, suite="repeat", case=f"repeat-{repeat_count}", count=repeat_count))
    if not cases:
        raise ValueError("no prepared fixtures match the requested suites")
    return cases


def machine_metadata(repo):
    def read_optional(path):
        try:
            return Path(path).read_text().strip()
        except OSError:
            return None

    cpuinfo = read_optional("/proc/cpuinfo") or ""
    cpu_model = next((line.split(":", 1)[1].strip() for line in cpuinfo.splitlines()
                      if line.startswith("model name")), None)
    filesystem = subprocess.run(["stat", "-f", "-c", "%T", str(repo)],
                                capture_output=True, text=True, check=False).stdout.strip()
    try:
        revision = git(ROOT, "rev-parse", "HEAD").decode()
        dirty = bool(git(ROOT, "status", "--porcelain"))
    except subprocess.CalledProcessError:
        revision, dirty = None, None
    return dict(cpu_model=cpu_model, machine=platform.machine(), logical_cpus=os.cpu_count(),
                physical_memory_bytes=os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES"),
                cgroup_memory_max=read_optional("/sys/fs/cgroup/memory.max"),
                fixture_filesystem_type=filesystem or None, storage_device_model=None,
                source_revision=revision, source_worktree_dirty=dirty,
                source_revision_note="Checkout at measurement time; binary identity is its SHA-256.")


def benchmark(args):
    require_fuse()  # A real mount failure later is also a hard failure, never a skip.
    binary = args.binary.resolve(strict=True)
    source = args.fixtures.resolve(strict=True)
    fixture_manifest = json.loads((source / "fixtures.json").read_text())
    if fixture_manifest["format"] != FORMAT:
        raise ValueError("unsupported fixture format")
    repo = source / "repository"
    cases = make_cases(fixture_manifest["fixtures"], args.suites, args.repeat_count)
    for case in cases:
        if git(repo, "rev-parse", case["ref"]).decode() != case["tree_oid"]:
            raise ValueError(f"fixture reference changed: {case['ref']}")
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    cat = shutil.which("cat")
    if any(case["suite"] == "digits" for case in cases) and not cat:
        raise RuntimeError("cat executable not found")
    samples = []
    with (output / "results.jsonl").open("x") as results:
        def record(value):
            results.write(json.dumps(value, sort_keys=True) + "\n")
            results.flush()

        with binary.open("rb") as binary_file:
            binary_hash = hashlib.file_digest(binary_file, "sha256").hexdigest()
        record(dict(event="metadata", format=FORMAT, timestamp_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    platform=platform.platform(), hardware_and_source=machine_metadata(repo), python=sys.version,
                    binary=str(binary), binary_sha256=binary_hash, cat=cat,
                    binary_version=subprocess.check_output([str(binary), "--version"]).decode().strip(),
                    fixture_manifest=fixture_manifest, repeats=args.repeats, warmups=args.warmups,
                    timeout_s=args.timeout, seed=args.seed, cache_policy="uncontrolled OS cache; per-case warmups",
                    object_read_counts=None, object_read_bytes=None,
                    object_metrics_unavailable_reason="The runtime has no object-read instrumentation.",
                    cat_child_startup_unavailable_reason="Unmodified cat has no child-start marker.",
                    rss_semantics="Linux wait4 ru_maxrss in KiB; maximum process high-water including waited descendants, not concurrent sum.",
                    command_line=sys.argv))
        rng = random.Random(args.seed)
        for iteration in range(-args.warmups, args.repeats):
            ordered = cases.copy()
            rng.shuffle(ordered)
            for case in ordered:
                tag = f"{len(samples):04d}-{case['case']}"
                run = output / tag
                env = environment()
                env["GIT_ON_RUN_DIR"] = str(run)
                stdout, stderr = output / f"{tag}.stdout", output / f"{tag}.stderr"
                argv = [str(binary), case["tree_oid"], sys.executable, "-I", "-S", "-c", CHILD,
                        case["suite"], str(case["count"]), str(case.get("large_bytes", 0))]
                expected_stdout = None
                selection = {}
                if case["suite"] == "digits":
                    number = random.Random(f"{args.seed}:{case['case']}:{iteration}").randrange(case["digit_files"])
                    digits = f"{number:06d}"
                    path = "/".join(digits)
                    expected_stdout = (digits + "\n").encode()
                    selection = dict(selected_number=digits, selected_path=path, selection_seed=args.seed)
                    argv = [str(binary), case["tree_oid"], cat, path]
                measured = measured_process(argv, repo, env, stdout, stderr, args.timeout, expected_stdout)
                sample = dict(event="sample", case=case, iteration=max(0, iteration), warmup=iteration < 0,
                              evidence_dir=str(run), stdout=str(stdout), stderr=str(stderr), argv=argv,
                              **selection, **measured)
                try:
                    if measured["returncode"] or measured["timed_out"] or measured["interrupted"]:
                        raise RuntimeError("command failed, timed out, or was interrupted")
                    manifest = json.loads((run / "manifest.json").read_text())
                    if (manifest["status"]["runner_error"] or manifest["status"]["exit_code"] != 0
                            or not manifest["logs_complete"]):
                        raise RuntimeError("runner manifest does not record complete success")
                    sample.update(observed_total_ms=(measured["observed_exit_ns"] - measured["started_ns"]) / 1e6,
                                  success=True, child_startup_ms=None, workload_ms=None,
                                  child_ready_to_finish_ms=None, verified_stdout_ms=None)
                    if case["suite"] == "digits":
                        if stdout.read_bytes() != expected_stdout or measured["stdout_match_ns"] is None:
                            raise RuntimeError("cat stdout does not exactly match the selected six-digit number plus LF")
                        sample.update(verified_stdout_ms=(measured["stdout_match_ns"] - measured["started_ns"]) / 1e6,
                                      application_bytes_read=7)
                    else:
                        child = json.loads(stdout.read_text())
                        if not (measured["started_ns"] <= child["started_ns"] <= child["workload_ns"]
                                <= child["finished_ns"] <= measured["observed_exit_ns"]):
                            raise RuntimeError("invalid child monotonic timestamps")
                        sample.update(child_startup_ms=(child["started_ns"] - measured["started_ns"]) / 1e6,
                                      workload_ms=(child["finished_ns"] - child["workload_ns"]) / 1e6,
                                      child_ready_to_finish_ms=(child["finished_ns"] - child["started_ns"]) / 1e6,
                                      application_bytes_read=child["application_bytes_read"])

                except (OSError, ValueError, KeyError, RuntimeError) as error:
                    sample.update(success=False, error=str(error), child_startup_ms=None, workload_ms=None)
                    if measured["timed_out"] or measured["interrupted"]:
                        # Only the runner's checked cleanup may unmount/release its pin.
                        try:
                            cleanup = subprocess.run([str(binary), "--cleanup", str(run)], env=env,
                                                     capture_output=True, timeout=20)
                            sample["cleanup_returncode"] = cleanup.returncode
                            sample["cleanup_stderr"] = cleanup.stderr.decode(errors="replace")
                        except subprocess.TimeoutExpired:
                            sample["cleanup_error"] = "cleanup timed out; inspect retained run directory"
                record(sample)
                samples.append(sample)
                metric = "verified_stdout_ms" if case["suite"] == "digits" else "child_startup_ms"
                print(f"{tag}: " + (f"{metric}={sample[metric]:.3f}, "
                      f"wait4 RSS={sample['wait4_peak_rss_kib']} KiB" if sample["success"]
                      else f"FAILED; inspect {stderr}"), flush=True)
                if not sample["success"]:
                    return 130 if measured["interrupted"] else 1
        for summary in summarize(samples):
            record(summary)
    return 0


def summarize(samples):
    groups = defaultdict(list)
    for sample in samples:
        if sample["success"] and not sample["warmup"]:
            groups[sample["case"]["case"]].append(sample)
    for case, group in sorted(groups.items()):
        metrics = {}
        for name in ("verified_stdout_ms", "child_startup_ms", "workload_ms", "child_ready_to_finish_ms",
                     "observed_total_ms", "wait4_peak_rss_kib"):
            values = [sample[name] for sample in group if sample.get(name) is not None]
            if not values:
                continue
            metrics[name] = dict(min=min(values), median=statistics.median(values), max=max(values))
        yield dict(event="summary", case=case, samples=len(group), metrics=metrics)


class SelfTests(unittest.TestCase):
    """Infrastructure checks only; these never substitute for real mount samples."""

    def test_fixture_counts_and_reproducibility(self):
        with tempfile.TemporaryDirectory() as tmp:
            first = build_fixtures(Path(tmp) / "a", [0, 1, 101, 1000000], [1], "loose", 1000)
            second = build_fixtures(Path(tmp) / "b", [0, 1, 101, 1000000], [1], "packed", 1000)
            self.assertEqual(first["fixtures"], second["fixtures"])
            repo = Path(tmp) / "a/repository"

            @lru_cache(maxsize=None)
            def count(oid):
                files = size = 0
                for line in git(repo, "ls-tree", oid).splitlines():
                    meta, _ = line.split(b"\t", 1)
                    _, kind, child = meta.split()
                    if kind == b"tree":
                        child_files, child_size = count(child.decode())
                        files += child_files
                        size += child_size
                    else:
                        files += 1
                        size += int(git(repo, "cat-file", "-s", child.decode()))
                return files, size

            for fixture in first["fixtures"]:
                self.assertEqual(count(fixture["tree_oid"]),
                                 (fixture["logical_files"], fixture["logical_blob_bytes"]))
                git(repo, "fsck", "--strict", "--no-reflogs", fixture["tree_oid"])
            self.assertLess(count.cache_info().currsize, 130)
            digit_tree = first["fixtures"][0]["tree_oid"]
            for number in (0, 123, 999):
                digits = f"{number:06d}"
                self.assertEqual(git(repo, "show", digit_tree + ":" + "/".join(digits), strip=False), (digits + "\n").encode())

    def test_child_workloads_and_measurement_plumbing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "active").mkdir()
            for name, content in SMALL:
                (root / "active" / name).write_bytes(content)
            (root / "large.bin").write_bytes(b"".join(large_block(i) for i in range(16)))
            for mode, count, expected in [("scale", 1, 768), ("repeat", 5, 3840), ("large", 1, 8192)]:
                result = measured_process([sys.executable, "-I", "-S", "-c", CHILD, mode,
                                           str(count), str(1024 * 1024)], root, environment(),
                                          root / "out", root / "err", 5)
                self.assertEqual(result["returncode"], 0, (root / "err").read_text())
                child = json.loads((root / "out").read_text())
                self.assertEqual(child["application_bytes_read"], expected)
                self.assertGreaterEqual(child["started_ns"], result["started_ns"])
                self.assertGreater(result["wait4_peak_rss_kib"], 0)
            result = measured_process([shutil.which("cat"), "active/file-0.txt"], root, environment(),
                                      root / "out", root / "err", 5, SMALL[0][1])
            self.assertEqual((root / "out").read_bytes(), SMALL[0][1])
            self.assertIsNotNone(result["stdout_match_ns"])
            result = measured_process([sys.executable, "-c", "import time; time.sleep(10)"],
                                      root, environment(), root / "out", root / "err", 0.01)
            self.assertTrue(result["timed_out"])
            self.assertNotEqual(result["returncode"], 0)

    def test_summary_excludes_warmups_and_failures(self):
        sample = dict(case={"case": "x"}, success=True, warmup=False, child_startup_ms=3,
                      workload_ms=4, child_ready_to_finish_ms=5, observed_total_ms=6, wait4_peak_rss_kib=7)
        rows = list(summarize([sample, dict(sample, warmup=True, child_startup_ms=100),
                               dict(sample, success=False, child_startup_ms=None)]))
        self.assertEqual(rows[0]["samples"], 1)
        self.assertEqual(rows[0]["metrics"]["child_startup_ms"]["median"], 3)


def integers(value):
    try:
        values = list(dict.fromkeys(int(part) for part in value.split(",")))
        if not values or min(values) < 0:
            raise ValueError()
        return values
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected comma-separated nonnegative integers") from error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    prepare = sub.add_parser("prepare", help="generate deterministic Git objects, without timing or FUSE")
    prepare.add_argument("--directory", type=Path, required=True, help="new fixture directory")
    prepare.add_argument("--suites", nargs="+", choices=["digits", "scale", "large"], default=["digits"])
    prepare.add_argument("--digit-files", type=int, default=1000, help="1..1000000 distinct six-digit leaf files")
    prepare.add_argument("--sizes", type=integers, default=[0, 1000, 10000], help="unused logical file counts")
    prepare.add_argument("--large-mib", type=integers, default=[1, 16], help="positive large-blob sizes in MiB")
    prepare.add_argument("--storage", choices=["loose", "packed"], default="loose")
    run = sub.add_parser("run", help="measure the binary with real FUSE; no emulation or skips")
    run.add_argument("--fixtures", type=Path, required=True)
    run.add_argument("--output", type=Path, required=True, help="new directory for raw evidence and JSONL")
    run.add_argument("--binary", type=Path, default=ROOT / "target/release/git-on")
    run.add_argument("--suites", nargs="+", choices=["digits", "scale", "repeat", "large"], default=["digits"])
    run.add_argument("--repeats", type=int, default=5)
    run.add_argument("--warmups", type=int, default=1)
    run.add_argument("--repeat-count", type=int, default=1000)
    run.add_argument("--timeout", type=float, default=180, help="per-sample seconds; then TERM and KILL")
    run.add_argument("--seed", type=int, default=0, help="deterministic per-round case-order shuffle")
    sub.add_parser("self-test", help="test fixture and measurement infrastructure without claiming FUSE results")
    args = parser.parse_args()
    if args.command == "self-test":
        return not unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)).wasSuccessful()
    if args.command == "prepare":
        if any(size < 1 for size in args.large_mib):
            parser.error("--large-mib values must be positive")
        if not 1 <= args.digit_files <= 1000000:
            parser.error("--digit-files must be between 1 and 1000000")
        manifest = build_fixtures(args.directory, args.sizes if "scale" in args.suites else [],
                                  args.large_mib if "large" in args.suites else [], args.storage,
                                  args.digit_files if "digits" in args.suites else None)
        print(json.dumps(manifest, indent=2))
        return 0
    if args.repeats < 1 or args.warmups < 0 or args.repeat_count < 1 or not (0 < args.timeout < float("inf")):
        parser.error("repeats, repeat-count and timeout must be positive; warmups must be nonnegative")
    return benchmark(args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"benchmark: {error}", file=sys.stderr)
        sys.exit(2)
