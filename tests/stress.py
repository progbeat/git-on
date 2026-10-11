#!/usr/bin/env python3
"""Seeded, bounded differential syscall stress on a real git-on FUSE mount.

Normal runs require FUSE and never silently substitute a host filesystem.
--self-test checks the harness on host directories only, not the product.
See stress.md for bounds, oracle limitations, CI use, and exact trace replay.
"""
from __future__ import annotations

import argparse
import collections
import errno
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import traceback
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
VERSION = 1
FIXTURE = {
    ".gitignore": b"out/\n*.cache\n!keep.cache\n**/.gitignore\n",
    "tracked.txt": b"immutable source\x00\xff\n",
    "out/protected.txt": b"tracked despite matching out/\n",
    "plain/source.txt": b"plain source\n",
    "nested/.gitignore": b"*.tmp\n!keep.tmp\n",
    "nested/source.txt": b"nested source\n",
    "executable.sh": b"#!/bin/sh\nexit 0\n",
}
TRACKED = set(FIXTURE) | {"tracked-link", "out", "plain", "nested"}
PROTECTED_FILES = ["tracked.txt", "out/protected.txt", "plain/source.txt",
                   "nested/.gitignore", ".gitignore", "executable.sh"]


def fixture(root):
    root.mkdir(parents=True, exist_ok=True)
    for name, data in FIXTURE.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(0o755 if name == "executable.sh" else 0o644)
    (root / "tracked-link").symlink_to("tracked.txt")
    for name in ("out", "plain", "nested"):
        (root / name).chmod(0o755)


def check_path(name):
    """Replay never permits an absolute path, traversal, or host escape."""
    if not isinstance(name, str) or not name or "\x00" in name:
        raise ValueError(f"invalid relative path: {name!r}")
    if name.startswith("/") or any(part in ("", ".", "..") for part in name.split("/")):
        raise ValueError(f"unsafe relative path: {name!r}")
    if len(name.encode()) > 1024 or len(name.split("/")) > 6:
        raise ValueError(f"path exceeds harness limit: {name!r}")


def ignored(name):
    # Deliberately independent of git-on's matcher. This tiny fixture grammar
    # covers ignored ancestors, root patterns, nested rules and negations.
    parts = name.split("/")
    if parts[0] == "out":
        return True
    if any(part.endswith(".cache") and part != "keep.cache" for part in parts):
        return True
    if parts[-1] == ".gitignore":
        return True
    return (parts[0] == "nested" and
            any(part.endswith(".tmp") and part != "keep.tmp" for part in parts[1:]))


def followed_name(root, name):
    """Resolve links without ever looking up a component outside root."""
    pending = collections.deque(name.split("/"))
    resolved = []
    links = 0
    while pending:
        part = pending.popleft()
        if part in ("", "."):
            continue
        if part == "..":
            if not resolved:
                raise ValueError("symlink escaped the harness directory")
            resolved.pop()
            continue
        candidate = root.joinpath(*resolved, part)
        if candidate.is_symlink():
            links += 1
            if links > 40:
                raise OSError(errno.ELOOP, "symlink loop in harness path")
            target = os.readlink(candidate)
            if target.startswith("/"):
                raise ValueError("absolute symlink target outside harness policy")
            pending.extendleft(reversed(target.split("/")))
        else:
            resolved.append(part)
    return "/".join(resolved)


def contained_operation(root, op):
    # Namespace operations act on the link itself but still traverse parents.
    # The generated workload never traverses directory symlink ancestors.
    for key in ("path", "dest"):
        if key not in op:
            continue
        path = Path(op[key])
        for parent in reversed(list(path.parents)[:-1]):
            if (root / parent).is_symlink():
                raise ValueError("symlink ancestor is outside the harness grammar")
        followed_name(root, path.parent.as_posix())
    if op["op"] in ("read", "stat", "write", "append", "truncate", "listdir"):
        try:
            followed_name(root, op["path"])
        except OSError as error:
            if error.errno != errno.ELOOP:
                raise
            # The real syscall and host oracle must agree on ELOOP.
    if op["op"] == "rename":
        source, dest = op["path"], op["dest"]
        candidate = root / source
        links = [candidate] if candidate.is_symlink() else []
        if not candidate.is_symlink() and candidate.is_dir():
            for directory, dirs, files in os.walk(candidate, followlinks=False):
                links.extend(Path(directory) / name for name in dirs + files
                             if (Path(directory) / name).is_symlink())
        for link in links:
            moved = Path(dest) / link.relative_to(candidate) if link != candidate else Path(dest)
            target = os.readlink(link)
            normalized = os.path.normpath(str(moved.parent / target))
            check_path(normalized)
            try:
                followed_name(root, normalized)
            except OSError as error:
                if error.errno != errno.ELOOP:
                    raise


