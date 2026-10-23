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
#[cfg(target_os = "linux")]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

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
    load_operands(root, OsStr::new(&source), OsStr::new(rev), verify)
}
fn load_operands(
    root: &Path,
    source: &OsStr,
    rev: &OsStr,
    verify: bool,
) -> anyhow::Result<Snapshot> {
    // Production always owns a fresh private run. Keep it under the fixture so
    // repeated native fetches share the cache, never a previous FETCH_HEAD.
    let run = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)?
        .keep();
    Snapshot::load_source_with_pin_record(
        root,
        Some(source),
        rev,
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
fn remote_colon_refspec_selects_full_tree_in_a_private_repository() {
    let (temp, _, tree, _) = fixture();
    let snap = load(temp.path(), "main:deep", false).unwrap();
    assert_eq!(snap.tree_oid, tree);
    let entry = snap.entry(Path::new("deep/value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    let server = Repository::open_bare(temp.path().join("server")).unwrap();
    assert!(server.find_reference("refs/heads/deep").is_err());
}
#[test]
fn branch_and_tag_with_the_same_name_follow_native_fetch_precedence() {
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
    // Native fetch prefers the branch; git-on must not invent an ambiguity.
    for revision in ["main", "refs/heads/main", "refs/tags/main"] {
        assert_eq!(load(temp.path(), revision, false).unwrap().tree_oid, tree);
    }
}
#[test]
fn native_fetch_ref_names_force_prefix_and_destination_forms_are_accepted() {
    let (temp, _, tree, _) = fixture();
    for revision in [
        "heads/main",
        "+main",
        "",
        "main:",
        "main:refs/heads/selected",
    ] {
        assert_eq!(load(temp.path(), revision, false).unwrap().tree_oid, tree);
    }
}
#[test]
fn invalid_remote_refspecs_keep_native_git_diagnostics() {
    let (temp, _, _, _) = fixture();
    for revision in ["main:deep/.", "main:deep//", "main:deep/.."] {
        let error = load(temp.path(), revision, false).unwrap_err();
        assert!(
            error.to_string().contains("invalid refspec"),
            "{revision}: {error:#}"
        );
    }
}
#[test]
#[cfg(target_os = "linux")] // Linux host filenames can preserve these bytes; APFS rejects them.
fn direct_non_utf8_repository_paths_are_native_git_operands() {
    let (temp, _, tree, _) = fixture();
    let source = temp
        .path()
        .join(OsString::from_vec(b"source-\xff".to_vec()));
    std::os::unix::fs::symlink(temp.path().join("server"), &source).unwrap();
    let snap = load_operands(temp.path(), source.as_os_str(), OsStr::new("main"), false).unwrap();
    assert_eq!(snap.tree_oid, tree);
    let entry = snap.entry(Path::new("deep/value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
}
#[test]
fn remote_materialization_preserves_the_selected_tree_without_a_checkout() {
    let (temp, _, tree, _) = fixture();
    let source = temp.path().join("server");
    let snap = load_operands(temp.path(), source.as_os_str(), OsStr::new("main"), false).unwrap();
    let destination = temp.path().join("materialized");
    std::fs::create_dir(&destination).unwrap();
    git_on::materialize::materialize(&snap, &destination).unwrap();
    assert_eq!(snap.tree_oid, tree);
    assert_eq!(
        std::fs::read(destination.join("deep/value")).unwrap(),
        b"selected\n"
    );
    assert!(!destination.join(".git").exists());
}
#[test]
fn a_refspec_selecting_different_objects_cannot_choose_an_arbitrary_snapshot() {
    let (temp, _, tree, _) = fixture();
    let server = Repository::open_bare(temp.path().join("server")).unwrap();
    let sig = Signature::now("Test", "test@example.invalid").unwrap();
    server
        .commit(
            Some("refs/heads/second"),
            &sig,
            &sig,
            "different selected object",
            &server.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    let error = load(temp.path(), "refs/heads/*:refs/remotes/private/*", false).unwrap_err();
    assert!(
        format!("{error:#}").contains("selected multiple objects"),
        "{error:#}"
    );
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

#[test]
fn review_cache_restore_failure_is_latched() {
    // Isolate the process-wide cache environment from concurrent loader tests.
    if std::env::var_os("GIT_ON_REVIEW_CACHE_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "review_cache_restore_failure_is_latched",
                "--nocapture",
            ])
            .env("GIT_ON_REVIEW_CACHE_CHILD", "1")
            .env("XDG_CACHE_HOME", directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated cache failure regression: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let (temporary, _, _, _) = fixture();
    let snapshot = load(temporary.path(), "HEAD", false).unwrap();
    let cache = std::path::PathBuf::from(std::env::var_os("XDG_CACHE_HOME").unwrap())
        .join("git-on/objects-v1");
    // A competing/failed cache writer left an unusable directory entry. The
    // demanded subtree cannot be restored and the affected operation fails.
    std::fs::write(cache.join("unrecognized-entry"), b"fault").unwrap();
    let error = snapshot.entry(Path::new("deep/value")).unwrap_err();
    assert!(error
        .to_string()
        .contains("unrecognized remote cache entry"));
    // Returning EIO alone permits the child to catch it and exit 0. The runner
    // must retain the failure so that an incomplete execution returns 125.
    let failure = snapshot
        .remote_failure()
        .expect("demanded cache failure was not latched for runner failure");
    assert!(failure.contains("unrecognized remote cache entry"));
    std::fs::remove_file(cache.join("unrecognized-entry")).unwrap();
    // Removing the broken record does not authorize reconnecting/refetching
    // after the latched failure or replace the original diagnostic.
    assert!(snapshot.entry(Path::new("deep/value")).is_err());
    assert_eq!(snapshot.remote_failure().as_deref(), Some(failure.as_str()));
}
