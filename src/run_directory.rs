use std::{
    ffi::{CStr, CString},
    fs::{self, File, OpenOptions},
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

const GROUP_NAME: &str = "git-on";
const OWNERSHIP: &CStr = c".git-on-owner";

pub(super) struct Group {
    path: PathBuf,
    directory: File,
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
        Self::create_in(&base, rename_group, create_run)
    }

    fn create_in(
        base: &Path,
        rename: impl Fn(&CStr, &CStr) -> io::Result<()>,
        create: impl Fn(&File, &CStr) -> io::Result<()>,
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
                    || metadata.mode() & 0o1777 != 0o1777 && !has_marker(&directory, uid)
                {
                    break;
                }
                let id = uuid::Uuid::new_v4().to_string();
                let name = &id[..8];
                let entry = CString::new(name).expect("UUID is ASCII");
                let error = match create(&directory, &entry) {
                    Ok(()) => {
                        let run = path.join(name);
                        let group = Self {
                            path,
                            directory,
                            metadata,
                        };
                        return Ok((Some(group), run, id));
                    }
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => break,
                    Err(error) => error,
                };
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
        let directory = open_group(path).ok()?;
        let metadata = directory.metadata().ok()?;
        // Recovery may start while a retiring run has the marker removed.
        // Hold the directory through cleanup and check its marker on drop.
        private_directory(&metadata, uid).then(|| Self {
            path: path.to_owned(),
            directory,
            metadata,
        })
    }
}

fn create_run(directory: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
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
    write_marker(&open_group(staged.path())?)?;
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
        self.remove_if_empty(only_marker, |path| fs::remove_dir(path));
    }
}

impl Group {
    fn remove_if_empty(
        &self,
        mut inspect: impl FnMut(&Path) -> bool,
        mut remove: impl FnMut(&Path) -> io::Result<()>,
    ) {
        let uid = unsafe { libc::geteuid() };
        if !private_directory(&self.metadata, uid) {
            return;
        }
        while self.same_private_directory(uid)
            && inspect(&self.path)
            && has_marker(&self.directory, uid)
        {
            // Only the cleaner that removes this group's marker can retire
            // it. A replacement at the same path has a different descriptor.
            let removed =
                unsafe { libc::unlinkat(self.directory.as_raw_fd(), OWNERSHIP.as_ptr(), 0) };
            if removed != 0 || !self.same_private_directory(uid) {
                return;
            }
            let Err(error) = remove(&self.path) else {
                return;
            };
            if !self.same_private_directory(uid) || write_marker(&self.directory).is_err() {
                return;
            }
            // A concurrent run can finish while the marker is absent.
            // Check again after restoring it so that run can be the last.
            if !matches!(
                error.kind(),
                io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
            ) {
                return;
            }
        }
    }

    fn same_private_directory(&self, uid: u32) -> bool {
        self.path.symlink_metadata().is_ok_and(|current| {
            private_directory(&current, uid)
                && current.ino() == self.metadata.ino()
                && current.dev() == self.metadata.dev()
        })
    }
}

fn private_directory(metadata: &fs::Metadata, uid: u32) -> bool {
    metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o1777 == 0o700
}

fn only_marker(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| {
        entries.all(|entry| {
            entry.is_ok_and(|entry| entry.file_name().as_encoded_bytes() == OWNERSHIP.to_bytes())
        })
    })
}

fn write_marker(directory: &File) -> io::Result<()> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            OWNERSHIP.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(unsafe { File::from_raw_fd(fd) });
    Ok(())
}