def policy_errno(root, op):
    kind, path = op["op"], op["path"]
    if kind in ("read", "stat", "lstat", "readlink", "listdir"):
        return None
    if kind in ("write", "append", "truncate"):
        path = followed_name(root, path)
        if path in TRACKED:
            return errno.EROFS
        return None  # Existing-only opens; the host supplies ENOENT/EISDIR.
    if kind in ("create", "mkdir", "symlink"):
        # O_EXCL/mkdir/symlink collide before evaluating the policy.
        if os.path.lexists(root / path):
            return None
        return None if ignored(path) else errno.EROFS
    if kind in ("unlink", "rmdir"):
        return errno.EROFS if path in TRACKED else None
    if kind in ("rename", "hardlink"):
        if not os.path.lexists(root / path):
            return None
        if path in TRACKED:
            return errno.EROFS
        dest = op["dest"]
        if kind == "hardlink" and os.path.lexists(root / dest):
            return None
        if dest in TRACKED or not ignored(dest):
            return errno.EROFS
    return None


def metadata(info, *, mutable_directory=False):
    kind = stat.S_IFMT(info.st_mode)
    result = {"type": kind, "mode": stat.S_IMODE(info.st_mode),
              "uid": info.st_uid, "gid": info.st_gid}
    if kind == stat.S_IFDIR and mutable_directory:
        result["nlink"] = info.st_nlink
    if kind != stat.S_IFDIR:
        result.update(size=info.st_size, nlink=info.st_nlink)
    return result


def syscall(root, op):
    kind, path = op["op"], root / op["path"]
    if kind in ("create", "write", "append"):
        flags = os.O_WRONLY
        if kind == "create":
            flags |= os.O_CREAT | os.O_EXCL
        elif kind == "append":
            flags |= os.O_APPEND
        fd = os.open(path, flags, 0o644)
        try:
            if kind != "append":
                os.lseek(fd, op.get("offset", 0), os.SEEK_SET)
            data = bytes.fromhex(op.get("data", ""))
            total = 0
            while total < len(data):
                count = os.write(fd, data[total:])
                if count == 0:
                    raise AssertionError("write made no progress")
                total += count
            return total
        finally:
            os.close(fd)
    if kind == "truncate":
        os.truncate(path, op["size"])
    elif kind == "read":
        fd = os.open(path, os.O_RDONLY)
        try:
            return os.pread(fd, op["length"], op["offset"]).hex()
        finally:
            os.close(fd)
    elif kind == "mkdir":
        os.mkdir(path, 0o755)
    elif kind == "rmdir":
        os.rmdir(path)
    elif kind == "unlink":
        os.unlink(path)
    elif kind == "rename":
        os.rename(path, root / op["dest"])
    elif kind == "hardlink":
        os.link(path, root / op["dest"], follow_symlinks=False)
    elif kind == "symlink":
        os.symlink(op["target"], path)
    elif kind == "readlink":
        return os.readlink(path)
    elif kind == "stat":
        return metadata(os.stat(path), mutable_directory=op["path"] not in TRACKED)
    elif kind == "lstat":
        return metadata(os.lstat(path), mutable_directory=op["path"] not in TRACKED)
    elif kind == "listdir":
        return sorted(os.listdir(path))
    else:
        raise ValueError(f"unknown operation {kind!r}")
    return None


def outcome(root, op, enforce_policy=False):
    contained_operation(root, op)
    try:
        if enforce_policy and (code := policy_errno(root, op)) is not None:
            return {"errno": code}
        return {"value": syscall(root, op)}
    except OSError as error:
        return {"errno": error.errno}


