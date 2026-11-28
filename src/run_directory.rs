use std::{
    ffi::{CStr, CString},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

const GROUP_NAME: &str = "git-on";
const OWNERSHIP: &str = ".git-on-owner";

pub(super) struct Group {
    path: PathBuf,
    metadata: fs::Metadata,
}

impl Group {
    pub(super) fn create() -> io::Result<(Option<Self>, PathBuf, String)> {
        let base = std::env::temp_dir();
        let base = fs::canonicalize(if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &base
        })?;
        Self::create_in(&base, rename_group)
    }

    fn create_in(
        base: &Path,
        rename: impl Fn(&CStr, &CStr) -> io::Result<()>,
    ) -> io::Result<(Option<Self>, PathBuf, String)> {
        let uid = unsafe { libc::geteuid() };
        for name in [GROUP_NAME.to_owned(), private_name(uid)] {
            let path = base.join(name);
            let mut retried_stale = false;
            loop {
                let directory = match open_group(&path) {
                    Ok(directory) => directory,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        if publish_group(base, &path, &rename)? {
                            continue;
                        }
                        break;
                    }
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::ENOTDIR | libc::EACCES | libc::ELOOP)
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error),
                };
                let metadata = directory.metadata()?;
                if !usable(&metadata, uid)
                    || metadata.mode() & 0o1777 != 0o1777 && !has_marker(&path, uid)
                {
                    break;
                }
                let id = uuid::Uuid::new_v4().to_string();
                let name = &id[..8];
                let entry = CString::new(name).expect("UUID is ASCII");
                let fd = directory.as_raw_fd();
                if unsafe { libc::mkdirat(fd, entry.as_ptr(), 0o700) } == 0 {
                    let run = path.join(name);
                    return Ok((Some(Self { path, metadata }), run, id));
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESTALE) && !retried_stale {
                    retried_stale = true;
                } else if !matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) {
                    return Err(error);
                }
            }
        }
        // Neither group is available. A standalone run also works on
        // filesystems that cannot publish a directory without replacing it.
        let run = tempfile::Builder::new()
            .prefix("git-on-")
            .rand_bytes(8)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(base)?
            .keep();
        Ok((None, run, uuid::Uuid::new_v4().to_string()))
    }

    pub(super) fn for_run(run: &Path) -> Option<Self> {
        let basename = run.file_name()?.as_encoded_bytes();
        if basename.len() != 8 || !basename.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let path = run.parent()?;
        let name = path.file_name()?;
        let uid = unsafe { libc::geteuid() };
        if name != GROUP_NAME && name != private_name(uid).as_str() {
            return None;
        }
        let metadata = path.symlink_metadata().ok()?;
        (usable(&metadata, uid) && has_marker(path, uid)).then(|| Self {
            path: path.to_owned(),
            metadata,
        })
    }
}

fn publish_group(
    base: &Path,
    path: &Path,
    rename: impl Fn(&CStr, &CStr) -> io::Result<()>,
) -> io::Result<bool> {
    let staged = tempfile::Builder::new()
        .prefix(".git-on-group-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(base)?;
    write_marker(staged.path())?;
    let from = CString::new(staged.path().as_os_str().as_encoded_bytes())?;
    let to = CString::new(path.as_os_str().as_encoded_bytes())?;
    match rename(&from, &to) {
        Ok(()) => {
            let _ = staged.keep();
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(true),
        Err(error)
            if error.raw_os_error() == Some(libc::ENOTSUP)
                || matches!(
                    error.kind(),
                    io::ErrorKind::Unsupported
                        | io::ErrorKind::InvalidInput
                        | io::ErrorKind::PermissionDenied
                ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn rename_group(from: &CStr, to: &CStr) -> io::Result<()> {
    // An ordinary rename can replace an existing empty directory.
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let uid = unsafe { libc::geteuid() };
        if self.metadata.uid() == uid
            && self.metadata.mode() & 0o1777 == 0o700
            && self.same_private_directory(uid)
            && fs::read_dir(&self.path).is_ok_and(|mut entries| {
                entries.all(|entry| entry.is_ok_and(|entry| entry.file_name() == OWNERSHIP))
            })
        {
            let removed = fs::remove_file(self.path.join(OWNERSHIP));
            if (removed.is_ok()
                || removed.is_err_and(|error| error.kind() == io::ErrorKind::NotFound))
                && self.same_private_directory(uid)
            {
                // A launch can add a run after read_dir. Keep the ownership
                // record in that case so the last finishing run can clean it.
                if fs::remove_dir(&self.path).is_err_and(|error| {
                    matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
                    )
                }) && self.same_private_directory(uid)
                {
                    let _ = write_marker(&self.path);
                }
            }
        }
    }
}

impl Group {
    fn same_private_directory(&self, uid: u32) -> bool {
        self.path.symlink_metadata().is_ok_and(|current| {
            current.is_dir()
                && current.uid() == uid
                && current.mode() & 0o1777 == 0o700
                && current.ino() == self.metadata.ino()
                && current.dev() == self.metadata.dev()
        })
    }
}

fn write_marker(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.join(OWNERSHIP))
        .map(drop)
}

fn has_marker(path: &Path, uid: u32) -> bool {
    path.join(OWNERSHIP).symlink_metadata().is_ok_and(|marker| {
        marker.is_file() && marker.uid() == uid && marker.mode() & 0o777 == 0o600
    })
}

fn private_name(uid: u32) -> String {
    format!(".go{uid:x}")
}

fn open_group(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

fn usable(group: &fs::Metadata, uid: u32) -> bool {
    let mode = group.mode();
    group.is_dir()
        && (group.uid() == uid && mode & 0o722 == 0o700
            || group.uid() == 0 && mode & 0o1777 == 0o1777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn occupied_group_and_unmarked_fallback_allow_a_private_run() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        let occupied = base.join(GROUP_NAME);
        fs::write(&occupied, b"user data").unwrap();
        let fallback = base.join(private_name(unsafe { libc::geteuid() }));
        fs::DirBuilder::new().mode(0o700).create(&fallback).unwrap();

        // A concurrent creator must leave an existing empty directory alone.
        publish_group(base, &fallback, rename_group).unwrap();
        let (group, run, _) = Group::create_in(base, rename_group).unwrap();
        assert!(group.is_none());
        assert_eq!(run.parent(), Some(base));
        assert!(run.is_dir());
        fs::remove_dir(run).unwrap();
        assert_eq!(fs::read_dir(&fallback).unwrap().count(), 0);
        assert_eq!(fs::read(&occupied).unwrap(), b"user data");
        assert_eq!(fs::read_dir(base).unwrap().count(), 2);
    }

    #[test]
    fn unsupported_group_rename_allows_a_standalone_run() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        for errno in [libc::ENOTSUP, libc::EPERM] {
            let (group, run, _) =
                Group::create_in(base, |_, _| Err(io::Error::from_raw_os_error(errno))).unwrap();
            assert!(group.is_none());
            assert_eq!(run.parent(), Some(base));
            assert_eq!(fs::symlink_metadata(&run).unwrap().mode() & 0o777, 0o700);
            fs::remove_dir(run).unwrap();
            assert_eq!(fs::read_dir(base).unwrap().count(), 0);
        }
    }
}
