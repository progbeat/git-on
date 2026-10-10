//! Immutable snapshot read only from Git's object database.
use anyhow::{bail, Context, Result};
use git2::{Oid, Repository, Tree};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: EntryKind,
    pub data: Vec<u8>,
    pub mode: u32,
}
#[derive(Debug)]
struct Pin {
    git_dir: PathBuf,
    name: String,
    oid: Oid,
}
impl Drop for Pin {
    fn drop(&mut self) {
        if let Ok(repo) = Repository::open(&self.git_dir) {
            if let Ok(mut reference) = repo.find_reference(&self.name) {
                if reference.target() == Some(self.oid) {
                    let _ = reference.delete();
                }
            };
        }
    }
}
#[derive(Debug)]
pub struct Snapshot {
    pub tree_oid: Oid,
    pub entries: BTreeMap<PathBuf, Entry>,
    pub git_dir: PathBuf,
    pub pin_ref: String,
    _pin: Pin,
}
impl Snapshot {
    pub fn load(repo_path: &Path, treeish: &OsStr, run_id: &str) -> Result<Self> {
        Self::load_with_pin_record(repo_path, treeish, run_id, |_, _, _| Ok(()))
    }
    /// Persist the planned pin before creating it, so interrupted loads are recoverable.
    pub fn load_with_pin_record<F>(
        repo_path: &Path,
        treeish: &OsStr,
        run_id: &str,
        mut record: F,
    ) -> Result<Self>
    where
        F: FnMut(&Path, &str, Oid) -> Result<()>,
    {
        let spec = treeish.to_str().context("tree-ish must be valid UTF-8")?;
        let repo = Repository::discover(repo_path).context("discover Git repository")?;
        let object = repo
            .revparse_single(spec)
            .with_context(|| format!("resolve tree-ish {spec:?}"))?;
        let tree = object
            .peel_to_tree()
            .context("tree-ish does not resolve to a tree")?;
        let tree_oid = tree.id();
        let pin_ref = format!("refs/git-on/{run_id}");
        if run_id.is_empty() || !git2::Reference::is_valid_name(&pin_ref) {
            bail!("invalid snapshot run ID");
        }
        record(repo.path(), &pin_ref, tree_oid)?;
        repo.reference(&pin_ref, tree_oid, false, "git-on snapshot pin")
            .context("pin snapshot tree against garbage collection")?;
        let git_dir = repo.path().to_path_buf();
        let pin = Pin {
            git_dir: git_dir.clone(),
            name: pin_ref.clone(),
            oid: tree_oid,
        };
        let mut entries = BTreeMap::new();
        entries.insert(
            PathBuf::new(),
            Entry {
                kind: EntryKind::Directory,
                data: Vec::new(),
                mode: 0o040000,
            },
        );
        Self::read_tree(&repo, &tree, Path::new(""), &mut entries)?;
        Ok(Self {
            tree_oid,
            entries,
            git_dir,
            pin_ref,
            _pin: pin,
        })
    }
    fn read_tree(
        repo: &Repository,
        tree: &Tree<'_>,
        base: &Path,
        entries: &mut BTreeMap<PathBuf, Entry>,
    ) -> Result<()> {
        for item in tree.iter() {
            let name = OsString::from_vec(item.name_bytes().to_vec());
            if item.name_bytes().contains(&b'/') || name == "." || name == ".." || name.is_empty() {
                bail!("invalid name in Git tree");
            }
            let path = base.join(name);
            let mode = item.filemode() as u32;
            let kind = match mode {
                0o040000 => EntryKind::Directory,
                0o100644 | 0o100755 => EntryKind::File,
                0o120000 => EntryKind::Symlink,
                0o160000 => bail!("submodules are unsupported: {}", path.display()),
                _ => bail!("unsupported Git mode {mode:o}: {}", path.display()),
            };
            let data = if kind == EntryKind::Directory {
                Vec::new()
            } else {
                let blob = repo
                    .find_blob(item.id())
                    .with_context(|| format!("read blob {}", path.display()))?;
                let content = blob.content();
                if content.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
                    || content.starts_with(b"version https://git-lfs.github.com/spec/v1\r\n")
                {
                    bail!("Git LFS pointer blobs are unsupported: {}", path.display());
                }
                if kind == EntryKind::Symlink && (content.is_empty() || content.contains(&0)) {
                    bail!("unusable symlink target: {}", path.display());
                }
                content.to_vec()
            };
            if entries
                .insert(path.clone(), Entry { kind, data, mode })
                .is_some()
            {
                bail!("duplicate path in Git tree: {}", path.display());
            }
            if kind == EntryKind::Directory {
                Self::read_tree(repo, &repo.find_tree(item.id())?, &path, entries)?;
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use tempfile::TempDir;
    fn fixture() -> (TempDir, Oid) {
        let tmp = TempDir::new().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let blob = repo.blob(b"original\n").unwrap();
        let target = repo.blob(b"script").unwrap();
        let mut nested = repo.treebuilder(None).unwrap();
        nested.insert("child", blob, 0o100644).unwrap();
        let nested_oid = nested.write().unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("script", blob, 0o100755).unwrap();
        builder.insert("naïve space", blob, 0o100644).unwrap();
        builder.insert("link", target, 0o120000).unwrap();
        builder.insert("dir", nested_oid, 0o040000).unwrap();
        let oid = builder.write().unwrap();
        let sig = Signature::now("Test", "test@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "initial",
            &repo.find_tree(oid).unwrap(),
            &[],
        )
        .unwrap();
        (tmp, oid)
    }
    #[test]
    fn reads_objects_modes_links_and_cleans_pin() {
        let (tmp, oid) = fixture();
        std::fs::write(tmp.path().join("script"), b"dirty checkout").unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "test-pin").unwrap();
        assert_eq!(snap.tree_oid, oid);
        assert_eq!(snap.entries[Path::new("naïve space")].data, b"original\n");
        assert_eq!(snap.entries[Path::new("script")].data, b"original\n");
        assert_eq!(snap.entries[Path::new("script")].mode, 0o100755);
        assert_eq!(snap.entries[Path::new("link")].kind, EntryKind::Symlink);
        assert_eq!(snap.entries[Path::new("link")].data, b"script");
        let repo = Repository::open(tmp.path()).unwrap();
        assert_eq!(
            repo.find_reference("refs/git-on/test-pin")
                .unwrap()
                .target(),
            Some(oid)
        );
        assert!(Snapshot::load(tmp.path(), OsStr::new("HEAD"), "test-pin").is_err());
        drop(snap);
        assert!(repo.find_reference("refs/git-on/test-pin").is_err());
        assert_eq!(
            std::fs::read(tmp.path().join("script")).unwrap(),
            b"dirty checkout"
        );
    }
    #[test]
    fn accepts_raw_tree_and_subtree() {
        let (tmp, oid) = fixture();
        let snap = Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "raw").unwrap();
        assert_eq!(snap.tree_oid, oid);
        let sub = Snapshot::load(tmp.path(), OsStr::new("HEAD:dir"), "sub").unwrap();
        assert!(sub.entries.contains_key(Path::new("child")));
        assert!(!sub.entries.contains_key(Path::new("dir")));
        assert!(sub.entries.contains_key(Path::new("")));
    }
    #[test]
    fn rejects_lfs_submodules_and_cleans_failed_pin() {
        for submodule in [false, true] {
            let (tmp, _) = fixture();
            let repo = Repository::open(tmp.path()).unwrap();
            let oid = if submodule {
                repo.head().unwrap().target().unwrap()
            } else {
                repo.blob(b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n")
                    .unwrap()
            };
            let mut builder = repo.treebuilder(None).unwrap();
            builder
                .insert(
                    "unsupported",
                    oid,
                    if submodule { 0o160000 } else { 0o100644 },
                )
                .unwrap();
            let tree = builder.write().unwrap();
            assert!(Snapshot::load(tmp.path(), OsStr::new(&tree.to_string()), "failure").is_err());
            assert!(repo.find_reference("refs/git-on/failure").is_err());
        }
    }
    #[test]
    fn invocation_from_worktree_subdirectory() {
        let (tmp, oid) = fixture();
        let directory = tmp.path().join("some/deep/path");
        std::fs::create_dir_all(&directory).unwrap();
        let snap = Snapshot::load(&directory, OsStr::new("HEAD"), "discovery").unwrap();
        assert_eq!(snap.tree_oid, oid);
    }
    #[test]
    fn durable_pin_record_precedes_pin_and_failed_record_leaves_no_reference() {
        let (tmp, oid) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let result = Snapshot::load_with_pin_record(
            tmp.path(),
            OsStr::new(&oid.to_string()),
            "record",
            |dir, pin, tree| {
                assert_eq!(dir, repo.path());
                assert_eq!(tree, oid);
                assert!(repo.find_reference(pin).is_err());
                anyhow::bail!("simulated manifest failure")
            },
        );
        assert!(result.unwrap_err().to_string().contains("manifest failure"));
        assert!(repo.find_reference("refs/git-on/record").is_err());
    }
}