def snapshot(root, max_nodes, max_bytes):
    """Full namespace/data oracle; never follow symlinks during traversal."""
    entries, groups = {}, collections.defaultdict(list)

    def walk(directory):
        for entry in sorted(os.scandir(directory), key=lambda item: item.name):
            path = Path(entry.path)
            name = path.relative_to(root).as_posix()
            info = entry.stat(follow_symlinks=False)
            item = metadata(info, mutable_directory=name not in TRACKED)
            if stat.S_ISREG(info.st_mode):
                if info.st_size > max_bytes:
                    raise AssertionError(f"file exceeds harness bound: {name}, {info.st_size}")
                item["data"] = path.read_bytes().hex()
                groups[(info.st_dev, info.st_ino)].append(name)
            elif stat.S_ISLNK(info.st_mode):
                item["target"] = os.readlink(path)
                groups[(info.st_dev, info.st_ino)].append(name)
            elif not stat.S_ISDIR(info.st_mode):
                raise AssertionError(f"unexpected special inode: {name}")
            entries[name] = item
            if len(entries) > max_nodes + len(TRACKED):
                raise AssertionError("namespace exceeds harness node bound")
            if stat.S_ISDIR(info.st_mode):
                walk(path)
    walk(root)
    # Inode numbers vary across filesystems; alias equivalence must not.
    return entries, sorted(sorted(names) for names in groups.values())


def diagnostic(value):
    if isinstance(value, str) and len(value) > 256:
        return {"length": len(value), "sha256": hashlib.sha256(value.encode()).hexdigest(),
                "prefix": value[:64]}
    if isinstance(value, dict):
        return {key: diagnostic(item) for key, item in value.items()}
    if isinstance(value, list):
        return [diagnostic(item) for item in value]
    return value


def compare(expected, actual, label):
    if expected != actual:
        if isinstance(expected, tuple) and isinstance(actual, tuple):
            left, right = expected[0], actual[0]
            differing = [name for name in sorted(left.keys() | right.keys())
                         if left.get(name) != right.get(name)]
            detail = [{"path": name, "expected": left.get(name), "actual": right.get(name)}
                      for name in differing[:3]]
            if not detail:
                detail = [{"expected_links": expected[1], "actual_links": actual[1]}]
        else:
            detail = {"expected": expected, "actual": actual}
        # Keep terminal diagnostics bounded; full operation bytes are in JSONL.
        raise AssertionError(f"{label}: {json.dumps(diagnostic(detail), sort_keys=True)[:6000]}")


def validate_op(op, max_bytes):
    check_path(op["path"])
    if "dest" in op:
        check_path(op["dest"])
    if op["op"] == "symlink":
        # Targets may use .. to refer to siblings, but are verified relative to
        # their link before creation. Absolute targets are never accepted.
        target = op["target"]
        if not isinstance(target, str) or target.startswith("/") or "\x00" in target:
            raise ValueError("unsafe symlink target")
        normalized = os.path.normpath(str(Path(op["path"]).parent / target))
        check_path(normalized)
    data = bytes.fromhex(op.get("data", ""))
    if len(data) > 1024:
        raise ValueError("write payload exceeds 1024-byte harness bound")
    for key in ("size", "offset", "length"):
        value = op.get(key, 0)
        if not isinstance(value, int) or not 0 <= value <= max_bytes:
            raise ValueError(f"{key} outside harness bound")
    if op.get("offset", 0) + len(data) > max_bytes:
        raise ValueError("write end outside harness bound")


