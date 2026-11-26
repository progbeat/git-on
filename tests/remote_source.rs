//! Local source compatibility through the public snapshot loaders.
use git2::{Oid, Repository, Signature};
use git_on::snapshot::Snapshot;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
#[cfg(target_os = "linux")]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

const _: fn(&git_on::remote::Remote) -> anyhow::Result<()> = git_on::remote::Remote::stop_transport;

fn fixture() -> (tempfile::TempDir, Oid, Oid) {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(temp.path().join("server")).unwrap();
    let blob = repo.blob(b"selected\n").unwrap();
    let untouched = repo.blob(b"unrelated\n").unwrap();
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
    (temp, commit, tree)
}
fn record_pin(_: &Path, _: &str, _: git2_legacy::Oid) -> anyhow::Result<()> {
    panic!("loading a snapshot should leave Git references alone");
}

fn load_operands(
    root: &Path,
    source: &OsStr,
    rev: &OsStr,
    verify: bool,
) -> anyhow::Result<(Snapshot, PathBuf)> {
    // Production always owns a fresh private run. Keep it under the fixture so
    // all files stay within the fixture.
    let run = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)?
        .keep();
    let snapshot = Snapshot::load_source_with_pin_record(
        root,
        Some(source),
        rev,
        verify,
        false,
        &run,
        "test-pin",
        record_pin,
    )?;
    Ok((snapshot, run))
}

#[test]
fn legacy_local_loader_accepts_the_pin_callback_without_creating_refs() {
    let (temp, _, tree) = fixture();
    let path = temp.path().join("server");
    let snapshot =
        Snapshot::load_with_pin_record(&path, OsStr::new("HEAD"), "legacy-local", record_pin)
            .unwrap();
    assert_eq!(snapshot.tree_oid, tree);
    let repo = Repository::open_bare(&path).unwrap();
    assert!(repo
        .references_glob("refs/git-on/*")
        .unwrap()
        .next()
        .is_none());
}

#[test]
#[cfg(target_os = "linux")] // Linux host filenames can preserve these bytes; APFS rejects them.
fn direct_non_utf8_repository_paths_are_native_git_operands() {
    let (temp, _, tree) = fixture();
    let source = temp
        .path()
        .join(OsString::from_vec(b"source-\xff".to_vec()));
    std::os::unix::fs::symlink(temp.path().join("server"), &source).unwrap();
    let (snap, _) =
        load_operands(temp.path(), source.as_os_str(), OsStr::new("main"), false).unwrap();
    assert_eq!(snap.tree_oid, tree);
    let entry = snap.entry(Path::new("deep/value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
}
#[test]
fn local_materialization_preserves_the_selected_tree_without_a_checkout() {
    let (temp, _, tree) = fixture();
    let source = temp.path().join("server");
    let (snap, _) =
        load_operands(temp.path(), source.as_os_str(), OsStr::new("main"), false).unwrap();
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
fn materialization_reuses_objects_across_batch_boundaries() {
    let (temp, parent, _) = fixture();
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
    let (snapshot, _) = load_operands(
        temp.path(),
        server.path().as_os_str(),
        OsStr::new("HEAD:"),
        false,
    )
    .unwrap();
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
