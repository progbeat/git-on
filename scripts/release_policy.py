#!/usr/bin/env python3
"""Require a release for binary changes; no network or publishing side effects."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib


def package_in_lock(lock):
    own = [p for p in lock["package"] if p["name"] == "git-on" and "source" not in p]
    if len(own) != 1:
        raise ValueError("expected exactly one local git-on package in Cargo.lock")
    return own[0]


def binary_manifest(manifest):
    # Keep unrecognized Cargo settings conservative. Only known descriptive and
    # test/example-only settings are removed; edition, features, profiles,
    # target dependencies, build scripts and patch overrides remain significant.
    result = {key: value for key, value in manifest.items()
              if key not in {"dev-dependencies", "example", "test", "bench"}}
    result["package"] = {key: value for key, value in manifest["package"].items()
                         if key not in {"version", "authors", "description", "documentation", "homepage",
                                        "repository", "readme", "keywords", "categories", "license",
                                        "license-file", "publish", "metadata", "exclude", "include",
                                        "autoexamples", "autotests", "autobenches"}}
    if "target" in result:
        result["target"] = {target: {key: value for key, value in settings.items()
                                     if key != "dev-dependencies"}
                            for target, settings in result["target"].items()}
        result["target"] = {target: settings for target, settings in result["target"].items() if settings}
        if not result["target"]:
            del result["target"]
    if "profile" in result:
        result["profile"] = {name: settings for name, settings in result["profile"].items()
                             if name not in {"dev", "test", "bench"}}
        if not result["profile"]:
            del result["profile"]
    return result


def runtime_dependencies(manifest):
    tables = [manifest, *manifest.get("target", {}).values()]
    return [(value.get("package", name) if isinstance(value, dict) else name, value)
            for table in tables for kind in ("dependencies", "build-dependencies")
            for name, value in table.get(kind, {}).items()]


def cargo_version_matches(requirement, locked):
    """Match Cargo's version comparators without resolving or downloading crates."""
    pattern = r"(\^|~|>=|<=|>|<|=)?\s*([0-9xX*]+(?:\.[0-9xX*]+){0,2})(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?"

    def parse(text):
        match = re.fullmatch(pattern, text.strip())
        if match is None:
            raise ValueError(f"cannot classify Cargo version requirement {text!r}")
        operator, core, pre, _ = match.groups()
        numbers = []
        wildcard = False
        for part in core.split("."):
            if part in ("*", "x", "X"):
                wildcard = True
            elif wildcard or (len(part) > 1 and part.startswith("0")):
                raise ValueError(f"cannot classify Cargo version requirement {text!r}")
            else:
                numbers.append(int(part))
        if pre and len(numbers) != 3:
            raise ValueError(f"cannot classify Cargo version requirement {text!r}")
        # Stable versions sort after every prerelease. Numeric identifiers sort
        # before textual identifiers, with numeric rather than lexical order.
        identifiers = []
        for part in pre.split(".") if pre else []:
            if not part or (part.isdigit() and len(part) > 1 and part.startswith("0")):
                raise ValueError(f"cannot classify Cargo version requirement {text!r}")
            identifiers.append((0, int(part)) if part.isdigit() else (1, part))
        prerelease = tuple(identifiers) if pre else ((2, ""),)
        return operator or ("=" if wildcard else "^"), tuple(numbers), prerelease, bool(pre)

    _, candidate, candidate_pre, has_pre = parse(locked)
    if len(candidate) != 3:
        raise ValueError(f"invalid locked Cargo version {locked!r}")
    if requirement.strip() in ("*", "x", "X"):
        return not has_pre
    comparators = [parse(part) for part in requirement.split(",")]
    if any(not numbers for _, numbers, _, _ in comparators):
        raise ValueError(f"cannot classify Cargo version requirement {requirement!r}")
    for operator, numbers, pre, _ in comparators:
        precision = len(numbers)
        prefix = candidate[:precision]
        exact = prefix == numbers and candidate_pre == pre
        greater = prefix > numbers or (precision == 3 and prefix == numbers and candidate_pre > pre)
        less = prefix < numbers or (precision == 3 and prefix == numbers and candidate_pre < pre)
        if operator in ("=", ">", ">=", "<", "<="):
            matches = {"=": exact, ">": greater, ">=": exact or greater,
                       "<": less, "<=": exact or less}[operator]
        elif operator == "~":
            matches = candidate[:min(precision, 2)] == numbers[:2] and (
                (precision == 3 and candidate[2] > numbers[2])
                or (prefix == numbers and candidate_pre >= pre))
        else:
            boundary = next((index + 1 for index, number in enumerate(numbers) if number), precision)
            matches = candidate[:boundary] == numbers[:boundary] and prefix >= numbers and (
                precision < 3 or prefix != numbers or candidate_pre >= pre)
        if not matches:
            return False
    return not has_pre or any(numbers == candidate and pre_is_requested
                              for _, numbers, _, pre_is_requested in comparators)


