use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
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
    pub(super) fn create() -> io::Result<(Self, PathBuf, String)> {
        let base = std::env::temp_dir();
        let base = fs::canonicalize(if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &base
        })?;
        let uid = unsafe { libc::geteuid() };
        for name in [GROUP_NAME.to_owned(), private_name(uid)] {
            let path = base.join(name);
            let mut retried_stale = false;
            let mut retried_marker = false;
            loop {
                let created = match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => true,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(error) => return Err(error),
                };
                let directory = match open_group(&path) {
                    Ok(directory) => directory,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
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
                if !usable(&metadata, uid) {
                    break;
                }
                if created {
                    match write_marker(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            let _ = fs::remove_dir(&path);
                            return Err(error);
                        }
                    }
                } else if metadata.mode() & 0o1777 != 0o1777 && !has_marker(&path, uid) {
                    if !retried_marker {
                        // A launch can be between mkdir and publishing ownership.
                        retried_marker = true;
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    break;
                }
                let group = Self {
                    path: path.clone(),
                    metadata,
                };
                let id = uuid::Uuid::new_v4().to_string();
                let name = &id[..8];
                let entry = CString::new(name).expect("UUID is ASCII");
                let fd = directory.as_raw_fd();
                if unsafe { libc::mkdirat(fd, entry.as_ptr(), 0o700) } == 0 {
                    return Ok((group, path.join(name), id));
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
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no usable private git-on group in the system temporary directory",
        ))
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
