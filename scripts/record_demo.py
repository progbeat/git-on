#!/usr/bin/env python3
"""Run the SL VHS tape while verifying real FUSE execution and cleanup.

Requires VHS 0.12.1, ttyd, ffmpeg, Chromium, Source Code Pro,
make/gcc/ncurses headers,
Git 2.45.1+, and a usable FUSE device. Read upstream source before recording.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = "https://github.com/mtoyoda/sl.git"
COMMIT = "923e7d7ebc5c1f009755bdeb789ac25658ccce03"
TREE = "56a5fd07310088f1e6372d81455c8489c66ea00b"


def output(*argv):
    return subprocess.check_output(argv, text=True).strip()


def record(binary, destination):
    if destination.exists() or destination.is_symlink():
        if not destination.is_dir() or any(destination.iterdir()):
            raise RuntimeError(f"Refusing to overwrite recording artifacts: use a new or empty directory: {destination}")
    for tool in ("vhs", "ttyd", "ffmpeg", "git", "make", "gcc", "fusermount3"):
        if shutil.which(tool) is None:
            raise RuntimeError(f"Missing recording dependency: {tool}")
    fd = os.open("/dev/fuse", os.O_RDWR)
    os.close(fd)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"Build git-on first: {binary}")
    destination.mkdir(parents=True, exist_ok=True)
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
    with tempfile.TemporaryDirectory(prefix="git-on-demo-") as temporary:
        private = Path(temporary)
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
        log = destination / "vhs.log"
        observed = set()
        generated = False
        with log.open("w") as stream:
            # All relative Output/Screenshot paths in the tape belong to the
            # requested recording directory, never the source checkout.
            process = subprocess.Popen(["vhs", str(ROOT / "assets/demo.tape")],
                                       cwd=destination, env=env, stdout=stream, stderr=subprocess.STDOUT)
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
                    raise RuntimeError(f"VHS failed with status {process.returncode}; inspect {log}")
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
    size = (destination / "demo.gif").stat().st_size
    info["gif_bytes"] = size
    info["gif_sha256"] = hashlib.sha256((destination / "demo.gif").read_bytes()).hexdigest()
    (destination / "demo.json").write_text(json.dumps(info, indent=2) + "\n")
    print(f"Recorded actual SL/FUSE execution on {info['host']}: {size:,} bytes")
    if size > 5 * 1024 * 1024:
        raise RuntimeError("GIF exceeds the 5 MiB repository-asset budget: do not commit it or open a PR")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/git-on")
    parser.add_argument("--output", type=Path, default=ROOT / "target/sl-recording",
                        help="New or empty recording directory (default: target/sl-recording)")
    args = parser.parse_args()
    record(args.binary.resolve(), args.output.resolve())
