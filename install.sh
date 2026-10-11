#!/bin/sh
# Install or update git-on. The entire payload is parsed before main() can run.
git_on_install() {
set -eu
if ! command -v python3 >/dev/null 2>&1; then
    printf '%s\n' 'git-on installer: Python 3.8+ is required; install it with your distribution tools.' >&2
    exit 1
fi
exec python3 - "$@" <<'GIT_ON_INSTALLER_PYTHON'
import argparse
import hashlib
import gzip
import json
import os
from pathlib import Path
import platform
import re
import shutil
import secrets
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request

REPO = "progbeat/git-on"
TARGET = "x86_64-unknown-linux-gnu"
VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
MAX_ARCHIVE = 128 * 1024 * 1024
MAX_METADATA = 1024 * 1024


def fail(message):
    raise RuntimeError(message)


def supported_host():
    if sys.version_info < (3, 8):
        fail("Python 3.8 or newer is required.")
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "amd64"):
        fail("Prebuilt releases support Linux x86_64 only; build from source on other Linux architectures.")
    try:
        libc = os.confstr("CS_GNU_LIBC_VERSION") or ""
    except (ValueError, OSError):
        libc = ""
    match = re.fullmatch(r"glibc ([0-9]+)\.([0-9]+)", libc)
    if not match or tuple(map(int, match.groups())) < (2, 35):
        fail("This binary needs glibc 2.35 or newer (Ubuntu 22.04 baseline); musl/Alpine is not supported.")
    if os.geteuid() == 0:
        fail("Run this installer as your normal user, without sudo.")


class HTTPSOnly(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, url):
        if not url.startswith("https://"):
            fail("Refusing a non-HTTPS download redirect.")
        return super().redirect_request(request, fp, code, message, headers, url)


