# /// script
# requires-python = ">=3.11,<3.14"
# dependencies = ["numpy==2.2.6", "numba==0.61.2"]
# ///
"""Run twice in one snapshot to prove local imports and Numba cache reuse."""
from pathlib import Path
import subprocess
import sys

for index in range(2):
    result = subprocess.run([sys.executable, "worker.py"], check=True, capture_output=True, text=True)
    print(result.stdout, end="")
    cache = {str(path): path.stat().st_mtime_ns for path in Path("__pycache__").glob("*.nb*")}
    assert any(name.endswith(".nbc") for name in cache), "Numba did not write cache=True artifacts"
    if index == 0:
        first = cache
    else:
        assert cache == first, "Second process rewrote rather than reused the Numba cache"
print("cache-reused")

# Select artifacts to retain after git-on discards the writable snapshot layer.
import os
import shutil
if output := os.environ.get("GIT_ON_OUTPUT_DIR"):
    shutil.copytree("__pycache__", Path(output) / "cache-evidence")
