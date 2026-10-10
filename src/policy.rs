//! Creation rules compiled exclusively from snapshot .gitignore blobs.
use crate::snapshot::{EntryKind, Snapshot};
use anyhow::{bail, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

#[derive(Debug)]
struct Rule {
    pattern: Vec<u8>,
    negative: bool,
    directory_only: bool,
    basename: bool,
}
impl Rule {
    fn parse(mut line: &[u8]) -> Option<Self> {
        // Git's patterns are byte strings. Only ASCII SPACE is trimmed, and only
        // when unescaped. Tabs, Unicode whitespace, braces and malformed bracket
        // expressions must never be normalized by a generic glob parser.
        if let Some(nul) = line.iter().position(|&b| b == 0) {
            line = &line[..nul];
        }
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.first() == Some(&b'#') {
            return None;
        }
        while line.last() == Some(&b' ') {
            let slashes = line[..line.len() - 1]
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count();
            if slashes % 2 == 1 {
                break;
            }
            line = &line[..line.len() - 1];
        }
        let negative = line.first() == Some(&b'!');
        if negative {
            line = &line[1..];
        }
        if line.is_empty() {
            return None;
        }
        let anchored = line.first() == Some(&b'/');
        if anchored {
            line = &line[1..];
        }
        let directory_only = line.last() == Some(&b'/');
        if directory_only {
            line = &line[..line.len() - 1];
        }
        let basename = !anchored && !line.contains(&b'/');
        Some(Self {
            pattern: line.to_vec(),
            negative,
            directory_only,
            basename,
        })
    }
    fn matches(&self, mut path: &[u8], is_dir: bool) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        if self.basename {
            if let Some(slash) = path.iter().rposition(|&b| b == b'/') {
                path = &path[slash + 1..];
            }
        }
        gix_glob::wildmatch(
            self.pattern.as_slice().into(),
            path.into(),
            gix_glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
        )
    }
}