def binary_lock(manifest, lock):
    """Compare the runtime/build dependency closure, excluding test-only crates."""
    packages = lock["package"]
    own = package_in_lock(lock)

    def resolve(dependency):
        parts = dependency.split(maxsplit=2)
        candidates = [p for p in packages if p["name"] == parts[0]
                      and (len(parts) < 2 or p["version"] == parts[1])
                      and (len(parts) < 3 or p.get("source") == parts[2].strip("()"))]
        if len(candidates) != 1:
            raise ValueError(f"cannot resolve locked dependency {dependency!r}")
        return candidates[0]

    root_edges = [(item, resolve(item)) for item in own.get("dependencies", [])]
    pending = []
    for name, declaration in runtime_dependencies(manifest):
        candidates = [(item, package) for item, package in root_edges if package["name"] == name]
        if len(candidates) > 1:
            requirement = declaration if isinstance(declaration, str) else declaration.get("version")
            if requirement is not None:
                candidates = [(item, package) for item, package in candidates
                              if cargo_version_matches(requirement, package["version"])]
            if isinstance(declaration, dict) and "path" in declaration:
                candidates = [(item, package) for item, package in candidates if "source" not in package]
        if len(candidates) != 1:
            raise ValueError(f"cannot unambiguously classify runtime/build dependency {name!r}; "
                             "Cargo.lock does not identify its dependency kind")
        pending.extend(item for item, _ in candidates)
    found = {}
    while pending:
        package = resolve(pending.pop())
        identity = (package["name"], package["version"], package.get("source", ""))
        if identity not in found:
            found[identity] = {**package, "dependencies": sorted(package.get("dependencies", []))}
            pending.extend(package.get("dependencies", []))
    return found


def distribution_build(workflow):
    # The package job is the authoritative distribution build. Test-only jobs
    # and the publication job cannot change its executable. Inherited env and
    # run defaults also affect compilation (for example RUSTFLAGS or shell).
    # Comparing their complete YAML blocks is deliberately conservative and
    # requires no external YAML parser in the policy job.
    match = re.search(r"^  package:\s*\n(.*?)(?=^  [A-Za-z0-9_-]+:\s*\n|\Z)", workflow,
                      re.M | re.S)
    inherited = re.findall(r"^(?:env|defaults):[^\n]*(?:\n(?:[ \t].*|[ \t]*|\#.*))*", workflow, re.M)
    return (match.group(1) if match else "", tuple(inherited))


