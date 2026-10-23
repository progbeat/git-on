#!/usr/bin/env python3
"""Check the reviewed FUSE callback inventory, not runtime/branch coverage.

Run after cargo has fetched the locked dependencies. The dependency source is
resolved through cargo metadata, never through a guessed Cargo registry path.
"""
import argparse
import ast
import hashlib
import json
import re
import subprocess
import sys
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
FUSER_VERSION = "0.16.0"
VENDORED_SHA256 = "fa632e1a6272c0fd100f2950cbd6bdab7fcca188f50aac00fa8a5a7c9f4b3f19"
FUSER_CHECKSUM = "0bb29a3ae32279fe3e79a958fe01899f5fb23eadccee919cf88e145b54ed9367"
TRAIT_SHA256 = "6d47b0047ba5917f4534ab34186152e1ef909ab04323bdc8d4bc7e9a87479fdf"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def rust_mask(source):
    # Preserve positions while hiding comments and ordinary Rust strings. This
    # deliberately small parser is checked against a pinned trait fingerprint;
    # an upstream syntax/source change requires review before accepting it.
    return re.sub(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"',
                  lambda m: "".join("\n" if c == "\n" else " " for c in m[0]),
                  source, flags=re.S)


def block(source, declaration):
    masked = rust_mask(source)
    start = masked.index(declaration)
    opening = masked.index("{", start)
    depth = 1
    for end in range(opening + 1, len(masked)):
        depth += (masked[end] == "{") - (masked[end] == "}")
        if depth == 0:
            return source[start:end + 1]
    raise ValueError("unterminated Rust declaration: " + declaration)


def methods(source):
    masked = rust_mask(source)
    found = list(re.finditer(r"^    fn ([a-z_][a-z_0-9]*)\s*\(", masked, re.M))
    result = {}
    previous_end = 0
    for match in found:
        opening = masked.index("{", match.end())
        depth = 1
        end = opening + 1
        while depth:
            require(end < len(masked), "unterminated method " + match[1])
            depth += (masked[end] == "{") - (masked[end] == "}")
            end += 1
        prefix = source[previous_end:match.start()]
        result[match[1]] = {
            "macos": bool(re.search(r'#\[cfg\(target_os\s*=\s*"macos"\)\]', prefix)),
            "body": masked[opening:end],
        }
        previous_end = end
    return result


def table(document, marker, columns):
    start = "<!-- " + marker + " START -->"
    end = "<!-- " + marker + " END -->"
    require(document.count(start) == document.count(end) == 1,
            "missing/duplicated table markers: " + marker)
    region = document.split(start)[1].split(end)[0]
    rows = []
    for line in region.splitlines():
        if not line.startswith("|"):
            continue
        cells = [cell.strip().strip("`") for cell in line.strip().strip("|").split("|")]
        require(len(cells) == columns, "wrong number of table columns: " + line)
        if set("".join(cells)) <= set("-: "):
            continue
        rows.append(cells)
    require(len(rows) >= 2, "empty table: " + marker)
    return rows[1:]


def classifications(trait, overrides):
    require(set(overrides) <= set(trait), "Overlay method missing from pinned Filesystem trait")
    return {name: ("macos-only" if item["macos"] else
                   "implemented" if name in overrides else
                   "unsupported" if "reply.error(ENOSYS)" in item["body"] else
                   "inherited") for name, item in trait.items()}


def validate_rows(rows, expected, aliases):
    names = [row[0] for row in rows]
    require(len(names) == len(set(names)), "duplicate callback row")
    require(set(names) == set(expected),
            "callback inventory differs: missing=" + str(sorted(set(expected) - set(names))) +
            "; extra=" + str(sorted(set(names) - set(expected))))
    for name, classification, protocol, mounted, notes in rows:
        require(classification == expected[name], "wrong classification for " + name)
        require(bool(notes), "missing assertions/limitations for " + name)
        for ids, kind in [(protocol, "protocol"), (mounted, "mounted")]:
            if ids == "-":
                continue
            for identifier in ids.split(","):
                identifier = identifier.strip()
                require(identifier in aliases, "unknown test ID: " + identifier)
                require(aliases[identifier][0] == ("tests/fuse_protocol.rs" if kind == "protocol"
                                                   else "tests/integration.py"),
                        "wrong evidence layer for " + name + ": " + identifier)
        if classification == "macos-only":
            require(protocol == mounted == "-", "macOS callback cannot claim Linux evidence")
        else:
            require(protocol != "-", "missing protocol test for " + name)