#[derive(Debug)]
pub struct Policy {
    tracked: BTreeSet<PathBuf>,
    rules: BTreeMap<PathBuf, Vec<Rule>>,
}
impl Policy {
    pub fn new(snapshot: &Snapshot) -> Result<Self> {
        let mut rules = BTreeMap::new();
        for (path, entry) in &snapshot.entries {
            if path.file_name() != Some(std::ffi::OsStr::new(".gitignore"))
                || entry.kind != EntryKind::File
            {
                continue;
            }
            let dir = path.parent().unwrap_or(Path::new(""));
            // Git recognizes only the UTF-8 BOM, not arbitrary Unicode BOMs.
            let bytes = entry
                .data
                .strip_prefix(b"\xef\xbb\xbf")
                .unwrap_or(&entry.data);
            let parsed: Vec<Rule> = bytes
                .split(|&b| b == b'\n')
                .filter_map(Rule::parse)
                .collect();
            // gix wildmatch has a recursion guard at 64. Every recursive frame
            // consumes at least one star; conservatively bound raw star bytes
            // so exhaustion can never masquerade as a nonmatching negation.
            if parsed
                .iter()
                .any(|rule| rule.pattern.iter().filter(|&&b| b == b'*').count() >= 64)
            {
                bail!(
                    "ignore pattern exceeds the supported 63-star complexity limit: {}",
                    path.display()
                );
            }
            rules.insert(dir.to_path_buf(), parsed);
        }
        Ok(Self {
            tracked: snapshot.entries.keys().cloned().collect(),
            rules,
        })
    }
    /// True only for a new, ignored relative path; tracked names stay immutable.
    pub fn allows(&self, path: &Path, is_dir: bool) -> bool {
        if path.as_os_str().is_empty() || self.tracked.contains(path) {
            return false;
        }
        let components: Vec<_> = path.components().collect();
        if components
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return false;
        }
        let mut current = PathBuf::new();
        let mut applicable = Vec::new();
        if let Some(rules) = self.rules.get(Path::new("")) {
            applicable.push((Path::new(""), rules));
        }
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            let directory = index + 1 < components.len() || is_dir;
            let mut ignored = false;
            for (base, rules) in &applicable {
                let relative = current.strip_prefix(base).expect("ancestor ignore base");
                for rule in *rules {
                    if rule.matches(relative.as_os_str().as_bytes(), directory) {
                        ignored = !rule.negative;
                    }
                }
            }
            // Negation below an excluded parent cannot re-include a child.
            if ignored {
                return true;
            }
            if directory {
                if let Some((base, rules)) = self.rules.get_key_value(&current) {
                    applicable.push((base.as_path(), rules));
                }
            }
        }
        false
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use std::ffi::OsStr;
    use tempfile::TempDir;
    fn fixture(root: &str, nested: &str) -> (TempDir, Snapshot, Policy) {
        let tmp = TempDir::new().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let root_ignore = repo.blob(root.as_bytes()).unwrap();
        let nested_ignore = repo.blob(nested.as_bytes()).unwrap();
        let content = repo.blob(b"tracked").unwrap();
        let mut child = repo.treebuilder(None).unwrap();
        child.insert(".gitignore", nested_ignore, 0o100644).unwrap();
        child.insert("existing.log", content, 0o100644).unwrap();
        let child_oid = child.write().unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".gitignore", root_ignore, 0o100644).unwrap();
        tree.insert("nested", child_oid, 0o040000).unwrap();
        let oid = tree.write().unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "policy").unwrap();
        let policy = Policy::new(&snap).unwrap();
        (tmp, snap, policy)
    }
    #[test]
    fn nested_negation_and_tracked_names() {
        let (_tmp, _snap, p) = fixture("*.log\n/root-only\n", "!keep.log\n*.tmp\n");
        for path in ["a.log", "nested/new.log", "nested/new.tmp", "root-only"] {
            assert!(p.allows(Path::new(path), false), "{path}");
        }
        for path in [
            "nested",
            "nested/existing.log",
            "nested/keep.log",
            "nested/root-only",
            "new.tmp",
            "ordinary",
            "",
        ] {
            assert!(!p.allows(Path::new(path), false), "{path}");
        }
    }
    #[test]
    fn excluded_parent_cannot_be_reincluded() {
        let (_tmp, _snap, p) = fixture("nested/\n!nested/keep.log\n", "!keep.log\n");
        assert!(p.allows(Path::new("nested/keep.log"), false));
        assert!(!p.allows(Path::new("nested/existing.log"), false));
        assert!(!p.allows(Path::new("nested"), true));
    }
    #[test]
    fn negating_parent_allows_specific_rules() {
        let (_tmp, _snap, p) = fixture("*\n!nested/\nnested/*\n!nested/keep\n", "!keep\n");
        assert!(!p.allows(Path::new("nested/keep"), false));
        assert!(p.allows(Path::new("nested/new"), false));
    }
    #[test]
    fn directory_patterns_and_snapshot_only() {
        let (tmp, _snap, p) = fixture("cache/\n", "");
        std::fs::write(tmp.path().join(".gitignore"), "*\n").unwrap();
        assert!(!p.allows(Path::new("cache"), false));
        assert!(p.allows(Path::new("cache"), true));
        assert!(p.allows(Path::new("cache/anything"), false));
        assert!(p.allows(Path::new("nested/cache/anything"), false));
        assert!(!p.allows(Path::new("anything"), false));
        assert!(!p.allows(Path::new("../cache/anything"), false));
        assert!(!p.allows(Path::new("/cache/anything"), false));
    }
    #[test]
    fn matches_git_check_ignore_oracle() {
        let rule_sets = [
            ("\u{feff}*.log\n", ""),
            ("*.log\n/root-only\n", "!keep.log\n*.tmp\n"),
            ("nested/\n!nested/keep.log\n", "!keep.log\n"),
            ("*\n!nested/\nnested/*\n!nested/keep\n", "!keep\n"),
            ("cache/\n**/generated/**\n", "!generated/keep\n"),
            (
                "\\#literal\n\\!literal\nspace\\ \ntrimmed   \nfile?.[ch]\n",
                "",
            ),
            ("**/*.tmp\n!keep.tmp\n", "/keep.tmp\n!deep/keep.tmp\n"),
            ("nested/*\n!nested/deep/\n", "deep/*\n!deep/keep\n"),
        ];
        let candidates = [
            "a.log",
            "keep.log",
            "root-only",
            "nested/root-only",
            "nested/keep.log",
            "nested/new.log",
            "nested/new.tmp",
            "new.tmp",
            "nested/keep",
            "nested/deep/keep",
            "nested/deep/new",
            "cache",
            "cache/a",
            "nested/cache/a",
            "generated/a",
            "nested/generated/keep",
            "#literal",
            "!literal",
            "space ",
            "space",
            "trimmed",
            "trimmed   ",
            "file1.c",
            "file2.h",
            "file22.c",
            "keep.tmp",
            "nested/keep.tmp",
            "nested/deep/keep.tmp",
            "nested/deep/other.tmp",
            "ordinary",
        ];
        for (root, nested) in rule_sets {
            let (tmp, _snap, policy) = fixture(root, nested);
            std::fs::write(tmp.path().join(".gitignore"), root).unwrap();
            std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
            std::fs::write(tmp.path().join("nested/.gitignore"), nested).unwrap();
            for path in candidates {
                let status = std::process::Command::new("git")
                    .current_dir(tmp.path())
                    .args([
                        "-c",
                        "core.excludesFile=/dev/null",
                        "-c",
                        "core.ignoreCase=false",
                        "check-ignore",
                        "--no-index",
                        "-q",
                        "--",
                        path,
                    ])
                    .status()
                    .unwrap();
                assert!(matches!(status.code(), Some(0 | 1)));
                assert_eq!(
                    policy.allows(Path::new(path), false),
                    status.success(),
                    "path={path:?}, root={root:?}, nested={nested:?}"
                );
            }
            std::fs::create_dir_all(tmp.path().join("cache")).unwrap();
            let status = std::process::Command::new("git")
                .current_dir(tmp.path())
                .args([
                    "-c",
                    "core.excludesFile=/dev/null",
                    "check-ignore",
                    "--no-index",
                    "-q",
                    "--",
                    "cache/",
                ])
                .status()
                .unwrap();
            assert_eq!(policy.allows(Path::new("cache"), true), status.success());
        }
    }
    #[test]
    fn symlink_ignore_is_not_followed() {
        let tmp = TempDir::new().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let pattern = repo.blob(b"*\n").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".gitignore", pattern, 0o120000).unwrap();
        let oid = tree.write().unwrap();
        let snap =
            Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "symlink-ignore").unwrap();
        let policy = Policy::new(&snap).unwrap();
        assert!(!policy.allows(Path::new("new-file"), false));
    }
    #[test]
    fn adversarial_patterns_match_git_byte_for_byte() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut patterns = vec![
            "{foo,bar}",
            "foo\t",
            "foo\u{a0}",
            "[abc",
            "*\n!\t",
            "\t",
            "foo\\ ",
            "foo\\  ",
            "foo\\\\ ",
            "foo\\\\  ",
            "foo\\\\\\ ",
            "foo\r",
            "foo\r\r",
            "foo\0bar",
            "foo \0bar",
            "foo\0 ",
            "\0foo",
            "foo\r ",
            "foo \r",
            "foo\0\nbar",
            "foo\u{b}",
            "\\!foo",
            "\\#foo",
            "\\$foo",
            "!$foo",
            "$foo",
            "#foo",
            " #foo",
            "[]",
            "[!]",
            "[!a]",
            "[^a]",
            "[[:alpha:]]",
            "[[:digit:]]",
            "[[:bogus:]]",
            "[a-z]",
            "[z-a]",
            "[a\\-z]",
            "[a/b]",
            "[]a]",
            "[a[]",
            "[[]",
            "a\\",
            "**",
            "***",
            "a**b",
            "a/**/b",
            "a**/b",
            "a/**b",
            "**/foo",
            "foo/**",
            "/foo",
            "//foo",
            "foo/",
            "foo//",
            "foo\\/",
            "foo/**/",
            "!",
            "/",
            "\\",
            "\\u{feff}foo",
            "\u{feff}foo",
            "foo\n!foo",
            "*\n!foo\t",
            "*\n!foo\u{a0}",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        for atom in [
            "*",
            "?",
            "[ab]",
            "[!ab]",
            "[^ab]",
            "[[:space:]]",
            "[abc",
            "{a,b}",
            "\\*",
            "\\?",
            "\\[",
            "**",
        ] {
            for prefix in ["", "foo", "/", "nested/", "**/"] {
                for suffix in ["", ".log", " ", "\t", "/"] {
                    patterns.push(format!("{prefix}{atom}{suffix}"));
                }
            }
        }
        let negative_patterns: Vec<_> = patterns.iter().map(|p| format!("*\n!{p}")).collect();
        patterns.extend(negative_patterns);
        let mut candidates = [
            "foo",
            "bar",
            "a",
            "b",
            "c",
            "1",
            "A",
            "$foo",
            "!foo",
            "#foo",
            " #foo",
            "{foo,bar}",
            "{a,b}",
            "[abc",
            "[",
            "]",
            "[]",
            "*",
            "?",
            "\\",
            "a\\",
            "foo\\",
            "foo ",
            "foo  ",
            "foo\\ ",
            "foo\t",
            "foo\u{a0}",
            "foo\r",
            "foo\r\r",
            "foo\u{b}",
            "\t",
            " ",
            "\u{feff}foo",
            "a/b",
            "a/x/b",
            "ax/b",
            "a/xb",
            "foo/x",
            "foo/deep/x",
            "a.log",
            "foo.log",
            "foo*.log",
            "foo?.log",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let base = candidates.clone();
        for path in base {
            candidates.push(format!("nested/{path}"));
            candidates.push(format!("nested/deep/{path}"));
        }
        for root in patterns {
            let nested = "!bar\n*.tmp\n";
            let (tmp, _snap, policy) = fixture(&root, nested);
            std::fs::write(tmp.path().join(".gitignore"), root.as_bytes()).unwrap();
            std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
            std::fs::write(tmp.path().join("nested/.gitignore"), nested).unwrap();
            for is_dir in [false, true] {
                if is_dir {
                    for path in &candidates {
                        std::fs::create_dir_all(tmp.path().join(path)).unwrap();
                    }
                }
                let mut child = Command::new("git")
                    .current_dir(tmp.path())
                    .args([
                        "-c",
                        "core.excludesFile=/dev/null",
                        "-c",
                        "core.ignoreCase=false",
                        "check-ignore",
                        "--no-index",
                        "--stdin",
                        "-z",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                {
                    let mut input = child.stdin.take().unwrap();
                    for path in &candidates {
                        input.write_all(path.as_bytes()).unwrap();
                        input.write_all(&[0]).unwrap();
                    }
                }
                let output = child.wait_with_output().unwrap();
                assert!(matches!(output.status.code(), Some(0 | 1)));
                let ignored: BTreeSet<_> = output
                    .stdout
                    .split(|&b| b == 0)
                    .filter(|v| !v.is_empty())
                    .collect();
                for path in &candidates {
                    assert_eq!(
                        policy.allows(Path::new(path), is_dir),
                        ignored.contains(path.as_bytes()),
                        "pattern={root:?} path={path:?} is_dir={is_dir}"
                    );
                }
            }
        }
    }
    #[test]
    fn non_utf8_patterns_and_names_match_git() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for bytes in [
            b"\xff\n".as_slice(),
            b"*\n!\xff\n",
            b"[a-\xff]\n",
            b"\xff\xfefoo\n",
            b"[[:alpha:]]\n",
        ] {
            let tmp = TempDir::new().unwrap();
            let repo = Repository::init(tmp.path()).unwrap();
            let blob = repo.blob(bytes).unwrap();
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert(".gitignore", blob, 0o100644).unwrap();
            let oid = tree.write().unwrap();
            let snap =
                Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "raw-bytes").unwrap();
            let policy = Policy::new(&snap).unwrap();
            std::fs::write(tmp.path().join(".gitignore"), bytes).unwrap();
            let candidates = [
                b"\xff".as_slice(),
                b"\xff\xfefoo",
                b"foo",
                b"a",
                b"\xc3\xa9",
                b"nested/\xff",
            ];
            let mut child = Command::new("git")
                .current_dir(tmp.path())
                .args([
                    "-c",
                    "core.excludesFile=/dev/null",
                    "check-ignore",
                    "--no-index",
                    "--stdin",
                    "-z",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            {
                let mut input = child.stdin.take().unwrap();
                for path in candidates {
                    input.write_all(path).unwrap();
                    input.write_all(&[0]).unwrap();
                }
            }
            let output = child.wait_with_output().unwrap();
            assert!(matches!(output.status.code(), Some(0 | 1)));
            let ignored: BTreeSet<_> = output
                .stdout
                .split(|&b| b == 0)
                .filter(|v| !v.is_empty())
                .collect();
            for path in candidates {
                assert_eq!(
                    policy.allows(Path::new(OsStr::from_bytes(path)), false),
                    ignored.contains(path),
                    "pattern={bytes:?} path={path:?}"
                );
            }
        }
    }
    #[test]
    fn excessive_wildcards_fail_before_they_can_hide_a_negation() {
        let tmp = TempDir::new().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let pattern = format!("*\n!{}\n", "*a".repeat(64));
        let blob = repo.blob(pattern.as_bytes()).unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".gitignore", blob, 0o100644).unwrap();
        let oid = tree.write().unwrap();
        let snapshot =
            Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "complexity").unwrap();
        assert!(Policy::new(&snapshot)
            .unwrap_err()
            .to_string()
            .contains("63-star"));
    }
}