def release_reasons(base, current_manifest, current_lock):
    def old_text(name):
        listing = subprocess.check_output(["git", "ls-tree", "--name-only", base, "--", name], text=True).strip()
        return subprocess.check_output(["git", "show", f"{base}:{name}"], text=True) if listing else None

    old_manifest_text = old_text("Cargo.toml")
    if old_manifest_text is None:
        return None, ["initial Cargo package"]
    old_manifest = tomllib.loads(old_manifest_text)
    old_lock_text = old_text("Cargo.lock")
    # Both sides of a rename matter: moving src/ into examples/ still removes
    # released source. Include untracked files so the local pre-commit check
    # agrees with CI after those files are added.
    changed = subprocess.check_output(["git", "diff", "--no-renames", "--name-only", "-z", base, "--"], text=True).split("\0")
    changed += subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard", "-z"], text=True).split("\0")
    build_paths = {"build.rs", "rust-toolchain", "rust-toolchain.toml"}
    for manifest in (old_manifest, current_manifest):
        if isinstance(manifest["package"].get("build"), str):
            build_paths.add(manifest["package"]["build"])
        for kind in ("bin", "lib"):
            targets = manifest.get(kind, []) if kind == "bin" else [manifest.get(kind, {})]
            build_paths.update(target["path"] for target in targets if "path" in target)
    reasons = [path for path in changed if path.startswith(("src/", ".cargo/")) or path in build_paths]
    if binary_manifest(current_manifest) != binary_manifest(old_manifest):
        reasons.append("Cargo.toml binary settings")
    if old_lock_text is None or binary_lock(current_manifest, current_lock) != binary_lock(old_manifest, tomllib.loads(old_lock_text)):
        reasons.append("Cargo.lock runtime/build dependencies")
    workflow_name = ".github/workflows/ci.yml"
    if workflow_name in changed:
        current_workflow = Path(workflow_name).read_text() if Path(workflow_name).exists() else ""
        if distribution_build(current_workflow) != distribution_build(old_text(workflow_name) or ""):
            reasons.append("distribution build job")
    return old_manifest["package"]["version"], reasons


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


def push_base(event_path):
    event = json.loads(Path(event_path).read_text())
    before, after = event.get("before", ""), event.get("after", "")
    for value in (before, after):
        if not re.fullmatch(r"[0-9a-f]{40}", value) or value == "0" * 40:
            raise ValueError("push must have nonzero before/after commit IDs")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if after != head:
        raise ValueError("push after SHA differs from checked-out commit")
    subprocess.run(["git", "merge-base", "--is-ancestor", before, after], check=True)
    return before


def check(base):
    manifest = tomllib.loads(Path("Cargo.toml").read_text())
    package = manifest["package"]
    lock = tomllib.loads(Path("Cargo.lock").read_text())
    own = package_in_lock(lock)
    version(package["version"])
    if own["version"] != package["version"]:
        raise ValueError("Cargo.lock git-on version differs from Cargo.toml")
    # A missing file is the bootstrap case; an invalid ref must never become one.
    subprocess.run(["git", "rev-parse", "--verify", f"{base}^{{commit}}"], check=True, stdout=subprocess.DEVNULL)
    old, reasons = release_reasons(base, manifest, lock)
    required = bool(reasons)
    body_path = Path("release-body.md")
    body_path.unlink(missing_ok=True)
    if required:
        body = validate(package["version"], old, own["version"], Path(".github/release-notes.md").read_text())
        body_path.write_text(body + "\n")
        print(f"Validated {old or '(initial)'} -> {package['version']}: {', '.join(reasons)}")
    else:
        if package["version"] != old:
            raise ValueError("no binary changes: keep the current master version and skip publication")
        notes_name = ".github/release-notes.md"
        base_notes_path = subprocess.check_output(["git", "ls-tree", "--name-only", base, "--", notes_name])
        base_notes = subprocess.check_output(["git", "show", f"{base}:{notes_name}"]) if base_notes_path else None
        notes_path = Path(notes_name)
        current_notes = notes_path.read_bytes() if notes_path.exists() else None
        if current_notes != base_notes:
            raise ValueError("no binary changes: keep the current master release notes and skip publication")
        print(f"No binary changes; retain {package['version']} and skip publication")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a") as stream:
            stream.write(f"version={package['version']}\nrelease_required={str(required).lower()}\n")
    return required


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--base")
    mode.add_argument("--push-event", type=Path)
    args = parser.parse_args()
    check(push_base(args.push_event) if args.push_event else args.base)
