#!/usr/bin/env python3
"""Require selected upstream pynfs conformance tests against the production Unix listener."""
import json
import os
from pathlib import Path
import re
import sys
import unittest

from integration import BINARY, OPTIONS, PARSER, UNITTEST_ARGS, RepositoryTest, git


class PynfsTests(RepositoryTest):
    def test_supported_nfs41_operations(self):
        source = Path(os.environ["PYNFS_SOURCE"]).resolve(strict=True) / "nfs4.1"
        adapter = Path(__file__).resolve().parent / "support/pynfs_unix.py"
        self.put("tree/file", "source bytes")
        self.put("tmp/.keep", "")
        git(self.repo, "add", "--force", "tree", "tmp")
        git(self.repo, "commit", "-qm", "upstream pynfs fixture")
        # Exercise the actual upstream tests and require a PASS for each code.
        # --noinit avoids the optional block/character-device fixtures.
        cases = ["COMP1", "COMP2", "COMP4a", "COMP4b", "COMP5", "SEQ1", "SEQ2", "SEQ4", "PUTFH1r", "PUTFH2", "OPEN1", "OPEN2", "CSID1", "CSID3", "CSID4", "LKPP1r", "RECC1", "RECC4", "RNM1r"]
        evidence = self.root / "pynfs.json"
        result, left = self.invoke_argv([
            "HEAD", "--backend=nfs", "--write-policy=all", "--", sys.executable, str(adapter), str(source),
            "--noinit", "--force", "--jsonout", str(evidence), *cases,
        ], timeout=90)
        self.assert_success(result)
        output = result.stdout.decode(errors="replace")
        self.assertEqual(json.loads(evidence.read_text())["failures"], 0, output)
        for case in cases:
            self.assertRegex(output, r"(?m)^" + re.escape(case) + r"\s+[^\n]+:\s*PASS\b", output)
        self.assertIsNone(left)
        self.assert_no_runtime_files()


if __name__ == "__main__":
    if not BINARY.is_file() or not OPTIONS.require_nfs or sys.platform != "darwin":
        PARSER.error("a runnable binary and --require-nfs on macOS are required")
    unittest.main(argv=[sys.argv[0], *UNITTEST_ARGS], verbosity=2)