class Sequence:
    def __init__(self, oracle, seed, max_nodes, max_bytes):
        self.root, self.rng = oracle, random.Random(seed)
        self.max_nodes, self.max_bytes = max_nodes, max_bytes
        self.serial = 0

    def point(self):
        edges = [0, 1, 2, 63, 64, 255, 256, 4095, 4096, 4097,
                 self.max_bytes - 1, self.max_bytes]
        return self.rng.choice([n for n in edges if n <= self.max_bytes]) if self.rng.random() < .7 else self.rng.randrange(self.max_bytes + 1)

    def data(self, limit=1024):
        length = self.rng.choice([0, 1, 2, 17, 63, 255, 256, 1024])
        return bytes(self.rng.randrange(256) for _ in range(min(length, limit))).hex()

    def fresh(self, directories, category=None):
        self.serial += 1
        # Reuse some slots deliberately: collisions are part of the oracle.
        token = str(self.rng.randrange(16)) if self.rng.random() < .25 else str(self.serial)
        stem = self.rng.choice(["file", "sp ace", "unicodé", ".hidden", "-dash"]) + token
        category = category or self.rng.choice(["out"] * 6 + ["root", "nested"])
        if category == "root":
            return stem + ".cache"
        if category == "nested":
            return "nested/" + stem + ".tmp"
        base = self.rng.choice(["out"] + [d for d in directories if d.startswith("out/") and d.count("/") < 3])
        return base + "/" + stem

    def prelude(self):
        # Known boundaries and policy checks complement, but do not replace,
        # genuinely state-dependent random operations below.
        yield {"op": "mkdir", "path": "out/start"}
        yield {"op": "mkdir", "path": "out/start/child"}
        yield {"op": "stat", "path": "out/start"}
        yield {"op": "rmdir", "path": "out/start/child"}
        yield {"op": "create", "path": "out/start/a", "data": "0001ff"}
        yield {"op": "rmdir", "path": "out/start/a"}
        yield {"op": "unlink", "path": "out/start"}
        yield {"op": "read", "path": "out/start", "offset": 0, "length": 1}
        yield {"op": "create", "path": "out/start/a/child", "data": "ff"}
        yield {"op": "write", "path": "out/start/a", "offset": 4095, "data": "aabbcc"}
        yield {"op": "truncate", "path": "out/start/a", "size": self.max_bytes}
        yield {"op": "hardlink", "path": "out/start/a", "dest": "out/start/b"}
        yield {"op": "append", "path": "out/start/b", "data": ""}
        yield {"op": "truncate", "path": "out/start/b", "size": 3}
        yield {"op": "append", "path": "out/start/b", "data": "ff00"}
        yield {"op": "read", "path": "out/start/a", "offset": 0, "length": 20}
        yield {"op": "rename", "path": "out/start/a", "dest": "out/start/b"}
        yield {"op": "symlink", "path": "out/start/link", "target": "a"}
        yield {"op": "write", "path": "out/start/link", "offset": 1, "data": "42"}
        yield {"op": "symlink", "path": "out/start/source-link", "target": "../../tracked.txt"}
        yield {"op": "write", "path": "out/start/source-link", "offset": 0, "data": "42"}
        yield {"op": "symlink", "path": "out/start/dangling", "target": "missing"}
        yield {"op": "read", "path": "out/start/dangling", "offset": 0, "length": 1}
        yield {"op": "readlink", "path": "out/start/dangling"}
        yield {"op": "rmdir", "path": "out/start"}  # ENOTEMPTY, no mutation
        yield {"op": "create", "path": "out/start/a", "data": "ff"}  # EEXIST
        yield {"op": "mkdir", "path": "out/empty"}
        yield {"op": "rmdir", "path": "out/empty"}
        yield {"op": "rename", "path": "out/start", "dest": "out/moved"}
        yield {"op": "create", "path": "root.cache", "data": "01"}
        yield {"op": "create", "path": "nested/local.tmp", "data": "02"}
        yield {"op": "create", "path": "plain/.gitignore", "data": "2a0a"}
        yield {"op": "create", "path": "plain/new-source.py", "data": "03"}
        yield {"op": "create", "path": "keep.cache", "data": "03"}
        yield {"op": "create", "path": "nested/keep.tmp", "data": "03"}
        for path in PROTECTED_FILES:
            yield {"op": "write", "path": path, "offset": 0, "data": "bad0"}
            yield {"op": "unlink", "path": path}
            yield {"op": "rename", "path": path, "dest": "out/forbidden-move"}
            yield {"op": "hardlink", "path": path, "dest": "out/forbidden-alias"}
        yield {"op": "rename", "path": "root.cache", "dest": "tracked.txt"}
        yield {"op": "rename", "path": "root.cache", "dest": "disallowed.txt"}
        yield {"op": "unlink", "path": "out/moved/b"}
        yield {"op": "stat", "path": "out/moved/a"}
        yield {"op": "lstat", "path": "tracked-link"}
        yield {"op": "listdir", "path": "out"}

    def next(self):
        entries, _ = snapshot(self.root, self.max_nodes, self.max_bytes)
        mutable = [p for p in entries if p not in TRACKED]
        files = [p for p in mutable if entries[p]["type"] == stat.S_IFREG]
        dirs = [p for p in mutable if entries[p]["type"] == stat.S_IFDIR]
        links = [p for p in mutable if entries[p]["type"] == stat.S_IFLNK]
        regular = [p for p in entries if entries[p]["type"] == stat.S_IFREG]
        rng = self.rng
        choices = (["create"] * 5 + ["write"] * 5 + ["append", "truncate"] * 3 +
                   ["read"] * 4 + ["rename"] * 3 + ["unlink"] * 3 +
                   ["mkdir", "rmdir", "hardlink", "symlink", "stat", "lstat", "listdir", "policy"])
        kind = rng.choice(choices)
        if len(mutable) >= self.max_nodes - 2 and kind in ("create", "mkdir", "hardlink", "symlink"):
            kind = rng.choice(["unlink", "rmdir", "write", "truncate", "read"])
        fresh = lambda: self.fresh(dirs)
        if kind == "policy":
            path = rng.choice(PROTECTED_FILES)
            return rng.choice([
                {"op": "write", "path": path, "offset": 0, "data": self.data(16)},
                {"op": "truncate", "path": path, "size": rng.randrange(16)},
                {"op": "unlink", "path": path},
                {"op": "rename", "path": path, "dest": fresh()},
                {"op": "hardlink", "path": path, "dest": fresh()},
                {"op": "create", "path": rng.choice(["plain/new-source.py", "keep.cache", "nested/keep.tmp", "denied.txt"]), "data": self.data(16)},
            ])
        if kind in ("create", "mkdir"):
            return {"op": kind, "path": fresh(), **({"data": self.data()} if kind == "create" else {})}
        if kind in ("write", "append", "truncate"):
            if not files:
                return {"op": "read", "path": "tracked.txt", "offset": 0, "length": 32}
            path = rng.choice(files)
            if kind == "truncate":
                return {"op": kind, "path": path, "size": self.point()}
            offset = self.point() if kind == "write" else entries[path]["size"]
            return {"op": kind, "path": path, "offset": offset,
                    "data": self.data(self.max_bytes - offset)}
        if kind == "read":
            return {"op": kind, "path": rng.choice(regular + links + ["out/absent"]),
                    "offset": self.point(), "length": self.point()}
        if kind in ("stat", "lstat"):
            return {"op": kind, "path": rng.choice(sorted(entries) + ["out/absent"])}
        if kind == "listdir":
            return {"op": kind, "path": rng.choice(["out", "nested", "plain"] + dirs)}
        if kind == "unlink":
            return {"op": kind, "path": rng.choice(files + links + ["out/absent"])}
        if kind == "rmdir":
            return {"op": kind, "path": rng.choice(dirs + ["out/absent"])}
        if kind == "rename":
            sources = files + links + dirs
            if not sources:
                return {"op": "stat", "path": "tracked.txt"}
            source = rng.choice(sources)
            targets = files if source in files else links if source in links else dirs
            proposed = fresh()
            if source in links or source in dirs:
                # Relative symlinks inside moved trees retain their depth.
                targets = [p for p in targets if p.count("/") == source.count("/")]
                if proposed.count("/") != source.count("/"):
                    proposed = str(Path(source).parent / Path(proposed).name)
                if not ignored(proposed):
                    proposed = source
            dest = rng.choice(targets + [proposed] * 3 + [source])
            return {"op": kind, "path": source, "dest": dest}
        if kind == "hardlink":
            return {"op": kind, "path": rng.choice(files or ["out/absent"]), "dest": fresh()}
        if kind == "symlink":
            path = fresh()
            target = rng.choice(files + PROTECTED_FILES + ["out/absent"])
            return {"op": kind, "path": path,
                    "target": os.path.relpath(target, str(Path(path).parent))}
        raise AssertionError(kind)


