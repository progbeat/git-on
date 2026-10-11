#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("release_policy", Path(__file__).resolve().parents[1] / "scripts/release_policy.py")
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)


def notes(v, kind):
    return f"# {v}\nBump: {kind}\n\nFix immutable file protection during concurrent writes.\n"


class ReleasePolicyTests(unittest.TestCase):
    def test_bootstrap(self):
        MOD.validate("0.1.0", None, "0.1.0", notes("0.1.0", "initial"))

    def test_all_increment_types(self):
        for new, kind in [("1.2.4", "patch"), ("1.3.0", "minor"), ("2.0.0", "major")]:
            MOD.validate(new, "1.2.3", new, notes(new, kind))

    def test_reject_unchanged_lower_skipped_and_stale_versions(self):
        for old, new, kind in [("1.2.3", "1.2.3", "patch"), ("1.2.3", "1.2.2", "patch"), ("1.2.3", "1.2.5", "patch"), ("1.2.4", "1.2.4", "patch"), ("1.2.3", "2.0.0", "minor")]:
            with self.subTest(old=old, new=new, kind=kind), self.assertRaises(ValueError):
                MOD.validate(new, old, new, notes(new, kind))

    def test_reject_invalid_semver(self):
        for text in ["v1.2.3", "01.2.3", "1.2", "1.2.3-rc1", "1.2.3+build", "-1.2.3"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                MOD.version(text)

    def test_reject_lock_and_notes_mismatch(self):
        for lock, note in [("0.1.0", notes("0.1.1", "patch")), ("0.1.1", notes("0.2.0", "minor")), ("0.1.1", "# 0.1.1\nBump: patch\n\nTODO write the release notes here.")]:
            with self.subTest(lock=lock, note=note), self.assertRaises(ValueError):
                MOD.validate("0.1.1", "0.1.0", lock, note)

    def test_initial_is_not_reusable(self):
        with self.assertRaises(ValueError):
            MOD.validate("0.1.0", "0.1.0", "0.1.0", notes("0.1.0", "initial"))


if __name__ == "__main__":
    unittest.main()