def rust_test_names(source):
    return set(re.findall(r"#\[test\]\s*fn\s+([a-z_][a-z_0-9]*)\s*\(", rust_mask(source)))


def test_names(path):
    source = path.read_text()
    if path.suffix == ".py":
        tree = ast.parse(source)
        return {node.name + "." + child.name
                for node in tree.body if isinstance(node, ast.ClassDef)
                for child in node.body if isinstance(child, ast.FunctionDef)
                and child.name.startswith("test_")}
    return rust_test_names(source)


def host_metadata(root):
    # cargo test/build fetch host dependencies only. Unfiltered metadata also
    # requests unused wasm/Windows packages, making an offline guard fail on a
    # perfectly valid Linux build. Match the active compiler's host explicitly.
    compiler = subprocess.check_output(["rustc", "-vV"], cwd=root, text=True)
    hosts = re.findall(r"^host: (\S+)$", compiler, re.M)
    require(len(hosts) == 1, "could not determine rustc host target")
    return json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
         "--filter-platform", hosts[0]], cwd=root))


def check(root=ROOT):
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    packages = [p for p in lock["package"] if p["name"] == "fuser"]
    require(len(packages) == 1, "expected exactly one locked fuser dependency")
    locked = packages[0]
    require(locked["version"] == FUSER_VERSION and "source" not in locked,
            "fuser dependency changed; review callback contract, matrix and fingerprint")
    vendored = root / "vendor/fuser"
    digest = hashlib.sha256()
    for path in sorted([vendored / "Cargo.toml", vendored / "build.rs", *vendored.glob("src/**/*.rs")]):
        digest.update(str(path.relative_to(vendored)).encode() + b"\0" + path.read_bytes())
    require(digest.hexdigest() == VENDORED_SHA256, "vendored fuser changed; review the compatibility patch and inventory")
    metadata = host_metadata(root)
    packages = [p for p in metadata["packages"] if p["name"] == "fuser"]
    require(len(packages) == 1 and packages[0]["version"] == FUSER_VERSION,
            "cargo metadata differs from reviewed fuser version")
    package = packages[0]
    resolved = next(n for n in metadata["resolve"]["nodes"] if n["id"] == package["id"])
    require("abi-7-31" in resolved["features"], "reviewed FUSE ABI 7.31 is not enabled")
    library = next(t for t in package["targets"] if "lib" in t["kind"])
    source = Path(library["src_path"]).read_text()
    trait_source = block(source, "pub trait Filesystem")
    require(hashlib.sha256(trait_source.encode()).hexdigest() == TRAIT_SHA256,
            "fuser Filesystem trait changed; review all callback/default semantics")
    trait = methods(trait_source)
    overlay = methods(block((root / "src/filesystem.rs").read_text(), "impl Filesystem for Overlay"))
    expected = classifications(trait, overlay)
    document = (root / "docs/fuse-coverage.md").read_text()
    aliases = {}
    cached_names = {}
    for identifier, filename, function in table(document, "TEST IDS", 3):
        require(identifier not in aliases, "duplicate test ID: " + identifier)
        require(filename in ("tests/fuse_protocol.rs", "tests/integration.py"),
                "unexpected evidence source: " + filename)
        if filename not in cached_names:
            cached_names[filename] = test_names(root / filename)
        require(function in cached_names[filename], "missing named test: " + filename + "::" + function)
        aliases[identifier] = (filename, function)
    rows = table(document, "CALLBACK MATRIX", 5)
    validate_rows(rows, expected, aliases)
    counts = {kind: sum(value == kind for value in expected.values())
              for kind in sorted(set(expected.values()))}
    print("FUSE inventory verified: " + ", ".join(f"{n} {kind}" for kind, n in counts.items()))
    print("Named tests exist; runtime success and branch coverage require separate test evidence.")


