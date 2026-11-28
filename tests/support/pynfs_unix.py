"""Run upstream pynfs unchanged with its NFS endpoint on an AF_UNIX socket."""
import os
from pathlib import Path
import runpy
import socket
import subprocess
import sys

source = Path(sys.argv[1]).resolve(strict=True)
point = os.fsencode(os.getcwd())
mounts = subprocess.check_output(["/sbin/mount"]).splitlines()
mount_target = b">:/ on " + point + b" (nfs"
for line in mounts:
    address, match, _ = line.partition(mount_target)
    if match and address.startswith(b"<"):
        mounted = address[1:]
        break
else:
    raise RuntimeError("could not find the Unix NFS socket for the test mount")
original_getaddrinfo = socket.getaddrinfo


def getaddrinfo(host, port, *args, **kwargs):
    if host == "git-on.test":
        return [(socket.AF_UNIX, socket.SOCK_STREAM, 0, "", os.fsdecode(mounted))]
    return original_getaddrinfo(host, port, *args, **kwargs)


socket.getaddrinfo = getaddrinfo
os.chdir(source)
sys.path.insert(0, str(source))
sys.argv = ["testserver.py", "git-on.test:/", "--minorversion", "1", *sys.argv[2:]]
runpy.run_path("testserver.py", run_name="__main__")
