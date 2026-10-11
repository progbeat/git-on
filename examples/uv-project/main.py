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
