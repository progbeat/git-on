//! Object/network demand checks without a FUSE mount. file:// still uses Git's
//! real upload-pack protocol and filters; SSH/HTTPS are separately exercised.
use git2::{Oid, Repository, Signature};
use git_on::snapshot::Snapshot;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
#[cfg(target_os = "linux")]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

const _: fn(&git_on::remote::Remote) -> anyhow::Result<()> = git_on::remote::Remote::stop_transport;

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
    load_server(root, rev, verify).map(|(snapshot, _)| snapshot)
}
fn load_server(root: &Path, rev: &str, verify: bool) -> anyhow::Result<(Snapshot, PathBuf)> {
    let source = format!("file://{}", root.join("server").display());
    load_operands(root, OsStr::new(&source), OsStr::new(rev), verify)
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
    // repeated native fetches share the cache, never a previous FETCH_HEAD.
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
    let (temp, _, tree, _) = fixture();
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
fn corrupted_remote_raw_reads_latch_failure_after_reader_or_handle_close() {
    use std::os::unix::fs::FileExt;
    use std::time::{Duration, Instant};
    const SIZE: usize = 128 * 1024;
    let (temp, _, _, untouched) = fixture();
    let repo = Repository::open_bare(temp.path().join("server")).unwrap();
    let oid = repo.blob(&vec![b'v'; SIZE]).unwrap();
    let mut builder = repo.treebuilder(None).unwrap();
    builder.insert("file", oid, 0o100644).unwrap();
    let fillers: Vec<_> = (0..8)
        .map(|value| {
            let filler = repo.blob(&vec![value; 64 * 1024 + 1]).unwrap();
            builder
                .insert(format!("filler-{value}"), filler, 0o100644)
                .unwrap();
            filler
        })
        .collect();
    let tree = builder.write().unwrap();
    let sig = Signature::now("Test", "test@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "raw integrity fixture",
        &repo.find_tree(tree).unwrap(),
        &[&repo.head().unwrap().peel_to_commit().unwrap()],
    )
    .unwrap();
    for retained_reader in [false, true] {
        let (snap, run) = load_server(temp.path(), "HEAD", false).unwrap();
        let entry = snap.entry(Path::new("file")).unwrap().unwrap();
        let reader = retained_reader.then(|| snap.open(entry.oid).unwrap());
        assert_eq!(snap.read(oid, 0, 1).unwrap(), b"v");
        for (value, filler) in fillers.iter().enumerate() {
            assert_eq!(snap.read(*filler, 0, 1).unwrap(), [value as u8]);
        }
        // The raw cache belongs to the run. Native Git's directory may
        // be on a separate temporary filesystem.
        let store = std::fs::read_dir(run)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b"raw-objects-")
            })
            .unwrap();
        let path = store.join(oid.to_string());
        // Early reads can return while the background writer is still
        // publishing the evicted object and its block digests.
        let deadline = Instant::now() + Duration::from_secs(5);
        while snap.metrics().retained_raw_blob_bytes < SIZE as u64 {
            assert!(Instant::now() < deadline, "raw blob did not spill");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .write_all_at(b"!", 0)
            .unwrap();
        let read = |offset| match &reader {
            Some(reader) => reader.read(offset, 1),
            None => snap.read(entry.oid, offset, 1),
        };
        let error = read(0).unwrap_err();
        assert!(
            error.to_string().contains("bytes were modified"),
            "{error:#}"
        );
        let original = snap
            .remote_failure()
            .expect("caught raw EIO must still fail remote invocation");
        assert!(original.contains("bytes were modified"), "{original}");
        std::fs::remove_file(path).unwrap();
        assert!(read(65536).is_err());
        assert!(
            snap.read(untouched, 0, 1).is_err(),
            "a new demand ignored the sticky failure"
        );
        assert_eq!(
            snap.remote_failure().as_deref(),
            Some(original.as_str()),
            "later errors replaced the original failure"
        );
    }
}

#[test]
fn remote_metadata_and_payloads_stay_bound_to_the_selected_snapshot() {
    let (temp, _, tree, _) = fixture();
    let snap = load(temp.path(), "HEAD", false).unwrap();
    assert_eq!(snap.tree_oid, tree);
    let startup = snap.metrics();
    assert_eq!(startup.remote_fetches, 2);
    assert_eq!(startup.blob_loads, 0);
    let names = snap.children(Path::new("")).unwrap();
    assert_eq!(names.len(), 2);
    assert_eq!(snap.metrics().remote_fetches, startup.remote_fetches);
    let server = Repository::open_bare(temp.path().join("server")).unwrap();
    server
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    let entry = snap.entry(Path::new("deep/value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    assert_eq!(snap.tree_oid, tree);
    let first = snap.metrics();
    assert_eq!(first.remote_fetches, 4);
    assert_eq!(first.remote_raw_blob_imports, 1);
    let pack_bytes = (
        first.remote_native_pack_bytes,
        first.remote_session_pack_bytes,
    );
    assert!(pack_bytes.0 + pack_bytes.1 > b"selected\n".len() as u64);
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
    assert_eq!(snap.metrics().remote_fetches, first.remote_fetches);
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
    let run = tempfile::tempdir_in(temp.path()).unwrap();
    let cwd = std::env::current_dir().unwrap();
    let relative_run = cwd
        .components()
        .skip(1)
        .fold(PathBuf::new(), |path, _| path.join(".."))
        .join(run.path().strip_prefix("/").unwrap());
    let source = format!("file://{}", temp.path().join("server").display());
    let snap = Snapshot::load_source(
        temp.path(),
        Some(OsStr::new(&source)),
        OsStr::new("main:deep"),
        false,
        false,
        &relative_run,
    )
    .unwrap();
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
    let (snap, _) =
        load_operands(temp.path(), source.as_os_str(), OsStr::new("main"), false).unwrap();
    assert_eq!(snap.tree_oid, tree);
    let entry = snap.entry(Path::new("deep/value")).unwrap().unwrap();
    assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"selected\n");
}
#[test]
fn remote_materialization_preserves_the_selected_tree_without_a_checkout() {
    let (temp, _, tree, _) = fixture();
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
    let snapshot = load(temp.path(), "HEAD:", false).unwrap();
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
    assert!(metrics.remote_decode_operations + metrics.remote_session_decode_operations <= 1);
    assert_eq!(metrics.remote_raw_blob_imports, 0);
    assert_eq!(metrics.remote_raw_blob_bytes, 0);
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
    let packs = snapshot.git_dir.join("pack-staging");
    let target = std::fs::read_link(&packs).unwrap();
    std::fs::remove_file(&packs).unwrap();
    std::fs::write(&packs, b"blocked private pack directory").unwrap();
    assert!(snapshot.entry(Path::new("deep/value")).is_err());
    let failure = snapshot
        .remote_failure()
        .expect("private ODB fetch failure was not latched");
    std::fs::remove_file(&packs).unwrap();
    std::os::unix::fs::symlink(target, &packs).unwrap();
    assert!(snapshot.entry(Path::new("deep/value")).is_err());
    assert_eq!(snapshot.remote_failure().as_deref(), Some(failure.as_str()));
}