def load_trace(path):
    if path.stat().st_size > 64 * 1024 * 1024:
        raise ValueError("trace exceeds 64 MiB harness bound")
    records = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    header = records[0]
    if header.get("version") != VERSION or header.get("type") != "header":
        raise ValueError("unsupported trace format")
    operations = [record["operation"] for record in records if record.get("type") == "operation"]
    if len(operations) > 10000:
        raise ValueError("trace exceeds operation bound")
    return header, operations


def trace_result(result):
    """Keep read diagnostics compact; compare raw results before encoding."""
    value = result.get("value")
    if isinstance(value, str) and len(value) > 256:
        return {"value_length": len(value),
                "value_sha256": hashlib.sha256(value.encode()).hexdigest(),
                "value_prefix": value[:64]}
    return result


def exercise(actual, oracle, config, trace, replay=None, host_policy=False):
    generator = Sequence(oracle, config["seed"], config["max_nodes"], config["max_bytes"])
    prelude = iter(generator.prelude())
    counts, errors = collections.Counter(), collections.Counter()
    start = time.monotonic()
    header = {"type": "header", "version": VERSION, **config}
    with trace.open("w", buffering=1) as journal:
        journal.write(json.dumps(header, sort_keys=True) + "\n")
        compare(snapshot(oracle, config["max_nodes"], config["max_bytes"]),
                snapshot(actual, config["max_nodes"], config["max_bytes"]), "initial snapshot")
        count = len(replay) if replay is not None else config["operations"]
        for index in range(count):
            if time.monotonic() - start > config["timeout"]:
                raise TimeoutError(f"stress timeout before operation {index}")
            op = replay[index] if replay is not None else next(prelude, None)
            if op is None:
                op = generator.next()
            validate_op(op, config["max_bytes"])
            contained_operation(oracle, op)
            if op["op"] == "append":
                try:
                    size = (oracle / followed_name(oracle, op["path"])).stat().st_size
                except OSError:
                    size = 0  # Let the exact syscall error oracle handle it.
                if size + len(bytes.fromhex(op.get("data", ""))) > config["max_bytes"]:
                    raise ValueError("append exceeds harness file bound")
            if op["op"] in ("create", "mkdir", "hardlink", "symlink"):
                destination = op.get("dest", op["path"])
                if not os.path.lexists(oracle / destination):
                    entries, _ = snapshot(oracle, config["max_nodes"], config["max_bytes"])
                    if len(entries) - len(TRACKED) >= config["max_nodes"]:
                        raise ValueError("operation exceeds harness node bound")
            # Persist before either filesystem is touched, including hangs/crashes.
            journal.write(json.dumps({"type": "operation", "index": index, "operation": op}, sort_keys=True) + "\n")
            expected = outcome(oracle, op, enforce_policy=True)
            actual_result = outcome(actual, op, enforce_policy=host_policy)
            label = f"seed={config['seed']} operation={index} {op}"
            compare(expected, actual_result, label)
            compare(snapshot(oracle, config["max_nodes"], config["max_bytes"]),
                    snapshot(actual, config["max_nodes"], config["max_bytes"]), label)
            counts[op["op"]] += 1
            if "errno" in expected:
                errors[errno.errorcode[expected["errno"]]] += 1
            journal.write(json.dumps({"type": "result", "index": index, "expected": trace_result(expected),
                                      "actual": trace_result(actual_result)}, sort_keys=True) + "\n")
        summary = {"type": "summary", "operations": count, "counts": dict(counts),
                   "errors": dict(errors), "seconds": round(time.monotonic() - start, 3)}
        journal.write(json.dumps(summary, sort_keys=True) + "\n")
        return summary


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.PIPE).decode().strip()


