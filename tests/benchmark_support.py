"""Native stat ABI support for byte-accounted benchmarks."""
from pathlib import Path
import subprocess


def source_identity(cwd=None):
    return dict(
        head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=cwd,text=True).strip(),
        working_tree_dirty=bool(subprocess.check_output(['git','diff','--name-only','HEAD'],cwd=cwd)))


def build_stat_observer(destination):
    source = Path(__file__).parent / 'support/stat_abi.c'
    subprocess.run(['cc', str(source), '-o', str(destination)],
                   check=True, capture_output=True)
    return destination


def stat_abi_bytes(observer):
    return int(subprocess.check_output([str(observer)]))
