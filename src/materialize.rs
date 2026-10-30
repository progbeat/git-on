//! Eager, byte-preserving non-FUSE execution view for explicit write-policy=all.
use crate::snapshot::{EntryKind, Snapshot};
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::{CStr, CString},
    fs::{self, OpenOptions},
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
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

pub fn materialize(snapshot: &Snapshot, root: &Path) -> Result<()> {
    let _progress = crate::progress::MaterializationProgress::start();
    snapshot.set_spool_directory(root)?;
    directory(snapshot, Path::new(""), root)
}
fn directory(snapshot: &Snapshot, relative: &Path, root: &Path) -> Result<()> {
    let children = snapshot.children(relative)?;
    snapshot.prepare_materialization(&children)?;
    let mut expected = Vec::new();
    for (name, _) in children {
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
        let entry = snapshot
            .entry(&relative)?
            .context("snapshot entry disappeared")?;
        let result = (|| -> Result<()> {
            match entry.kind {
                EntryKind::Directory => {
                    fs::create_dir(&path)?;
                    directory(snapshot, &relative, root)?;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                    ensure!(
                        fs::metadata(&path)?.permissions().mode() & 0o777 == 0o755,
                        "host cannot preserve directory permissions"
                    );
                }
                EntryKind::Symlink => {
                    let target = snapshot.read_link(entry.oid)?;
                    symlink(std::ffi::OsStr::from_bytes(&target), &path)?;
                    ensure!(
                        fs::read_link(&path)?.as_os_str().as_bytes() == target,
                        "host cannot preserve symlink target"
                    );
                }
                EntryKind::File => {
                    let mut output = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&path)?;
                    let reader = snapshot.open(entry.oid)?;
                    let size = entry.size.context("missing blob size")?;
                    let mut offset = 0;
                    while offset < size {
                        if let Some(signal) = crate::git::cancelled_signal() {
                            bail!("materialization cancelled by signal {signal}");
                        }
                        let data = reader.read(offset, (size - offset).min(64 * 1024) as u32)?;
                        ensure!(!data.is_empty(), "truncated Git blob");
                        output.write_all(&data)?;
                        offset += data.len() as u64;
                    }
                    // Also validate empty LFS pointers/objects through the ordinary reader.
                    if size == 0 {
                        reader.read(0, 1)?;
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
                let existing = fs::read_dir(path.parent().context("missing materialized parent")?)?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                bail!("snapshot path {} collides with existing host entries {:?}; this filesystem cannot represent the snapshot faithfully", relative.display(), existing);
            }
            return Err(error).with_context(|| format!("cannot faithfully materialize {} (unsupported name, symlink/permission limitation, or object error)", relative.display()));
        }
        expected.push(name);
    }
    let mut actual = fs::read_dir(root.join(relative))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    expected.sort();
    actual.sort();
    ensure!(
        actual == expected,
        "host changed snapshot names in {}: expected {:?}, found {:?}",
        relative.display(),
        expected,
        actual
    );
    Ok(())
}