def child(args):
    config = json.loads(args.child)
    replay = load_trace(args.replay)[1] if args.replay else None
    # Outer process also enforces a hard deadline, including a blocked syscall.
    def expired(_signal, _frame):
        raise TimeoutError("child stress deadline")

    signal.signal(signal.SIGALRM, expired)
    signal.alarm(config["timeout"])
    try:
        summary = exercise(Path.cwd(), args.oracle, config, args.trace, replay)
    finally:
        signal.alarm(0)
    print(json.dumps({"mode": "REAL_FUSE", "seed": config["seed"], **summary}, sort_keys=True), flush=True)


def run_seed(args, seed, replay=None):
    config = {"seed": seed, "operations": args.operations, "max_nodes": args.max_nodes,
              "max_bytes": args.max_file_bytes, "timeout": args.timeout}
    trace = args.trace_dir.resolve() / f"seed-{seed}.jsonl"
    # A replay must never be overwritten by its output journal.
    if replay and trace == replay.resolve():
        trace = trace.with_name(trace.stem + "-replay.jsonl")
    with tempfile.TemporaryDirectory(prefix="git-on-stress-") as temp:
        root = Path(temp)
        repo, oracle, runtime = root / "repo", root / "oracle", root / "runtime"
        fixture(repo)
        fixture(oracle)
        runtime.mkdir()
        git(repo, "init", "--quiet")
        git(repo, "config", "user.name", "Stress Test")
        git(repo, "config", "user.email", "stress@example.invalid")
        git(repo, "add", "--force", "--all")
        git(repo, "commit", "--quiet", "-m", "stress fixture")
        tree = git(repo, "rev-parse", "HEAD^{tree}")
        # Deliberately conflicting live and info-exclude policy. The mounted
        # command must keep using the selected tree's policy and source bytes.
        (repo / ".gitignore").write_text("*\n")
        (repo / ".git/info/exclude").write_text("*\n")
        (repo / "tracked.txt").write_text("dirty live source\n")
        env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_ON_")}
        env.update(TMPDIR=str(runtime), GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                   PYTHONDONTWRITEBYTECODE="1")
        command = [str(args.binary.resolve()), tree, sys.executable, str(Path(__file__).resolve()),
                   "--child", json.dumps(config), "--oracle", str(oracle), "--trace", str(trace)]
        if replay:
            command += ["--replay", str(replay.resolve())]
        print(f"REAL_FUSE seed={seed} operations={args.operations} trace={trace}", flush=True)
        process = subprocess.Popen(command, cwd=repo, env=env, start_new_session=True)
        try:
            status = process.wait(timeout=args.timeout + 15)
        except subprocess.TimeoutExpired:
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)
            raise TimeoutError(f"runner deadline exceeded; seed={seed}; trace={trace}")
        if status:
            raise AssertionError(f"real-FUSE stress failed: status={status}; seed={seed}; trace={trace}")
        if list(runtime.iterdir()):
            raise AssertionError(f"runner leaked temporary state: {list(runtime.iterdir())}")
        if git(repo, "for-each-ref", "--format=%(refname)", "refs/git-on/"):
            raise AssertionError("runner leaked tree pin")
        if (repo / "tracked.txt").read_text() != "dirty live source\n":
            raise AssertionError("mount modified the live repository")


