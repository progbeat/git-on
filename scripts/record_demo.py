#!/usr/bin/env python3
"""Run the SL VHS tape while verifying real FUSE execution and cleanup.

Requires VHS 0.12.1, ttyd, ffmpeg, Chromium, Source Code Pro,
make/gcc/ncurses headers,
Git 2.45.1+, and a usable FUSE device. Read upstream source before recording.
Successful recordings appear together under OUTPUT/capture/.
Failed recordings retain their staging directory and logs for inspection;
use a fresh output directory when retrying.
"""
import argparse
from contextlib import ExitStack
import ctypes
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = "https://github.com/mtoyoda/sl.git"
COMMIT = "923e7d7ebc5c1f009755bdeb789ac25658ccce03"
TREE = "56a5fd07310088f1e6372d81455c8489c66ea00b"


def rename_noreplace(source, destination, directory_fd):
    """Publish a directory atomically without replacing an existing entry."""
    libc = ctypes.CDLL(None, use_errno=True)
    try:
        renameat2 = libc.renameat2
    except AttributeError:
        raise RuntimeError("Recording publication requires Linux renameat2") from None
    renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p,
                         ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    renameat2.restype = ctypes.c_int
    if renameat2(directory_fd, os.fsencode(source), directory_fd,
                 os.fsencode(destination), 1) != 0:  # RENAME_NOREPLACE
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), destination)


def output(*argv):
    return subprocess.check_output(argv, text=True).strip()