fn has_marker(directory: &File, uid: u32) -> bool {
    let mut marker = MaybeUninit::<libc::stat>::uninit();
    let status = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            OWNERSHIP.as_ptr(),
            marker.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        return false;
    }
    let marker = unsafe { marker.assume_init() };
    marker.st_uid == uid && marker.st_mode & (libc::S_IFMT | 0o777) == libc::S_IFREG | 0o600
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
    fn finishing_group_does_not_remove_the_replacement_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        let (first, first_run, _) = Group::create_in(base, rename_group, create_run).unwrap();
        let (mut second, second_run, _) = Group::create_in(base, rename_group, create_run).unwrap();
        let first = first.unwrap();
        let path = first.path.clone();
        fs::remove_dir(first_run).unwrap();
        fs::remove_dir(second_run).unwrap();
        let mut replacement = None;
        first.remove_if_empty(
            |group_path| {
                assert!(only_marker(group_path));
                drop(second.take());
                assert!(!group_path.exists());
                replacement = Some(Group::create_in(base, rename_group, create_run).unwrap());
                true
            },
            |group_path| fs::remove_dir(group_path),
        );
        let (group, run, _) = replacement.unwrap();
        assert_eq!(run.parent(), Some(path.as_path()));
        assert!(
            has_marker(&group.as_ref().unwrap().directory, unsafe {
                libc::geteuid()
            }),
            "replacement lost its ownership marker"
        );
        fs::remove_dir(run).unwrap();
        drop(group);
        assert!(!path.exists());
    }

    #[test]
    fn stale_cleanup_can_finish_on_either_side_of_marker_restoration() {
        for finish_before_restore in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let (group, run, _) =
                Group::create_in(temporary.path(), rename_group, create_run).unwrap();
            let group = group.unwrap();
            fs::remove_dir(run).unwrap();
            let concurrent_run = group.path.join("12345678");
            let mut started = false;
            let mut cleanup = None;
            group.remove_if_empty(
                |path| {
                    let empty = only_marker(path);
                    if !started {
                        fs::create_dir(&concurrent_run).unwrap();
                        started = true;
                    }
                    empty
                },
                |path| {
                    let result = fs::remove_dir(path);
                    if result.is_err() {
                        assert!(!has_marker(&group.directory, unsafe { libc::geteuid() }));
                        cleanup = Group::for_run(&concurrent_run);
                        if finish_before_restore {
                            fs::remove_dir(&concurrent_run).unwrap();
                            drop(cleanup.take());
                        }
                    }
                    result
                },
            );
            if !finish_before_restore {
                fs::remove_dir(&concurrent_run).unwrap();
                drop(cleanup);
            }
            assert!(
                !group.path.exists(),
                "empty group left after recovery, finish_before_restore={finish_before_restore}"
            );
        }
    }

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
        let (group, run, _) = Group::create_in(base, rename_group, create_run).unwrap();
        assert!(group.is_none());
        assert_eq!(run.parent(), Some(base));
        assert!(run.is_dir());
        fs::remove_dir(run).unwrap();
        let unmarked_run = fallback.join("12345678");
        fs::create_dir(&unmarked_run).unwrap();
        let recovery = Group::for_run(&unmarked_run);
        fs::remove_dir(unmarked_run).unwrap();
        drop(recovery);
        assert_eq!(fs::read_dir(&fallback).unwrap().count(), 0);
        assert_eq!(fs::read(&occupied).unwrap(), b"user data");
        assert_eq!(fs::read_dir(base).unwrap().count(), 2);
    }

    #[test]
    fn unsupported_group_rename_allows_a_standalone_run() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        for errno in [libc::ENOTSUP, libc::EPERM] {
            let (group, run, _) = Group::create_in(
                base,
                |_, _| Err(io::Error::from_raw_os_error(errno)),
                create_run,
            )
            .unwrap();
            assert!(group.is_none());
            assert_eq!(run.parent(), Some(base));
            assert_eq!(fs::symlink_metadata(&run).unwrap().mode() & 0o777, 0o700);
            fs::remove_dir(run).unwrap();
            assert_eq!(fs::read_dir(base).unwrap().count(), 0);
        }
    }

    #[test]
    fn denied_group_creation_uses_the_private_group() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        let primary = base.join(GROUP_NAME);
        let fallback = base.join(private_name(unsafe { libc::geteuid() }));
        for directory in [&primary, &fallback] {
            fs::DirBuilder::new().mode(0o700).create(directory).unwrap();
            write_marker(&open_group(directory).unwrap()).unwrap();
        }
        let primary_inode = fs::metadata(&primary).unwrap().ino();
        let (group, run, _) = Group::create_in(
            base,
            |_, _| unreachable!("groups already exist"),
            |directory, name| {
                if directory.metadata()?.ino() == primary_inode {
                    return Err(io::Error::from_raw_os_error(libc::EACCES));
                }
                create_run(directory, name)
            },
        )
        .unwrap();
        assert!(group.is_some());
        assert_eq!(run.parent(), Some(fallback.as_path()));
        assert!(run.is_dir());
    }
}
