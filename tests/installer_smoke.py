#!/usr/bin/env python3
"""Run dispatcher/hash regressions; packaged installation uses dist_installer.py."""
import subprocess
import sys
from pathlib import Path
subprocess.run([sys.executable,str(Path(__file__).with_name('test_installer.py'))],check=True)
