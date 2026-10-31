//! Object/network demand checks without a FUSE mount. file:// still uses Git's
//! real upload-pack protocol and filters; SSH/HTTPS are separately exercised.
use git2::{Oid, Repository, Signature};
use git_on::snapshot::Snapshot;
use std::{ffi::OsStr, path::Path, process::Command};
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
        // Blob payloads are verified raw run data, rather than a second
        // recompressed copy in the metadata ODB.
        3
    );
    assert_eq!(snap.metrics().remote_raw_blob_imports, 1);
    let pack_bytes = (
        snap.metrics().remote_native_pack_bytes,
        snap.metrics().remote_session_pack_bytes,
    );
    assert!(pack_bytes.0 > 0);
    assert!(pack_bytes.0 + pack_bytes.1 > b"selected\n".len() as u64);
    let fetches = snap.metrics().remote_fetches;
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    assert_eq!(snap.metrics().remote_fetches, fetches);
    assert_eq!(snap.metrics().blob_loads, 1);
    assert_eq!(
        (
            snap.metrics().remote_native_pack_bytes,
            snap.metrics().remote_session_pack_bytes,
        ),
        pack_bytes,
        "same-run cache hits must not receive another pack"
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
fn eager_remote_batches_are_bounded_and_reuse_objects_across_batch_boundaries() {
    let (temp, parent, _, _) = fixture();
    let server = Repository::open_bare(temp.path().join("server")).unwrap();
    let mut tree = server.treebuilder(None).unwrap();
    for index in 0..300 {
        let bytes = format!("distinct-{index}\n");
        tree.insert(
            format!("f{index:04}"),
            server.blob(bytes.as_bytes()).unwrap(),
            0o100644,
        )
        .unwrap();
    }
    let shared = server.blob(b"shared\n").unwrap();
    tree.insert("a-shared", shared, 0o100644).unwrap();
    tree.insert("z-shared", shared, 0o100644).unwrap();
    let tree = tree.write().unwrap();
    let sig = Signature::now("Test", "test@example.invalid").unwrap();
    server
        .commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "bounded batches",
            &server.find_tree(tree).unwrap(),
            &[&server.find_commit(parent).unwrap()],
        )
        .unwrap();
    let snapshot = load(temp.path(), "HEAD", false).unwrap();
    let destination = temp.path().join("view");
    std::fs::create_dir(&destination).unwrap();
    git_on::materialize::materialize(&snapshot, &destination).unwrap();
    for index in 0..300 {
        assert_eq!(
            std::fs::read(destination.join(format!("f{index:04}"))).unwrap(),
            format!("distinct-{index}\n").as_bytes()
        );
    }
    let metrics = snapshot.metrics();
    assert_eq!(metrics.remote_fetches, 303);
    assert_eq!(
        metrics.remote_fetch_operations + metrics.remote_session_fetch_operations,
        4
    );
    assert_eq!(
        metrics.remote_decode_operations + metrics.remote_session_decode_operations,
        4
    );
    assert_eq!(metrics.blob_loads, 301);
    assert_eq!(
        std::fs::read(destination.join("a-shared")).unwrap(),
        b"shared\n"
    );
    assert_eq!(
        std::fs::read(destination.join("z-shared")).unwrap(),
        b"shared\n"
    );
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
fn review_run_odb_fetch_failure_is_latched() {
    let (temporary, _, _, _) = fixture();
    // Explicit fsck selects the native compatibility path whose pack-directory
    // failure this regression exercises. Session storage has its own control.
    Repository::init(temporary.path())
        .unwrap()
        .config()
        .unwrap()
        .set_bool("fetch.fsckObjects", false)
        .unwrap();
    let snapshot = load(temporary.path(), "HEAD", false).unwrap();
    // Break the private pack destination before a lazy subtree demand. The
    // native fetch must fail, and an application catching EIO cannot turn the
    // resulting incomplete remote execution into success.
    let packs = snapshot.git_dir.join("objects/pack");
    std::fs::remove_dir(&packs).unwrap();
    std::fs::write(&packs, b"blocked private pack directory").unwrap();
    assert!(snapshot.entry(Path::new("deep/value")).is_err());
    let failure = snapshot
        .remote_failure()
        .expect("private ODB fetch failure was not latched");
    std::fs::remove_file(&packs).unwrap();
    std::fs::create_dir(&packs).unwrap();
    assert!(snapshot.entry(Path::new("deep/value")).is_err());
    assert_eq!(snapshot.remote_failure().as_deref(), Some(failure.as_str()));
}

#[test]
fn session_staging_failure_is_latched_after_the_storage_is_repaired() {
    let (temporary, _, _, _) = fixture();
    let snapshot = load(temporary.path(), "HEAD", false).unwrap();
    snapshot.children(Path::new("")).unwrap();
    let parts = std::fs::read_dir(snapshot.git_dir.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("raw-parts-")
        })
        .unwrap();
    let mut substituted = Vec::new();
    for id in 0..256 {
        let path = parts.join(format!("{id:x}-0"));
        if !path.exists() {
            std::os::unix::fs::symlink(temporary.path().join("foreign-target"), &path).unwrap();
            substituted.push(path);
        }
    }
    let error = snapshot.entry(Path::new("deep/value"));
    for path in substituted {
        let _ = std::fs::remove_file(path);
    }
    assert!(error.is_err());
    let failure = snapshot
        .remote_failure()
        .expect("session staging failure was not latched");
    assert!(snapshot.entry(Path::new("deep/value")).is_err());
    assert_eq!(snapshot.remote_failure().as_deref(), Some(failure.as_str()));
    assert!(!temporary.path().join("foreign-target").exists());
}
