//! Eager, byte-preserving non-FUSE execution view for explicit write-policy=all.
use crate::{
    platform::{open_child_directory, DirectoryEntries},
    snapshot::{EntryKind, Snapshot},
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::CStr,
    fs::{self, OpenOptions},
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

/// After checking that no mount remains, restore only owned physical directory
/// permissions. A child may have removed all owner access from its disposable
/// view. Walk pinned directory descriptors; never traverse a snapshot symlink.
pub fn prepare_cleanup(run: &fs::File) -> Result<()> {
    restore_at(run.as_raw_fd(), c"mount")
}

fn restore_at(parent: libc::c_int, name: &CStr) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error).context("inspect disposable directory")
        };
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Ok(());
    }
    ensure!(
        stat.st_uid == unsafe { libc::geteuid() },
        "refusing to chmod a foreign directory"
    );
    let open = || open_child_directory(parent, name, libc::O_RDONLY);
    let directory = match open() {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            restore_locked_directory(parent, name, &stat)?;
            open()
        }
        result => result,
    }
    .context("open disposable directory for cleanup")?;
    let metadata = checked_directory_metadata(&directory, &stat)?;
    if metadata.mode() & 0o700 != 0o700 {
        directory.set_permissions(fs::Permissions::from_mode(metadata.mode() | 0o700))?;
    }
    let mut entries =
        DirectoryEntries::from_file(directory).context("read disposable directory")?;
    let directory = entries.as_raw_fd();
    while let Some((name, kind)) = entries
        .next_entry()
        .context("enumerate disposable directory")?
    {
        if matches!(kind, libc::DT_DIR | libc::DT_UNKNOWN) {
            restore_at(directory, name)?;
        }
    }
    Ok(())
}

fn checked_directory_metadata(file: &fs::File, expected: &libc::stat) -> Result<fs::Metadata> {
    let metadata = file.metadata()?;
    #[cfg(target_os = "linux")]
    let device = expected.st_dev;
    #[cfg(target_os = "macos")]
    let device = expected.st_dev as u64;
    ensure!(
        metadata.ino() == expected.st_ino
            && metadata.dev() == device
            && metadata.uid() == expected.st_uid,
        "disposable directory changed during cleanup"
    );
    Ok(metadata)
}

#[cfg(target_os = "linux")]
fn restore_locked_directory(parent: libc::c_int, name: &CStr, expected: &libc::stat) -> Result<()> {
    // O_PATH pins and checks the actual mount without requiring read access.
    let directory = open_child_directory(parent, name, libc::O_PATH)?;
    let metadata = checked_directory_metadata(&directory, expected)?;
    fs::set_permissions(
        format!("/proc/self/fd/{}", directory.as_raw_fd()),
        fs::Permissions::from_mode(metadata.mode() | 0o700),
    )
    .context("restore locked disposable directory")
}

#[cfg(target_os = "macos")]
fn restore_locked_directory(parent: libc::c_int, name: &CStr, expected: &libc::stat) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(parent, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect cleanup parent");
    }
    ensure!(
        unsafe { stat.assume_init() }.st_dev == expected.st_dev,
        "a mount remains below the disposable directory"
    );
    let mode = (expected.st_mode & 0o7777) | 0o700;
    if unsafe { libc::fchmodat(parent, name.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error()).context("restore locked disposable directory");
    }
    Ok(())
}

struct PreparedBlob {
    path: PathBuf,
    size: u64,
    kind: EntryKind,
}
impl PreparedBlob {
    fn copy_to(&self, root: &Path, oid: git2::Oid, output: &mut dyn std::io::Write) -> Result<()> {
        match self.kind {
            EntryKind::Symlink => self.with_symlink_target(root, oid, |bytes| {
                crate::object::write_verified_buffer(bytes, output)
            }),
            EntryKind::File => {
                let mut input = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(root.join(&self.path))?;
                crate::spool::private_file_metadata(&input)?;
                crate::object::copy_verified(
                    &mut input,
                    output,
                    git2::ObjectType::Blob,
                    usize::try_from(self.size)?,
                    oid,
                )
            }
            EntryKind::Directory => unreachable!("only prepared blobs are retained"),
        }
    }

