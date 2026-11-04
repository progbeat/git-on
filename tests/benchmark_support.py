"""Native ABI and waited-process peak support for reproducible benchmarks."""
from pathlib import Path
import subprocess


def build_resource_observer(destination):
    source = Path(__file__).parent / 'support/resource_observer.c'
    subprocess.run(['cc', str(source), '-o', str(destination)],
                   check=True, capture_output=True)
    return destination


def stat_abi_bytes(observer):
    return int(subprocess.check_output([str(observer), '--stat-size']))