class GuardTests(unittest.TestCase):
    def setUp(self):
        self.expected = {"lookup": "implemented", "destroy": "inherited", "setvolname": "macos-only"}
        self.aliases = {"P": ("tests/fuse_protocol.rs", "test_protocol")}
        self.rows = [["lookup", "implemented", "P", "-", "positive and negative"],
                     ["destroy", "inherited", "P", "-", "lifecycle"],
                     ["setvolname", "macos-only", "-", "-", "Linux exclusion"]]

    def test_complete_matrix_is_accepted(self):
        validate_rows(self.rows, self.expected, self.aliases)

    def test_missing_duplicate_extra_and_misclassified_rows_fail(self):
        variants = [self.rows[:-1], self.rows + [self.rows[0]],
                    self.rows + [["new_callback", "inherited", "P", "-", "new"]]]
        wrong = [row.copy() for row in self.rows]
        wrong[0][1] = "unsupported"
        variants.append(wrong)
        for rows in variants:
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                validate_rows(rows, self.expected, self.aliases)

    def test_missing_test_and_wrong_evidence_layer_fail(self):
        for identifier in ["missing", "-"]:
            rows = [row.copy() for row in self.rows]
            rows[0][2] = identifier
            with self.subTest(identifier=identifier), self.assertRaises(ValueError):
                validate_rows(rows, self.expected, self.aliases)
        with self.assertRaises(ValueError):
            validate_rows(self.rows, self.expected, {"P": ("tests/integration.py", "test_mounted")})

    def test_parser_preserves_defaults_and_platform_attributes(self):
        source = '''pub trait Filesystem {
    fn init(&mut self) { let _text = "}"; }
    fn getxattr(&mut self) { reply.error(ENOSYS); }
    // A comment with a closing brace: }
    #[cfg(target_os = "macos")]
    fn setvolname(&mut self) { reply.error(ENOSYS); }
}'''
        parsed = methods(block(source, "pub trait Filesystem"))
        self.assertEqual(classifications(parsed, {}), {
            "init": "inherited", "getxattr": "unsupported", "setvolname": "macos-only"})
        self.assertEqual(classifications(parsed, {"getxattr": {}})["getxattr"], "implemented")

    def test_commented_or_quoted_rust_tests_do_not_count(self):
        source = '''/* #[test]
fn block_comment() {} */
// #[test]
// fn line_comment() {}
const TEXT: &str = "#[test] fn quoted() {}";
#[test]
fn actual_test() {}'''
        self.assertEqual(rust_test_names(source), {"actual_test"})

    def test_metadata_stays_offline_and_filters_compiler_host(self):
        with patch.object(subprocess, "check_output", side_effect=[
                "rustc 1.89.0\nhost: x86_64-unknown-linux-gnu\n", b'{"packages": []}']) as output:
            self.assertEqual(host_metadata(ROOT), {"packages": []})
            self.assertEqual(output.call_args_list[0].args[0], ["rustc", "-vV"])
            self.assertEqual(output.call_args_list[1].args[0], [
                "cargo", "metadata", "--locked", "--offline", "--format-version", "1",
                "--filter-platform", "x86_64-unknown-linux-gnu"])

    def test_missing_compiler_host_does_not_run_unfiltered_metadata(self):
        with patch.object(subprocess, "check_output", return_value="rustc unknown\n") as output:
            with self.assertRaises(ValueError):
                host_metadata(ROOT)
            self.assertEqual(output.call_count, 1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]], verbosity=2)
    else:
        try:
            check()
        except (ValueError, OSError, subprocess.CalledProcessError) as error:
            raise SystemExit("FUSE coverage inventory failed: " + str(error)) from error
