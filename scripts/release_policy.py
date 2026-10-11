#!/usr/bin/env python3
"""Validate one release per master change; no network or publishing side effects."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tomllib


def version(text):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", text):
        raise ValueError("version must be a stable MAJOR.MINOR.PATCH (no prefix or leading zeros)")
    return tuple(map(int, text.split(".")))


def validate(current, previous, locked, notes):
    new = version(current)
    if locked != current:
        raise ValueError("Cargo.lock git-on version differs from Cargo.toml")
    lines = notes.splitlines()
    if len(lines) < 4 or lines[0] != f"# {current}" or not lines[1].startswith("Bump: "):
        raise ValueError("release notes must begin with '# VERSION' then 'Bump: KIND'")
    kind = lines[1][6:]
    if previous is None:
        if (current, kind) != ("0.1.0", "initial"):
            raise ValueError("first Cargo package must establish 0.1.0 / initial")
    else:
        major, minor, patch = version(previous)
        expected = {"patch": (major, minor, patch + 1), "minor": (major, minor + 1, 0), "major": (major + 1, 0, 0)}
        if kind not in expected or new != expected[kind]:
            raise ValueError(f"expected exactly one {kind!r} increment from {previous}")
    body = "\n".join(lines[2:]).strip()
    if len(body) < 30 or re.search(r"\b(TODO|TBD|PLACEHOLDER)\b", body, re.I):
        raise ValueError("provide concrete user-facing release notes, not a placeholder")
    return body


def check(base):
    package = tomllib.loads(Path("Cargo.toml").read_text())["package"]
    lock = tomllib.loads(Path("Cargo.lock").read_text())
    own = [p for p in lock["package"] if p["name"] == "git-on" and "source" not in p]
    if len(own) != 1:
        raise ValueError("expected exactly one local git-on package in Cargo.lock")
    # A missing file is the bootstrap case; an invalid ref must never become one.
    subprocess.run(["git", "rev-parse", "--verify", f"{base}^{{commit}}"], check=True, stdout=subprocess.DEVNULL)
    listing = subprocess.check_output(["git", "ls-tree", "--name-only", base, "--", "Cargo.toml"], text=True).strip()
    old = None
    if listing:
        old = tomllib.loads(subprocess.check_output(["git", "show", f"{base}:Cargo.toml"], text=True))["package"]["version"]
    body = validate(package["version"], old, own[0]["version"], Path(".github/release-notes.md").read_text())
    Path("release-body.md").write_text(body + "\n")
    print(f"Validated {old or '(initial)'} -> {package['version']}")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a") as stream:
            stream.write(f"version={package['version']}\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    args = parser.parse_args()
    check(args.base)
