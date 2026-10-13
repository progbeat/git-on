//! Write modes and demand-loaded ignore rules from pinned .gitignore blobs.
use crate::snapshot::{EntryKind, Snapshot};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Controls which paths may acquire private, temporary changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WritePolicy {
    None,
    #[default]
    Ignored,
    Untracked,
    All,
}

impl std::str::FromStr for WritePolicy {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "ignored" => Ok(Self::Ignored),
            "untracked" => Ok(Self::Untracked),
            "all" => Ok(Self::All),
            _ => Err("write policy must be none, ignored, untracked, or all"),
        }
    }
}

#[derive(Debug)]
struct Rule {
    pattern: crate::wildmatch::Pattern,
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
            pattern: crate::wildmatch::Pattern::new(line),
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
        self.pattern.matches(path)
    }
}

const RULE_CACHE_BYTES: usize = 4 * 1024 * 1024;
const RULE_CACHE_ENTRIES: usize = 128;

#[derive(Debug, Default)]
struct RulesCache {
    entries: BTreeMap<PathBuf, (Arc<Vec<Rule>>, usize, u64)>,
    bytes: usize,
    clock: u64,
}
impl RulesCache {
    fn get(&mut self, path: &Path) -> Option<Arc<Vec<Rule>>> {
        let (rules, _, used) = self.entries.get_mut(path)?;
        self.clock = self.clock.saturating_add(1);
        *used = self.clock;
        Some(rules.clone())
    }
    fn insert(&mut self, path: &Path, rules: Arc<Vec<Rule>>, cost: usize) {
        if cost > RULE_CACHE_BYTES {
            return;
        }
        if let Some((_, old_cost, _)) = self.entries.remove(path) {
            self.bytes -= old_cost;
        }
        while self.entries.len() >= RULE_CACHE_ENTRIES
            || self.bytes > RULE_CACHE_BYTES.saturating_sub(cost)
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, _, used))| used)
                .map(|(path, _)| path.clone())
                .expect("rule cache has a victim");
            let (_, removed, _) = self.entries.remove(&oldest).unwrap();
            self.bytes -= removed;
        }
        self.clock = self.clock.saturating_add(1);
        self.entries
            .insert(path.to_path_buf(), (rules, cost, self.clock));
        self.bytes += cost;
    }
}
#[derive(Debug)]
pub struct Policy {
    snapshot: Snapshot,
    mode: WritePolicy,
    rules: Mutex<RulesCache>,
}
impl Policy {
    pub fn new(snapshot: &Snapshot) -> Result<Self> {
        Self::with_mode(snapshot, WritePolicy::Ignored)
    }
    pub fn with_mode(snapshot: &Snapshot, mode: WritePolicy) -> Result<Self> {
        Ok(Self {
            snapshot: snapshot.clone(),
            mode,
            rules: Mutex::new(RulesCache::default()),
        })
    }
    fn rules_at(&self, directory: &Path) -> Result<Arc<Vec<Rule>>> {
        let mut cache = self
            .rules
            .lock()
            .map_err(|_| anyhow::anyhow!("ignore rule cache poisoned"))?;
        if let Some(rules) = cache.get(directory) {
            return Ok(rules);
        }
        let mut source_bytes = 0usize;
        let rules = match self.snapshot.entry(&directory.join(".gitignore"))? {
            Some(entry) if entry.kind == EntryKind::File => {
                self.snapshot.with_blob(entry.oid, |data| {
                    source_bytes = data.len();
                    // Git recognizes only the UTF-8 BOM.
                    let bytes = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
                    Ok(bytes
                        .split(|&b| b == b'\n')
                        .filter_map(Rule::parse)
                        .collect())
                })?
            }
            _ => Vec::new(),
        };
        let rules = Arc::new(rules);
        // Conservative compiled-memory charge: a byte can become a wildcard
        // atom/class allocation. Also charge paths, empty rules and map overhead.
        // Oversized rules still work for this operation but are never retained.
        let cost = source_bytes
            .saturating_mul(512)
            .saturating_add(directory.as_os_str().as_bytes().len())
            .saturating_add(1024);
        cache.insert(directory, rules.clone(), cost);
        Ok(rules)
    }
    /// True when the configured policy permits this relative path. Object/rule
    /// errors fail closed; ignore rules are loaded only in ignored mode.
    pub fn allows(&self, path: &Path, is_dir: bool) -> bool {
        self.check(path, is_dir).unwrap_or(false)
    }
    pub(crate) fn check(&self, path: &Path, is_dir: bool) -> Result<bool> {
        if path.as_os_str().is_empty() {
            return Ok(false);
        }
        let components: Vec<_> = path.components().collect();
        if components
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Ok(false);
        }
        match self.mode {
            WritePolicy::None => return Ok(false),
            WritePolicy::All => return Ok(true),
            WritePolicy::Ignored | WritePolicy::Untracked => {}
        }
        // Protect tracked paths without indexing their siblings or descendants.
        // A non-directory tracked ancestor is an error, never write permission.
        if self.snapshot.entry(path)?.is_some() {
            return Ok(false);
        }
        if self.mode == WritePolicy::Untracked {
            return Ok(true);
        }
        let mut current = PathBuf::new();
        let mut applicable = vec![(PathBuf::new(), self.rules_at(Path::new(""))?)];
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            let directory = index + 1 < components.len() || is_dir;
            let mut ignored = false;
            for (base, rules) in &applicable {
                let relative = current.strip_prefix(base).expect("ancestor ignore base");
                for rule in rules.iter() {
                    if rule.matches(relative.as_os_str().as_bytes(), directory) {
                        ignored = !rule.negative;
                    }
                }
            }
            // Negation below an excluded parent cannot re-include a child.
            if ignored {
                return Ok(true);
            }
            if index + 1 < components.len() {
                applicable.push((current.clone(), self.rules_at(&current)?));
            }
        }
        Ok(false)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use std::collections::BTreeSet;
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
    fn modes_preserve_pinned_paths_and_only_ignored_demands_rules() {
        let (_temp, snapshot, _) = fixture("*.tmp\n", "");
        let before = snapshot.metrics();
        for (mode, new_plain, new_ignored, tracked) in [
            (WritePolicy::None, false, false, false),
            (WritePolicy::Untracked, true, true, false),
            (WritePolicy::All, true, true, true),
        ] {
            let policy = Policy::with_mode(&snapshot, mode).unwrap();
            assert_eq!(policy.allows(Path::new("plain"), false), new_plain);
            assert_eq!(
                policy.allows(Path::new("generated.tmp"), false),
                new_ignored
            );
            assert_eq!(
                policy.allows(Path::new("nested/existing.log"), false),
                tracked
            );
            assert_eq!(policy.allows(Path::new("nested"), true), tracked);
            for path in ["", "/absolute", "../escape", "nested/../escape"] {
                assert!(!policy.allows(Path::new(path), false), "{mode:?} {path}");
            }
        }
        assert_eq!(snapshot.metrics().blob_loads, before.blob_loads);
        let policy = Policy::new(&snapshot).unwrap();
        assert!(!policy.allows(Path::new("plain"), false));
        assert!(policy.allows(Path::new("generated.tmp"), false));
        assert!(snapshot.metrics().blob_loads > before.blob_loads);
    }
    #[test]
    fn documented_ignore_cache_budget_matches_runtime() {
        let expected = format!(
            "{} MiB / {} compiled ignore-rule entries",
            RULE_CACHE_BYTES / (1024 * 1024),
            RULE_CACHE_ENTRIES
        );
        assert!(include_str!("../docs/usage.md").contains(&expected));
    }
    #[test]
    fn policy_initialization_and_ancestor_loading_are_lazy() {
        let (_tmp, snapshot, policy) = fixture("*.log\n", "!keep.log\n");
        assert_eq!(snapshot.metrics().tree_reads, 0);
        assert_eq!(snapshot.metrics().header_reads, 0);
        assert_eq!(snapshot.metrics().blob_loads, 0);
        assert!(policy.allows(Path::new("root.log"), false));
        assert_eq!(snapshot.metrics().tree_reads, 1);
        assert_eq!(snapshot.metrics().blob_loads, 1);
        assert!(!policy.allows(Path::new("nested/keep.log"), false));
        assert_eq!(snapshot.metrics().tree_reads, 2);
        assert_eq!(snapshot.metrics().blob_loads, 2);
        assert!(policy.allows(Path::new("nested/other.log"), false));
        assert_eq!(snapshot.metrics().blob_loads, 2);
        assert_eq!(snapshot.metrics().tree_reads, 2);
    }
    #[test]
    fn policy_owns_pin_and_errors_fail_closed() {
        let (tmp, snapshot, policy) = fixture("*\n", "");
        let repo = Repository::open(tmp.path()).unwrap();
        drop(snapshot);
        assert!(repo.find_reference("refs/git-on/policy").is_ok());
        assert!(!policy.allows(Path::new("nested/existing.log/child"), false));
        assert!(!policy.allows(Path::new("nested/existing.log"), false));
        assert!(policy.allows(Path::new("untracked"), false));
        drop(policy);
        assert!(repo.find_reference("refs/git-on/policy").is_err());
        for content in [
            b"*\n".as_slice(),
            b"version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize 123\n",
        ] {
            let tmp = TempDir::new().unwrap();
            let repo = Repository::init(tmp.path()).unwrap();
            let blob = repo.blob(content).unwrap();
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert(".gitignore", blob, 0o100644).unwrap();
            let oid = tree.write().unwrap();
            if content == b"*\n" {
                let hex = blob.to_string();
                std::fs::remove_file(repo.path().join("objects").join(&hex[..2]).join(&hex[2..]))
                    .unwrap();
            }
            let snapshot =
                Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "failure").unwrap();
            let policy = Policy::new(&snapshot).unwrap();
            assert!(!policy.allows(Path::new("new-file"), false));
        }
    }
    #[test]
    fn rule_cache_bounds_entries_bytes_and_bypasses_oversized_rules() {
        let (_tmp, _snapshot, policy) = fixture("", "");
        for n in 0..RULE_CACHE_ENTRIES + 10 {
            policy.rules_at(Path::new(&format!("absent-{n}"))).unwrap();
        }
        {
            let cache = policy.rules.lock().unwrap();
            assert_eq!(cache.entries.len(), RULE_CACHE_ENTRIES);
            assert!(cache.bytes <= RULE_CACHE_BYTES);
            assert!(!cache.entries.contains_key(Path::new("absent-0")));
        }
        let pattern = format!("{}\n*.log\n", "#ignored comment\n".repeat(1000));
        let (_tmp, _snapshot, policy) = fixture(&pattern, "");
        assert!(policy.allows(Path::new("allowed.log"), false));
        assert!(policy.rules.lock().unwrap().entries.is_empty());
        // Test byte-pressure eviction independently of entry pressure.
        let mut cache = RulesCache::default();
        for n in 0..3 {
            cache.insert(
                Path::new(&format!("dir-{n}")),
                Arc::new(Vec::new()),
                RULE_CACHE_BYTES / 2,
            );
        }
        assert_eq!(cache.bytes, RULE_CACHE_BYTES);
        assert_eq!(cache.entries.len(), 2);
        assert!(!cache.entries.contains_key(Path::new("dir-0")));
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
    fn deep_wildcards_cannot_hide_a_negation() {
        for count in [63, 64, 65, 100, 1000] {
            let pattern = format!("*\n!{}\n", "*a".repeat(count));
            let (_tmp, _snapshot, policy) = fixture(&pattern, "");
            assert!(!policy.allows(Path::new(&"a".repeat(count)), false));
            assert!(policy.allows(Path::new(&"a".repeat(count - 1)), false));
        }
    }
    #[test]
    fn deep_patterns_match_git_oracle() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for count in [63, 64, 65, 100] {
            for (atom, matching) in [
                ("*a", "a"),
                ("*[ab]", "a"),
                ("*\\*", "*"),
                ("*[[:digit:]]", "1"),
                ("*?", "a"),
            ] {
                for negative in [false, true] {
                    let root = format!(
                        "{}{}\n",
                        if negative { "*\n!" } else { "" },
                        atom.repeat(count)
                    );
                    let (tmp, _, policy) = fixture(&root, "");
                    std::fs::write(tmp.path().join(".gitignore"), &root).unwrap();
                    let paths = [
                        matching.repeat(count),
                        matching.repeat(count - 1),
                        matching.repeat(count + 1),
                        format!("{}z", matching.repeat(count)),
                    ];
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
                        let mut stdin = child.stdin.take().unwrap();
                        for path in &paths {
                            stdin.write_all(path.as_bytes()).unwrap();
                            stdin.write_all(&[0]).unwrap();
                        }
                    }
                    let output = child.wait_with_output().unwrap();
                    assert!(matches!(output.status.code(), Some(0 | 1)));
                    let ignored: BTreeSet<_> = output
                        .stdout
                        .split(|&b| b == 0)
                        .filter(|v| !v.is_empty())
                        .collect();
                    for path in paths {
                        assert_eq!(
                            policy.allows(Path::new(&path), false),
                            ignored.contains(path.as_bytes()),
                            "pattern={root:?}, path={path:?}"
                        );
                    }
                }
            }
        }
    }
}
