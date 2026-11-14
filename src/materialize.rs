//! Eager, byte-preserving non-FUSE execution view for explicit write-policy=all.
use crate::snapshot::{EntryKind, Snapshot};
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::{CStr, CString},
    fs::{self, OpenOptions},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

/// After checking that no mount remains, restore only owned physical directory
/// permissions. A child may have removed all owner access from its disposable
/// view. Walk pinned directory descriptors; never traverse a snapshot symlink.
pub fn prepare_cleanup(root: &Path) -> Result<()> {
    let root = CString::new(root.as_os_str().as_bytes())?;
    restore_at(libc::AT_FDCWD, &root)
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
    let mode = (stat.st_mode & 0o7777) | 0o700;
    let open = || unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let mut fd = open();
    if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied {
        // Opening a mode-000 directory first requires a no-follow chmod. This
        // operation also refuses a symlink substituted between stat and chmod.
        if unsafe { libc::fchmodat(parent, name.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("restore locked disposable directory");
        }
        fd = open();
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open disposable directory without following links");
    }
    let directory = unsafe { fs::File::from_raw_fd(fd) };
    use std::os::unix::fs::MetadataExt;
    let metadata = directory.metadata()?;
    #[cfg(target_os = "linux")]
    let device = stat.st_dev;
    #[cfg(target_os = "macos")]
    let device = stat.st_dev as u64;
    ensure!(
        metadata.ino() == stat.st_ino && metadata.dev() == device && metadata.uid() == stat.st_uid,
        "disposable directory changed during cleanup"
    );
    directory.set_permissions(fs::Permissions::from_mode(metadata.mode() | 0o700))?;
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error())
            .context("duplicate cleanup directory descriptor");
    }
    struct Entries(*mut libc::DIR);
    impl Drop for Entries {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let entries = unsafe { libc::fdopendir(duplicate) };
    if entries.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(duplicate);
        }
        return Err(error).context("read disposable directory");
    }
    let entries = Entries(entries);
    loop {
        // readdir returns null for either EOF or error; clear/check errno.
        #[cfg(target_os = "linux")]
        let errno = unsafe { libc::__errno_location() };
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        unsafe {
            *errno = 0;
        }
        let entry = unsafe { libc::readdir(entries.0) };
        if entry.is_null() {
            if unsafe { *errno } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("enumerate disposable directory");
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            restore_at(directory.as_raw_fd(), name)?;
        }
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
        let copy = |input: &mut dyn std::io::Read| {
            crate::object::copy_verified(
                input,
                output,
                git2::ObjectType::Blob,
                usize::try_from(self.size)?,
                oid,
            )
        };
        match self.kind {
            EntryKind::Symlink => {
                let target = fs::read_link(root.join(&self.path))?;
                copy(&mut target.as_os_str().as_bytes())
            }
            EntryKind::File => {
                let mut input = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(root.join(&self.path))?;
                crate::spool::private_file_metadata(&input)?;
                copy(&mut input)
            }
            EntryKind::Directory => unreachable!("only prepared blobs are retained"),
        }
    }
}

pub fn materialize(snapshot: &Snapshot, root: &Path) -> Result<()> {
    let _progress = crate::progress::MaterializationProgress::start();
    snapshot.set_spool_directory(root)?;
    snapshot.begin_materialization()?;
    let batch_size = if crate::spool::Files::size_limit()?.is_some() {
        1
    } else {
        256
    };
    directory(
        snapshot,
        Path::new(""),
        root,
        &mut std::collections::HashMap::new(),
        batch_size,
    )
}
fn directory(
    snapshot: &Snapshot,
    relative: &Path,
    root: &Path,
    prepared: &mut std::collections::HashMap<git2::Oid, PreparedBlob>,
    batch_size: usize,
) -> Result<()> {
    let mut children = snapshot.children(relative)?.into_iter();
    let mut expected = std::collections::HashSet::new();
    loop {
        let batch: Vec<_> = children.by_ref().take(batch_size).collect();
        if batch.is_empty() {
            break;
        }
        snapshot.prepare_materialization(&batch, |oid| prepared.contains_key(&oid))?;
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
            let relative = relative.join(&name);
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
                        directory(snapshot, &relative, root, prepared, batch_size)?;
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                        ensure!(
                            fs::metadata(&path)?.permissions().mode() & 0o777 == 0o755,
                            "host cannot preserve directory permissions"
                        );
                    }
                    EntryKind::Symlink => {
                        let target = if let Some(source) = prepared.get(&entry.oid) {
                            let mut target = Vec::new();
                            source.copy_to(root, entry.oid, &mut target)?;
                            ensure!(
                                !target.is_empty() && !target.contains(&0),
                                "unusable symlink target"
                            );
                            target
                        } else {
                            let mut target = Vec::new();
                            snapshot.copy_verified_blob(entry.oid, &mut target)?;
                            ensure!(
                                !target.is_empty() && !target.contains(&0),
                                "unusable symlink target"
                            );
                            target
                        };
                        symlink(std::ffi::OsStr::from_bytes(&target), &path)?;
                        ensure!(
                            fs::read_link(&path)?.as_os_str().as_bytes() == target,
                            "host cannot preserve symlink target"
                        );
                        prepared.entry(entry.oid).or_insert_with(|| PreparedBlob {
                            path: relative.clone(),
                            size: target.len() as u64,
                            kind: entry.kind,
                        });
                    }
                    EntryKind::File => {
                        let mut output = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
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
                        let mode = entry.mode & 0o777;
                        output.set_permissions(fs::Permissions::from_mode(mode))?;
                        ensure!(
                            output.metadata()?.permissions().mode() & 0o777 == mode,
                            "host cannot preserve executable/permission bits"
                        );
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
            expected.insert(name);
        }
    }
    let actual = fs::read_dir(root.join(relative))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    ensure!(
        actual == expected,
        "host changed snapshot names in {}: expected {:?}, found {:?}",
        relative.display(),
        expected,
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