class HarnessTests(unittest.TestCase):
    def test_ignore_fixture_grammar(self):
        for name in ["out/x", "a.cache", "a.cache/x", "nested/a.tmp", "nested/a.tmp/x", "plain/.gitignore"]:
            self.assertTrue(ignored(name), name)
        for name in ["keep.cache", "plain/x", "nested/keep.tmp", "a.tmp", "nested/x"]:
            self.assertFalse(ignored(name), name)

    def test_reject_unsafe_replay_paths(self):
        for name in ["/tmp/escape", "../escape", "out/../../escape", "out//a", "out/./a"]:
            with self.assertRaises(ValueError):
                validate_op({"op": "create", "path": name}, 65536)
        with self.assertRaises(ValueError):
            validate_op({"op": "symlink", "path": "out/link", "target": "../../escape"}, 65536)

    def test_detect_bytes_namespace_metadata_and_links(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            fixture(root)
            before = snapshot(root, 64, 65536)
            for change, restore in [
                (lambda: (root / "tracked.txt").write_bytes(b"bad"), lambda: (root / "tracked.txt").write_bytes(FIXTURE["tracked.txt"])),
                (lambda: (root / "tracked.txt").chmod(0o600), lambda: (root / "tracked.txt").chmod(0o644)),
                (lambda: os.link(root / "tracked.txt", root / "out/alias"), lambda: (root / "out/alias").unlink()),
                (lambda: (root / "out/empty").mkdir(), lambda: (root / "out/empty").rmdir()),
            ]:
                change()
                with self.assertRaises(AssertionError):
                    compare(before, snapshot(root, 64, 65536), "injected corruption")
                restore()
                compare(before, snapshot(root, 64, 65536), "restored")

    def test_relocated_symlink_cannot_read_external_sentinel(self):
        with tempfile.TemporaryDirectory() as temp:
            outer = Path(temp)
            root = outer / "one/two"
            fixture(root)
            (outer / "tracked.txt").write_bytes(b"outside-root sentinel")
            (root / "out/start").mkdir()
            (root / "out/start/source-link").symlink_to("../../tracked.txt")
            op = {"op": "rename", "path": "out/start/source-link", "dest": "moved.cache"}
            with self.assertRaises(ValueError):
                outcome(root, op, enforce_policy=True)
            # Check an already-relocated or hand-edited replay separately.
            (root / "moved.cache").symlink_to("../../tracked.txt")
            for operation in ("read", "stat", "write", "append", "truncate"):
                with self.assertRaises(ValueError):
                    outcome(root, {"op": operation, "path": "moved.cache", "offset": 0,
                                   "length": 100, "data": "aa", "size": 0})
            (root / "out/ancestor").symlink_to("../../..")
            with self.assertRaises(ValueError):
                outcome(root, {"op": "create", "path": "out/ancestor/escape", "data": "aa"})
            self.assertEqual((outer / "tracked.txt").read_bytes(), b"outside-root sentinel")

    def test_errno_comparison_is_exact(self):
        with self.assertRaises(AssertionError):
            compare({"errno": errno.EROFS}, {"errno": errno.EIO}, "injected wrong errno")
        with self.assertRaises(AssertionError):
            compare({"errno": errno.EROFS}, {"value": None}, "injected policy bypass")

    def test_failure_trace_keeps_the_failing_operation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            actual, oracle = root / "actual", root / "oracle"
            fixture(actual)
            fixture(oracle)
            trace = root / "failure.jsonl"
            real_syscall = syscall

            def lost_write(directory, operation):
                if directory == actual and operation["op"] == "write":
                    return len(bytes.fromhex(operation["data"]))
                return real_syscall(directory, operation)

            config = {"seed": 9, "operations": 100, "max_nodes": 32,
                      "max_bytes": 65536, "timeout": 10}
            with mock.patch.dict(globals(), {"syscall": lost_write}):
                with self.assertRaisesRegex(AssertionError, "seed=9 operation="):
                    exercise(actual, oracle, config, trace, host_policy=True)
            header, operations = load_trace(trace)
            self.assertEqual(header["seed"], 9)
            self.assertEqual(operations[-1]["op"], "write")
            self.assertEqual(operations[-1]["offset"], 4095)
            self.assertFalse(any(json.loads(line).get("type") == "summary"
                                 for line in trace.read_text().splitlines()))

    def test_seeded_host_oracle_and_trace_replay(self):
        for seed in (0, 1, 24301):
            with self.subTest(seed=seed), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                a, b = root / "a", root / "b"
                fixture(a)
                fixture(b)
                config = {"seed": seed, "operations": 300, "max_nodes": 48,
                          "max_bytes": 65536, "timeout": 30}
                first = root / "first.jsonl"
                summary = exercise(a, b, config, first, host_policy=True)
                self.assertGreaterEqual(len(summary["counts"]), 14)
                header, operations = load_trace(first)
                self.assertEqual(header["seed"], seed)
                shutil.rmtree(a)
                shutil.rmtree(b)
                fixture(a)
                fixture(b)
                second = root / "second.jsonl"
                exercise(a, b, config, second, replay=operations, host_policy=True)
                self.assertEqual(load_trace(first)[1], load_trace(second)[1])
                # Regenerating from the seed also yields the identical sequence.
                shutil.rmtree(a)
                shutil.rmtree(b)
                fixture(a)
                fixture(b)
                third = root / "third.jsonl"
                exercise(a, b, config, third, host_policy=True)
                self.assertEqual(operations, load_trace(third)[1])


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--binary", type=Path, default=ROOT / "target/debug/git-on")
    result.add_argument("--seed", type=lambda value: int(value, 0), action="append",
                        help="repeat for multiple independent mounts (default: 0x5eed, 0xc0ffee)")
    result.add_argument("--operations", type=int, default=400)
    result.add_argument("--max-nodes", type=int, default=64)
    result.add_argument("--max-file-bytes", type=int, default=65536)
    result.add_argument("--timeout", type=int, default=90, help="seconds per seed")
    result.add_argument("--trace-dir", type=Path, default=ROOT / "stress-artifacts")
    result.add_argument("--replay", type=Path, help="replay the exact operations from a JSONL trace")
    result.add_argument("--require-fuse", action="store_true", help="accepted for CI clarity; normal runs always require FUSE")
    result.add_argument("--self-test", action="store_true", help="host-only harness checks; NOT real-FUSE evidence")
    result.add_argument("--child", help=argparse.SUPPRESS)
    result.add_argument("--oracle", type=Path, help=argparse.SUPPRESS)
    result.add_argument("--trace", type=Path, help=argparse.SUPPRESS)
    return result


def main():
    os.umask(0o022)
    args = parser().parse_args()
    if args.self_test:
        print("HARNESS SELF-TEST ONLY: host filesystems, no git-on mount", flush=True)
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(HarnessTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if args.child:
        child(args)
        return 0
    if args.replay:
        header, operations = load_trace(args.replay)
        args.seed = [header["seed"]]
        args.operations = len(operations)
        args.max_nodes, args.max_file_bytes = header["max_nodes"], header["max_bytes"]
    if not 1 <= args.operations <= 10000 or not 16 <= args.max_nodes <= 256:
        raise ValueError("operations must be 1..10000 and max-nodes 16..256")
    if not 8192 <= args.max_file_bytes <= 1048576 or not 5 <= args.timeout <= 600:
        raise ValueError("max-file-bytes must be 8192..1048576 and timeout 5..600")
    seeds = args.seed or [0x5EED, 0xC0FFEE]
    if len(seeds) > 16 or any(not isinstance(seed, int) or not 0 <= seed < 2**64 for seed in seeds):
        raise ValueError("use 1..16 seeds, each an unsigned 64-bit integer")
    if not Path("/dev/fuse").exists() or not (shutil.which("fusermount3") or shutil.which("fusermount")):
        raise RuntimeError("real FUSE is required: /dev/fuse and fusermount3 are unavailable; no fallback/skip")
    if not args.binary.is_file():
        raise FileNotFoundError(args.binary)
    args.trace_dir.mkdir(parents=True, exist_ok=True)
    for seed in seeds:
        run_seed(args, seed, args.replay)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        traceback.print_exc()
        sys.exit(1)
