import os


def run_directories(root):
    """Find both current grouped runs and flat runs from the v1.5.2 oracle."""
    runs = list(root.glob("git-on-*"))
    for group in (root / "git-on", root / f".go{os.geteuid():x}"):
        runs.extend(path for path in group.glob("*") if path.is_dir())
    return runs


def run_lock(run):
    """Find the current registration or a lock from an earlier run layout."""
    try:
        identity = run.stat()
        registrations = sorted(run.parent.glob(
            f".run-{run.name}-*.{identity.st_dev:x}.{identity.st_ino:x}"
        ))
        assert len(registrations) <= 1, registrations
        if registrations:
            return registrations[0]
    except FileNotFoundError:
        pass
    locks = sorted(run.glob(f".lock-{run.name}-*"))
    assert len(locks) <= 1, locks
    if locks:
        assert locks[0].is_file() and locks[0].stat().st_size == 0, locks[0]
        return locks[0]


def run_id(run):
    lock = run_lock(run)
    if lock.name.startswith(".run-"):
        return lock.name.removeprefix(".run-").split(".", 1)[0]
    return lock.name.removeprefix(".lock-")


def ssh_owner_marker(run):
    markers = sorted(run.glob("ssh-control-owner-*"))
    assert len(markers) <= 1, markers
    if markers:
        assert markers[0].is_file() and markers[0].stat().st_size == 0, markers[0]
        return markers[0]
