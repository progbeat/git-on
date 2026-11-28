import os


def run_directories(root):
    """Find both current grouped runs and flat runs from the v1.5.2 oracle."""
    runs = list(root.glob("git-on-*"))
    for group in (root / "git-on", root / f".go{os.geteuid():x}"):
        runs.extend(path for path in group.glob("*") if path.is_dir())
    return runs
