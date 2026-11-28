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
mounted = next(line.split(b">:/", 1)[0][1:] for line in mounts
               if line.startswith(b"</private/tmp/git-on-nfs-") and b" on " + point + b" (nfs" in line)
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