class RecordingDestination:
    def __init__(self, path):
        self.path = path
        self.fd = None
        self.published = False

    def refuse(self):
        return RuntimeError(f"Refusing to overwrite recording artifacts: destination changed or is not empty: {self.path}")

    def __enter__(self):
        if self.path.exists() or self.path.is_symlink():
            if self.path.is_symlink() or not self.path.is_dir() or any(self.path.iterdir()):
                raise self.refuse()
            self.fd = os.open(self.path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                self.check()
            except BaseException:
                os.close(self.fd)
                self.fd = None
                raise
        return self

    def __exit__(self, *error):
        if self.fd is not None:
            os.close(self.fd)

    def check(self, staging_name=None, require_empty=True):
        try:
            current, pinned = self.path.lstat(), os.fstat(self.fd)
        except FileNotFoundError:
            raise self.refuse() from None
        if not stat.S_ISDIR(current.st_mode) or (current.st_dev, current.st_ino) != (pinned.st_dev, pinned.st_ino):
            raise self.refuse()
        if require_empty and any(name != staging_name for name in os.listdir(self.fd)):
            raise self.refuse()

    def reserve(self):
        if self.fd is None:
            try:
                self.path.mkdir(parents=True)
            except FileExistsError:
                raise self.refuse() from None
            self.fd = os.open(self.path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        self.check()

    def check_staging_entry(self, staging_fd, name):
        try:
            current = os.stat(name, dir_fd=self.fd, follow_symlinks=False)
        except FileNotFoundError:
            raise self.refuse() from None
        pinned = os.fstat(staging_fd)
        if not stat.S_ISDIR(current.st_mode) or (current.st_dev, current.st_ino) != (pinned.st_dev, pinned.st_ino):
            raise self.refuse()

    def publish(self, staging_fd, staging_name):
        self.check(staging_name)
        for name in os.listdir(staging_fd):
            source = os.stat(name, dir_fd=staging_fd, follow_symlinks=False)
            if not stat.S_ISREG(source.st_mode):
                raise RuntimeError(f"Unsupported recording artifact: {name}")
        self.check_staging_entry(staging_fd, staging_name)
        try:
            rename_noreplace(staging_name, "capture", self.fd)
        except FileExistsError:
            raise self.refuse() from None
        # A different directory can replace the source name during rename.
        # Detect that after publication without rolling back foreign entries.
        self.check_staging_entry(staging_fd, "capture")
        self.published = True
        self.check(require_empty=False)

    def report_retained_staging(self, staging_fd, name):
        if not self.published:
            try:
                retained = os.readlink(f"/proc/self/fd/{staging_fd}")
            except OSError:
                retained = str(self.path / name)
            print(f"Recording artifacts retained at {retained}; inspect vhs.log if present; "
                  "use a fresh --output directory for retry.", file=sys.stderr)


def record(binary, destination):
    with RecordingDestination(destination) as target:
        _record(binary, target)


def _record(binary, target):
    for tool in ("vhs", "ttyd", "ffmpeg", "git", "make", "gcc", "fusermount3"):
        if shutil.which(tool) is None:
            raise RuntimeError(f"Missing recording dependency: {tool}")
    fd = os.open("/dev/fuse", os.O_RDWR)
    os.close(fd)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"Build git-on first: {binary}")
    target.reserve()
    info = {
        "recorded_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "host": platform.node(),
        "platform": platform.platform(),
        "upstream": UPSTREAM,
        "upstream_commit": COMMIT,
        "executed_revision": COMMIT,
        "git_on_version": output(str(binary), "--version"),
        "git_on_binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "git": output("git", "--version"),
        "vhs": output("vhs", "--version"),
        "tape_sha256": hashlib.sha256((ROOT / "assets/demo.tape").read_bytes()).hexdigest(),
    }
    with tempfile.TemporaryDirectory(prefix="git-on-demo-") as temporary, ExitStack() as cleanup:
        private = Path(temporary)
        staging = Path(tempfile.mkdtemp(prefix=".git-on-recording-", dir=f"/proc/self/fd/{target.fd}"))
        staging_fd = os.open(staging.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=target.fd)
        cleanup.callback(os.close, staging_fd)
        # Failure artifacts stay intact. The directory FD reports its actual
        # location even if another writer renames it or reuses its old name.
        cleanup.callback(target.report_retained_staging, staging_fd, staging.name)
        tools = private / "bin"
        tools.mkdir()
        (tools / "git-on").symlink_to(binary)
        runtime = private / "runtime"
        runtime.mkdir()
        status = private / "shell-status"
        # The clean VHS Bash records its actual command status after each prompt.
        # This does not change the hero command or any of its terminal output.
        env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                   TMPDIR=str(runtime), GIT_ON_DEMO_STATUS=str(status),
                   PROMPT_COMMAND='printf "%s\\n" "$?" >> "$GIT_ON_DEMO_STATUS"',
                   LC_ALL="C.UTF-8")
        observed = set()
        generated = False
        log_fd = os.open("vhs.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=staging_fd)
        with os.fdopen(log_fd, "w") as stream:
            # The pinned staging cwd stays private even if the requested output
            # pathname is replaced. Publication never replaces existing files.
            process = subprocess.Popen(["vhs", str(ROOT / "assets/demo.tape")],
                                       cwd=f"/proc/self/fd/{staging_fd}", pass_fds=(staging_fd,),
                                       env=env, stdout=stream, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 240
                while process.poll() is None:
                    if time.monotonic() > deadline:
                        raise RuntimeError("VHS recording exceeded four minutes")
                    for state_path in runtime.glob("git-on-*/.state"):
                        try:
                            state = json.loads(state_path.read_text())
                        except (FileNotFoundError, json.JSONDecodeError):
                            continue
                        run = state_path.parent
                        if (run / "mount/sl").is_file():
                            if run not in observed:
                                tree = output("git", "--git-dir=" + str(run / "remote-repository"),
                                              "rev-parse", COMMIT + "^{tree}")
                                if state["tree_oid"] != tree or tree != TREE:
                                    raise RuntimeError("Running source differs from the reviewed upstream commit")
                                info["upstream_tree"] = tree
                                observed.add(run)
                            generated |= os.access(run / "mount/sl", os.X_OK)
                    time.sleep(0.02)
                if process.returncode:
                    raise RuntimeError(f"VHS failed with status {process.returncode}; recording artifacts were not published")
                if not generated or len(observed) != 1:
                    raise RuntimeError("No single real mounted SL build/execution was observed")
                statuses = status.read_text().splitlines() if status.exists() else []
                if statuses != ["0", "0"]:
                    raise RuntimeError(f"The recorded shell did not return successfully: {statuses}")
                if list(runtime.iterdir()) or any(run.exists() for run in observed):
                    raise RuntimeError("SL completion left its mounted snapshot or generated executable behind")
                info["generated_sl_removed_with_snapshot"] = True
                info["recorded_command_exit_status"] = 0
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=10)
        gif_fd = os.open("demo.gif", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=staging_fd)
        with os.fdopen(gif_fd, "rb") as gif:
            if not stat.S_ISREG(os.fstat(gif.fileno()).st_mode):
                raise RuntimeError("Unsupported recording artifact: demo.gif")
            contents = gif.read()
        size = len(contents)
        info["gif_bytes"] = size
        info["gif_sha256"] = hashlib.sha256(contents).hexdigest()
        metadata_fd = os.open("demo.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                              0o600, dir_fd=staging_fd)
        with os.fdopen(metadata_fd, "w") as metadata:
            metadata.write(json.dumps(info, indent=2) + "\n")
        if size > 5 * 1024 * 1024:
            raise RuntimeError("GIF exceeds the 5 MiB repository-asset budget: do not commit it or open a PR")
        target.publish(staging_fd, staging.name)
    print(f"Recorded actual SL/FUSE execution on {info['host']}: {size:,} bytes in {target.path / 'capture'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/git-on")
    parser.add_argument("--output", type=Path, default=ROOT / "target/sl-recording",
                        help="New or empty parent directory; artifacts appear in capture/ (default: target/sl-recording)")
    args = parser.parse_args()
    record(args.binary.resolve(), args.output.resolve())