def download(url, destination, limit):
    # No tokens are read, passed in URLs, or forwarded to redirected asset hosts.
    request = urllib.request.Request(url, headers={"User-Agent": "git-on-installer", "Accept": "application/vnd.github+json"})
    opener = urllib.request.build_opener(HTTPSOnly())
    with opener.open(request, timeout=30) as response, destination.open("wb") as output:
        length = response.headers.get("Content-Length")
        if length is not None and (not length.isdigit() or int(length) > limit):
            fail("Download has an invalid or oversized Content-Length.")
        total = 0
        while True:
            chunk = response.read(64 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > limit:
                fail("Download exceeds the size limit.")
            output.write(chunk)
        if length is not None and total != int(length):
            fail("Download was truncated.")


def gh(*args):
    # Use only the caller's existing GitHub CLI authentication. Never print its output on error.
    result = subprocess.run(["gh", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180)
    if result.returncode:
        fail("GitHub CLI request failed. Check gh auth status and repository/release access.")
    return result.stdout


def get_release(version, use_gh, directory):
    endpoint = "latest" if version is None else "tags/v" + version
    path = "repos/" + REPO + "/releases/" + endpoint
    if use_gh:
        if not shutil.which("gh"):
            fail("--github-cli requires an already authenticated GitHub CLI (gh).")
        data = gh("api", "--hostname", "github.com", path)
        if len(data) > MAX_METADATA:
            fail("Release metadata exceeds the size limit.")
    else:
        metadata = directory / "github-release.json"
        download("https://api.github.com/" + path, metadata, MAX_METADATA)
        data = metadata.read_bytes()
    release = json.loads(data)
    if not isinstance(release, dict):
        fail("Release metadata must be an object.")
    tag = release.get("tag_name", "")
    if not isinstance(tag, str) or not re.fullmatch("v" + VERSION, tag):
        fail("Release has an invalid stable version tag.")
    if release.get("draft") is not False or release.get("prerelease") is not False:
        fail("Refusing a draft or prerelease.")
    if version is not None and tag != "v" + version:
        fail("Release does not match the requested version.")
    archive = "git-on-" + tag + "-" + TARGET + ".tar.gz"
    expected = {archive, "SHA256SUMS", "release.json"}
    assets = release.get("assets", [])
    if not isinstance(assets, list) or len(assets) != len(expected):
        fail("Release has missing, duplicate, or unexpected assets.")
    names = set()
    for asset in assets:
        if not isinstance(asset, dict):
            fail("Release asset metadata must be an object.")
        name = asset.get("name")
        if not isinstance(name, str) or name not in expected or name in names or asset.get("state") != "uploaded":
            fail("Release has missing, duplicate, or unexpected assets.")
        names.add(name)
        url = "https://github.com/" + REPO + "/releases/download/" + tag + "/" + name
        if asset.get("browser_download_url") != url:
            fail("Release asset URL does not match its expected repository and version.")
        size = asset.get("size")
        limit = MAX_ARCHIVE if name == archive else MAX_METADATA
        if type(size) is not int or size <= 0 or size > limit:
            fail("Release asset has an invalid size.")
    return tag, archive, assets


def get_assets(tag, archive, assets, use_gh, directory):
    for asset in assets:
        name = asset["name"]
        if use_gh:
            gh("release", "download", tag, "--repo", "https://github.com/" + REPO,
               "--pattern", name, "--dir", str(directory))
        else:
            download(asset["browser_download_url"], directory / name,
                     MAX_ARCHIVE if name == archive else MAX_METADATA)
        path = directory / name
        if not path.is_file() or path.is_symlink() or path.stat().st_size != asset["size"]:
            fail("Downloaded asset has an unexpected size or type: " + name)


def verify(directory, tag, archive):
    expected = {archive, "release.json"}
    sums = {}
    for line in (directory / "SHA256SUMS").read_text(encoding="ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64}) [ *]([^/\\]+)", line)
        if not match or match[2] not in expected or match[2] in sums:
            fail("SHA256SUMS has missing, duplicate, or unexpected entries.")
        sums[match[2]] = match[1]
    if set(sums) != expected:
        fail("SHA256SUMS is incomplete.")
    for name, expected_hash in sums.items():
        digest = hashlib.sha256()
        with (directory / name).open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
        if digest.hexdigest() != expected_hash:
            fail("Checksum verification failed for " + name + "; existing installation is unchanged.")
    manifest = json.loads((directory / "release.json").read_bytes())
    if not isinstance(manifest, dict):
        fail("Verified release metadata must be an object.")
    if (manifest.get("version") != tag[1:] or manifest.get("target") != TARGET
            or manifest.get("baseline") != "Ubuntu 22.04 / glibc 2.35"
            or not isinstance(manifest.get("commit"), str)
            or not re.fullmatch(r"[0-9a-f]{40}", manifest.get("commit", ""))):
        fail("Verified release metadata does not match this version, target, or supported baseline.")


def safe_destination(directory):
    if not directory.is_absolute() or ".." in directory.parts:
        fail("--bin-dir must be an absolute path without .. components.")
    # Reject symlink components rather than silently installing somewhere else.
    for component in reversed((directory, *directory.parents)):
        if component.is_symlink():
            fail("Refusing a symlink in the installation directory: " + str(component))
        if component.exists() and not component.is_dir():
            fail("Installation directory contains a non-directory: " + str(component))
    destination = directory / "git-on"
    if destination.is_symlink() or (destination.exists() and not destination.is_file()):
        fail("Refusing to replace a symlink or non-regular git-on destination.")
    return destination


def unpack_binary(directory, archive):
    root = archive[:-7]
    expanded = directory / "package.tar"
    # Bound expansion before parsing headers: tarfile otherwise consumes hidden
    # PAX/GNU extension payloads before exposing members for validation.
    with gzip.open(directory / archive, "rb") as source, expanded.open("wb") as output:
        total = 0
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            total += len(chunk)
            if total > MAX_ARCHIVE + MAX_METADATA + 1024 * 1024:
                fail("Expanded archive exceeds the size limit.")
            output.write(chunk)
    expected = {root, root + "/git-on", root + "/README.md"}
    # v0.1.0 predates LICENSE in archives; retain pinned/repeat installation support.
    allowed = expected | {root + "/LICENSE"}
    members = set()
    binary_range = None
    with expanded.open("rb") as source:
        while True:
            header = source.read(512)
            if header == bytes(512):
                # A valid tar ends with at least two zero blocks, then padding.
                trailer = source.read()
                if len(trailer) < 512 or any(trailer):
                    fail("Archive has an invalid trailer.")
                break
            if len(header) != 512:
                fail("Archive header was truncated.")
            member = tarfile.TarInfo.frombuf(header, "utf-8", "strict")
            if member.name not in allowed or member.name in members:
                fail("Archive has duplicate, missing, or unsafe paths.")
            members.add(member.name)
            if member.name == root:
                if not member.isdir() or member.size != 0:
                    fail("Archive root is not an ordinary directory.")
            elif member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE):
                fail("Archive contains a symlink, hard link, or unexpected member type.")
            limit = MAX_ARCHIVE if member.name == root + "/git-on" else MAX_METADATA
            if member.size < 0 or member.size > limit:
                fail("Archive member exceeds the size limit.")
            if member.name == root + "/git-on":
                if member.size == 0:
                    fail("Archive binary is empty.")
                binary_range = (source.tell(), member.size)
            # No extension records, extraction paths, or sparse files are trusted.
            source.seek((member.size + 511) // 512 * 512, os.SEEK_CUR)
        if not expected <= members:
            fail("Archive has missing members.")
    return expanded, binary_range


def open_bin_dir(directory):
    # Pin every path component with O_NOFOLLOW. A renamed/replaced ancestor
    # cannot redirect the final write between a path check and os.replace.
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in directory.parts[1:]:
            try:
                os.mkdir(part, 0o755, dir_fd=fd)
            except FileExistsError:
                pass
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        info = os.fstat(fd)
        if info.st_uid != os.geteuid() or info.st_mode & 0o022:
            fail("Installation directory must be owned by you and not writable by other users.")
        result, fd = fd, None
        return result
    finally:
        if fd is not None:
            os.close(fd)


def check_target(fd):
    try:
        info = os.stat("git-on", dir_fd=fd, follow_symlinks=False)
    except FileNotFoundError:
        return
    if not stat.S_ISREG(info.st_mode):
        fail("Refusing to replace a symlink or non-regular git-on destination.")


def install(directory, archive, tag, bin_dir):
    destination = safe_destination(bin_dir)
    expanded, (offset, size) = unpack_binary(directory, archive)
    dir_fd = open_bin_dir(bin_dir)
    staged = ".git-on-install-" + secrets.token_hex(16)
    try:
        check_target(dir_fd)
        fd = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=dir_fd)
        with os.fdopen(fd, "wb") as output, expanded.open("rb") as source:
            source.seek(offset)
            remaining = size
            while remaining:
                chunk = source.read(min(remaining, 1024 * 1024))
                if not chunk:
                    fail("Archive binary was truncated.")
                output.write(chunk)
                remaining -= len(chunk)
            output.flush()
            os.fchmod(output.fileno(), 0o755)
            os.fsync(output.fileno())
        # /proc/self/fd pins the same verified directory for the version probe.
        result = subprocess.run(["/proc/self/fd/" + str(dir_fd) + "/" + staged, "--version"],
                                pass_fds=(dir_fd,), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        if result.returncode or result.stdout.decode("utf-8").strip() != "git-on " + tag[1:]:
            fail("Downloaded executable failed its version check. It needs glibc 2.35+, libz.so.1 and libgcc_s.so.1; check your distribution runtime libraries. Existing installation is unchanged.")
        check_target(dir_fd)
        os.replace(staged, "git-on", src_dir_fd=dir_fd, dst_dir_fd=dir_fd)
    finally:
        try:
            os.unlink(staged, dir_fd=dir_fd)
        except FileNotFoundError:
            pass
        os.close(dir_fd)
    return destination


def main(argv=None):
    parser = argparse.ArgumentParser(prog="install.sh", description="Install or update git-on to the latest stable GitHub Release, without sudo.")
    parser.add_argument("--version", help="install a specific stable version, e.g. 0.1.0 or v0.1.0")
    parser.add_argument("--bin-dir", default=str(Path.home() / ".local/bin"), help="absolute installation directory (default: ~/.local/bin)")
    parser.add_argument("--github-cli", action="store_true", help="use existing gh authentication for private releases or API rate limits")
    args = parser.parse_args(argv)
    version = args.version
    if version is not None:
        version = version[1:] if version.startswith("v") else version
        if not re.fullmatch(VERSION, version):
            fail("--version must be a stable version such as 0.1.0 or v0.1.0.")
    supported_host()
    bin_dir = Path(args.bin_dir)
    safe_destination(bin_dir)
    with tempfile.TemporaryDirectory(prefix="git-on-download-") as temporary:
        directory = Path(temporary)
        tag, archive, assets = get_release(version, args.github_cli, directory)
        print("Installing git-on " + tag + " ...", flush=True)
        get_assets(tag, archive, assets, args.github_cli, directory)
        verify(directory, tag, archive)
        destination = install(directory, archive, tag, bin_dir)
    print("Installed git-on " + tag + " to " + str(destination))
    print("Run this same installer again to update to the latest stable release.")
    if str(bin_dir) not in os.environ.get("PATH", "").split(os.pathsep):
        print("Add " + str(bin_dir) + " to your PATH, then run: git on --version")
    elif shutil.which("git-on") != str(destination):
        print("Another git-on comes first on PATH; move " + str(bin_dir) + " ahead of it.")
    if not shutil.which("fusermount3"):
        print("Before running git on: install your distribution's fuse3 package (fusermount3 is missing).")
    try:
        usable_fuse = stat.S_ISCHR(os.stat("/dev/fuse").st_mode) and os.access("/dev/fuse", os.R_OK | os.W_OK)
    except OSError:
        usable_fuse = False
    if not usable_fuse:
        print("Before running git on: provide a usable /dev/fuse; containers also need permission to mount FUSE.")
    print("No system packages, shell profiles, or FUSE permissions were changed.")


def interrupted(signum, frame):
    fail("Installation interrupted; temporary files are being removed.")


if __name__ == "__main__":
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    try:
        main()
    except urllib.error.HTTPError as error:
        print("git-on installer: GitHub download failed (HTTP " + str(error.code) + "). Check that a release exists and is public; use --github-cli for private access or rate limits.", file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, RuntimeError, tarfile.TarError, subprocess.SubprocessError) as error:
        print("git-on installer: " + str(error), file=sys.stderr)
        sys.exit(1)
GIT_ON_INSTALLER_PYTHON
}
{ git_on_install "$@"; }
