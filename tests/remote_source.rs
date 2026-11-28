//! Local source directory identity through the snapshot loader.
use git2::{Repository, Signature};
use git_on::snapshot::Snapshot;
use std::ffi::OsStr;

#[test]
fn explicit_local_source_preserves_the_selected_git_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let run = tempfile::tempdir_in(fixture.path()).unwrap();
    let source = fixture.path().join("source.git");
    let repo = Repository::init_bare(&source).unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let sig = Signature::now("Test", "test@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "local directory fixture",
        &repo.find_tree(tree).unwrap(),
        &[],
    )
    .unwrap();
    #[allow(unused_mut)]
    let mut paths = vec![source.clone()];
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let alias = fixture.path().join(OsStr::from_bytes(b"source-\xff.git"));
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        paths.push(alias);
    }
    for path in paths {
        let snapshot = Snapshot::load_source(
            fixture.path(),
            Some(path.as_os_str()),
            OsStr::new("HEAD"),
            false,
            false,
            run.path(),
        )
        .unwrap();
        assert_eq!(
            snapshot.source_git_dir().unwrap().canonicalize().unwrap(),
            source.canonicalize().unwrap()
        );
        assert_eq!(snapshot.tree_oid, tree);
        assert!(!run.path().join("remote-repository").exists());
    }
}