    fn with_symlink_target<T>(
        &self,
        root: &Path,
        oid: git2::Oid,
        consume: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        if self.kind == EntryKind::Symlink {
            let target = fs::read_link(root.join(&self.path))?;
            let bytes = target.as_os_str().as_bytes();
            let mut verifier = crate::object::VerifiedWriter::new(
                std::io::sink(),
                git2::ObjectType::Blob,
                usize::try_from(self.size)?,
                oid,
            );
            crate::object::write_verified_buffer(bytes, &mut verifier)?;
            verifier.finish()?;
            consume(bytes)
        } else {
            let mut target = Vec::new();
            self.copy_to(root, oid, &mut target)?;
            consume(&target)
        }
    }
}

pub fn materialize(snapshot: &Snapshot, root: &Path) -> Result<()> {
    // Finish any pack already used by lazy reads so the copy can reuse its
    // verified extra objects without admitting them to the hot blob cache.
    snapshot.verify_materialization_packs()?;
    snapshot.set_spool_directory(root)?;
    directory(
        snapshot,
        snapshot.tree_oid,
        Path::new(""),
        root,
        &mut std::collections::HashMap::new(),
    )?;
    snapshot.finish_materialization()
}
fn directory(
    snapshot: &Snapshot,
    oid: git2::Oid,
    relative: &Path,
    root: &Path,
    prepared: &mut std::collections::HashMap<git2::Oid, PreparedBlob>,
) -> Result<()> {
    let source = snapshot.directory_by_oid(oid)?;
    let children = Snapshot::directory_children(&source, relative).collect::<Result<Vec<_>>>()?;
    for batch in children.chunks(256) {
        snapshot.prepare_materialization(batch, |oid| prepared.contains_key(&oid))?;
        for (name, entry) in batch {
            if let Some(signal) = crate::git::cancelled_signal() {
                bail!("materialization cancelled by signal {signal}");
            }
            let bytes = name.as_bytes();
            ensure!(
                !bytes.is_empty()
                    && bytes != b"."
                    && bytes != b".."
                    && !bytes.contains(&b'/')
                    && !bytes.contains(&0),
                "unsupported snapshot name {:?}",
                name
            );
            let relative = relative.join(name);
            let path = root.join(&relative);
            ensure!(
                entry.mode != 0o160000,
                "submodules are unsupported: {}",
                relative.display()
            );
            let result = (|| -> Result<()> {
                match entry.kind {
                    EntryKind::Directory => {
                        fs::create_dir(&path)?;
                        directory(snapshot, entry.oid, &relative, root, prepared)?;
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                        ensure!(
                            fs::metadata(&path)?.permissions().mode() & 0o777 == 0o755,
                            "host cannot preserve directory permissions"
                        );
                    }
                    EntryKind::Symlink => {
                        let create = |target: &[u8]| -> Result<u64> {
                            ensure!(
                                !target.is_empty() && !target.contains(&0),
                                "unusable symlink target"
                            );
                            symlink(std::ffi::OsStr::from_bytes(target), &path)?;
                            ensure!(
                                fs::read_link(&path)?.as_os_str().as_bytes() == target,
                                "host cannot preserve symlink target"
                            );
                            Ok(target.len() as u64)
                        };
                        let size = match prepared.get(&entry.oid) {
                            Some(source) => source.with_symlink_target(root, entry.oid, create)?,
                            None => snapshot.with_materialized_symlink_target(entry.oid, create)?,
                        };
                        prepared.entry(entry.oid).or_insert_with(|| PreparedBlob {
                            path: relative.clone(),
                            size,
                            kind: entry.kind,
                        });
                    }
                    EntryKind::File => {
                        let mode = entry.mode & 0o777;
                        let mut output = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(mode)
                            .custom_flags(libc::O_NOFOLLOW)
                            .open(&path)?;
                        if let Some(source) = prepared.get(&entry.oid) {
                            // Reuse decoded bytes across directories, while checking
                            // the copied buffers against the original OID. Prepared
                            // files are mutable disk and cannot be trusted on reopen.
                            source.copy_to(root, entry.oid, &mut output)?;
                        } else {
                            let size = snapshot.copy_verified_blob(entry.oid, &mut output)?;
                            prepared.insert(
                                entry.oid,
                                PreparedBlob {
                                    path: relative.clone(),
                                    size,
                                    kind: entry.kind,
                                },
                            );
                        }
                        // Normally creation already applied the final mode.
                        // Repair and verify bits removed by the caller's umask.
                        if output.metadata()?.permissions().mode() & 0o777 != mode {
                            output.set_permissions(fs::Permissions::from_mode(mode))?;
                            ensure!(
                                output.metadata()?.permissions().mode() & 0o777 == mode,
                                "host cannot preserve executable/permission bits"
                            );
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
                {
                    let existing =
                        fs::read_dir(path.parent().context("missing materialized parent")?)?
                            .map(|entry| entry.map(|entry| entry.file_name()))
                            .collect::<std::io::Result<Vec<_>>>()?;
                    bail!("snapshot path {} collides with existing host entries {:?}; this filesystem cannot represent the snapshot faithfully", relative.display(), existing);
                }
                return Err(error).with_context(|| format!("cannot faithfully materialize {} (unsupported name, symlink/permission limitation, or object error)", relative.display()));
            }
        }
    }
    let actual = fs::read_dir(root.join(relative))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    ensure!(
        actual.len() == source.len() && actual.iter().all(|name| source.index(name).is_some()),
        "host changed snapshot names in {}: expected {:?}, found {:?}",
        relative.display(),
        source.children().map(|(name, ..)| name).collect::<Vec<_>>(),
        actual
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eager_reuse_decodes_once_across_directories_and_file_link_kinds() {
        let source = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(source.path()).unwrap();
        let blob = repo.blob(b"target").unwrap();
        let link_first = repo.blob(b"other-target").unwrap();
        let mut child = repo.treebuilder(None).unwrap();
        child.insert("file", blob, 0o100644).unwrap();
        child.insert("link", blob, 0o120000).unwrap();
        child.insert("other-file", link_first, 0o100644).unwrap();
        let child = child.write().unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("a-file", blob, 0o100644).unwrap();
        root.insert("b-link", link_first, 0o120000).unwrap();
        root.insert("directory", child, 0o040000).unwrap();
        let tree = root.write().unwrap();
        let snapshot = Snapshot::load(
            source.path(),
            std::ffi::OsStr::new(&tree.to_string()),
            "eager-reuse",
        )
        .unwrap();
        let storage = tempfile::tempdir().unwrap();
        let destination = storage.path().join("view");
        fs::create_dir(&destination).unwrap();
        materialize(&snapshot, &destination).unwrap();
        assert_eq!(snapshot.metrics().blob_loads, 2);
        assert_eq!(
            fs::read(destination.join("directory/file")).unwrap(),
            b"target"
        );
        assert_eq!(
            fs::read_link(destination.join("directory/link")).unwrap(),
            Path::new("target")
        );
        assert_eq!(
            fs::read(destination.join("directory/other-file")).unwrap(),
            b"other-target"
        );
        fs::write(destination.join("a-file"), b"changed").unwrap();
        assert_eq!(
            fs::read(destination.join("directory/file")).unwrap(),
            b"target"
        );
    }

    #[test]
    fn prepared_files_and_link_targets_are_reverified_on_reuse() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"original";
        let oid = git2::Oid::hash_object(git2::ObjectType::Blob, bytes).unwrap();
        for kind in [EntryKind::File, EntryKind::Symlink] {
            let path = root.path().join("prepared");
            if kind == EntryKind::File {
                fs::write(&path, bytes).unwrap();
            } else {
                symlink("original", &path).unwrap();
            }
            let prepared = PreparedBlob {
                path: "prepared".into(),
                size: bytes.len() as u64,
                kind,
            };
            let mut output = Vec::new();
            prepared.copy_to(root.path(), oid, &mut output).unwrap();
            assert_eq!(output, bytes);
            if kind == EntryKind::File {
                fs::write(&path, b"tampered").unwrap();
            } else {
                fs::remove_file(&path).unwrap();
                symlink("tampered", &path).unwrap();
            }
            assert!(prepared.copy_to(root.path(), oid, &mut Vec::new()).is_err());
            fs::remove_file(path).unwrap();
        }
    }
}
