//! Object/network demand checks without a FUSE mount. file:// still uses Git's
//! real upload-pack protocol and filters; SSH/HTTPS are separately exercised.
use git2::{Oid, Repository, Signature};
use git_on::snapshot::Snapshot;
use std::{
    ffi::OsStr,
    fs::OpenOptions,
    os::unix::{io::AsRawFd, process::ExitStatusExt},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn fixture() -> (tempfile::TempDir, Oid, Oid, Oid) {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(temp.path().join("server")).unwrap();
    repo.config()
        .unwrap()
        .set_bool("uploadpack.allowFilter", true)
        .unwrap();
    repo.config()
        .unwrap()
        .set_bool("uploadpack.allowAnySHA1InWant", true)
        .unwrap();
    let blob = repo.blob(b"selected\n").unwrap();
    let untouched = repo.blob(b"must stay unfetched\n").unwrap();
    let mut child = repo.treebuilder(None).unwrap();
    child.insert("value", blob, 0o100644).unwrap();
    let subtree = child.write().unwrap();
    let mut root = repo.treebuilder(None).unwrap();
    root.insert("deep", subtree, 0o040000).unwrap();
    root.insert("untouched", untouched, 0o100644).unwrap();
    let tree = root.write().unwrap();
    let sig = Signature::now("Test", "test@example.invalid").unwrap();
    let commit = repo
        .commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "fixture",
            &repo.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    (temp, commit, tree, untouched)
}
fn load(root: &Path, rev: &str, verify: bool) -> anyhow::Result<Snapshot> {
    let source = format!("file://{}", root.join("server").display());
    let run = root.join("run");
    std::fs::create_dir_all(&run)?;
    Snapshot::load_source_with_pin_record(
        root,
        Some(OsStr::new(&source)),
        OsStr::new(rev),
        verify,
        &run,
        "test",
        |_, _, _| Ok(()),
    )
}
#[test]
fn remote_metadata_and_payloads_are_demanded_and_pinned() {
    let (temp, commit, tree, untouched) = fixture();
    let snap = load(temp.path(), "HEAD", false).unwrap();
    assert_eq!(snap.tree_oid, tree);
    let run_repo = Repository::open_bare(&snap.git_dir).unwrap();
    let odb = run_repo.odb().unwrap();
    assert!(odb.exists(commit));
    assert!(!odb.exists(untouched));
    assert_eq!(snap.metrics().blob_loads, 0);
    let names = snap.children(Path::new("")).unwrap();
    assert_eq!(names.len(), 2);
    assert!(!odb.exists(untouched));
    let server = Repository::open_bare(temp.path().join("server")).unwrap();
    server
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    let entry = snap.entry(Path::new("deep/value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    odb.refresh().unwrap();
    assert!(!odb.exists(untouched));
    assert_eq!(snap.tree_oid, tree);
    let objects = Command::new("git")
        .arg("--git-dir")
        .arg(&snap.git_dir)
        .args(["cat-file", "--batch-all-objects", "--batch-check"])
        .output()
        .unwrap();
    assert!(objects.status.success());
    assert_eq!(
        objects
            .stdout
            .split(|x| *x == b'\n')
            .filter(|x| !x.is_empty())
            .count(),
        4
    );
}
#[test]
fn remote_subtrees_match_local_content() {
    let (temp, _, _, _) = fixture();
    let snap = load(temp.path(), "main:deep", false).unwrap();
    let entry = snap.entry(Path::new("value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    assert!(snap.entry(Path::new("untouched")).unwrap().is_none());
}
#[test]
fn review_same_oid_branch_and_tag_are_still_ambiguous() {
    let (temp, commit, tree, _) = fixture();
    let server = Repository::open_bare(temp.path().join("server")).unwrap();
    server
        .reference(
            "refs/tags/main",
            commit,
            false,
            "same-OID ambiguity fixture",
        )
        .unwrap();
    let error = load(temp.path(), "main", false).unwrap_err();
    assert!(
        error.to_string().contains("ambiguous remote revision"),
        "{error:#}"
    );
    // Explicit namespaces remain usable even when both references agree today.
    for revision in ["refs/heads/main", "refs/tags/main"] {
        assert_eq!(load(temp.path(), revision, false).unwrap().tree_oid, tree);
    }
}
#[test]
fn review_subtree_components_are_not_silently_normalized() {
    let (temp, _, tree, _) = fixture();
    for revision in [
        "main:deep/.",
        "main:deep/./",
        "main:deep//",
        "main:./deep",
        "main:deep/..",
    ] {
        let error = load(temp.path(), revision, false).unwrap_err();
        assert!(
            error.to_string().contains("REV:path"),
            "{revision}: {error:#}"
        );
    }
    // Native Git accepts an empty path and one trailing slash for a directory.
    assert_eq!(load(temp.path(), "main:", false).unwrap().tree_oid, tree);
    for revision in ["main:deep", "main:deep/"] {
        let snap = load(temp.path(), revision, false).unwrap();
        let entry = snap.entry(Path::new("value")).unwrap().unwrap();
        assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    }
}
#[test]
fn review_waiting_for_cache_lock_is_cancellable() {
    let (temp, _, _, _) = fixture();
    let cache = temp.path().join("cache/git-on/objects-v1");
    std::fs::create_dir_all(&cache).unwrap();
    let held_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache.join("lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held_lock.as_raw_fd(), libc::LOCK_EX) },
        0
    );
    let source = format!("file://{}", temp.path().join("server").display());
    let mut child = Command::new(env!("CARGO_BIN_EXE_git-on"))
        .current_dir(temp.path())
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("TMPDIR", temp.path())
        .args([source.as_str(), "HEAD", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The held cache lock prevents startup reaching FUSE. This is a competing
    // stopped writer, not a slow network or mount prerequisite.
    std::thread::sleep(Duration::from_millis(400));
    if let Some(status) = child.try_wait().unwrap() {
        panic!("runner exited before waiting on the cache: {status}");
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("runner ignored SIGTERM while another writer held the cache lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(!std::fs::read_dir(temp.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("git-on-")
    }));
}
#[test]
fn refused_filter_and_unsigned_objects_fail_before_execution() {
    let (temp, _, tree, _) = fixture();
    let error = load(temp.path(), "HEAD", true).unwrap_err();
    assert!(
        error.to_string().contains("execution forbidden"),
        "{error:#}"
    );
    let error = load(temp.path(), &tree.to_string(), true).unwrap_err();
    assert!(error.to_string().contains("raw trees"), "{error:#}");
    Repository::open_bare(temp.path().join("server"))
        .unwrap()
        .config()
        .unwrap()
        .set_bool("uploadpack.allowFilter", false)
        .unwrap();
    let error = load(temp.path(), "HEAD", false).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not advertise object filtering"),
        "{error:#}"
    );
}
