use git2::{ObjectType, Repository};
use git_on::snapshot::Snapshot;
use std::{ffi::OsStr, path::Path};

#[test]
fn out_of_range_tree_modes_stay_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let blob = repo.blob(b"payload").unwrap();
    let mut bytes = b"200644 file\0".to_vec();
    bytes.extend(blob.as_bytes());
    let tree = repo.odb().unwrap().write(ObjectType::Tree, &bytes).unwrap();
    assert!(
        repo.find_tree(tree).is_err(),
        "libgit2 should reject this mode"
    );
    let result = Snapshot::load(temp.path(), OsStr::new(&tree.to_string()), "mode-review")
        .and_then(|snapshot| snapshot.entry(Path::new("file")));
    assert!(
        result.is_err(),
        "out-of-range mode was silently accepted: {result:?}"
    );
}

#[test]
fn leading_zero_modes_preserve_native_tree_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let blob = repo.blob(b"payload").unwrap();
    let mut bytes = b"000000000000000000100644 file\0".to_vec();
    bytes.extend(blob.as_bytes());
    let tree = repo.odb().unwrap().write(ObjectType::Tree, &bytes).unwrap();
    assert_eq!(
        repo.find_tree(tree)
            .unwrap()
            .get_name("file")
            .unwrap()
            .filemode(),
        0o100644
    );
    let native = std::process::Command::new("git")
        .arg("-C")
        .arg(temp.path())
        .arg("ls-tree")
        .arg(tree.to_string())
        .output()
        .unwrap();
    assert!(native.status.success(), "{:?}", native.stderr);
    assert!(native.stdout.starts_with(b"100644 blob "));
    let snapshot = Snapshot::load(temp.path(), OsStr::new(&tree.to_string()), "zero-mode").unwrap();
    let entry = snapshot.entry(Path::new("file")).unwrap().unwrap();
    assert_eq!(entry.mode, 0o100644);
    assert_eq!(snapshot.read(entry.oid, 0, 100).unwrap(), b"payload");
}

#[test]
fn materialization_does_not_launch_on_an_out_of_range_mode() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let blob = repo.blob(b"payload").unwrap();
    let mut bytes = b"200644 file\0".to_vec();
    bytes.extend(blob.as_bytes());
    let tree = repo.odb().unwrap().write(ObjectType::Tree, &bytes).unwrap();
    let programs = temp.path().join("programs");
    std::fs::create_dir(&programs).unwrap();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    std::os::unix::fs::symlink(git, programs.join("git")).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_git-on"))
        .current_dir(temp.path())
        .env("PATH", programs)
        .args([
            &tree.to_string(),
            "--write-policy=all",
            "--",
            "/bin/echo",
            "child-must-not-start",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(125), "{:?}", result.stderr);
    assert!(result.stdout.is_empty());
}
