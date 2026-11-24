use crate::platform::{make_temporary_directory, open_child_directory, DirectoryEntries};
use std::{
    ffi::{CStr, CString, OsStr},
    fs::{File, Metadata},
    io,
    os::{
        fd::{AsRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

/// The directory and the parent entry from which it may be removed.
pub struct OwnedDirectory {
    path: PathBuf,
    name: CString,
    parent: File,
    pub(super) directory: File,
    pub(super) remove_on_drop: bool,
}

impl OwnedDirectory {
    pub(super) fn create(parent_path: &Path, prefix: &str) -> io::Result<Self> {
        Self::create_at(parent_path, File::open(parent_path)?, prefix)
    }

    pub(super) fn child(&self, prefix: &str) -> io::Result<Self> {
        if !current_entry(
            self.parent.as_raw_fd(),
            &self.name,
            &self.directory.metadata()?,
        )? {
            return Err(io::ErrorKind::NotFound.into());
        }
        Self::create_at(&self.path, self.directory.try_clone()?, prefix)
    }

    fn create_at(parent_path: &Path, parent: File, prefix: &str) -> io::Result<Self> {
        loop {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            let name = CString::new(format!("{prefix}{}", &suffix[..10]))?;
            match make_temporary_directory(&parent, &name) {
                Ok(()) => {
                    let path = parent_path.join(OsStr::from_bytes(name.to_bytes()));
                    let mut directory = Self::open_at(path, name, parent)?;
                    directory.remove_on_drop = true;
                    return Ok(directory);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let parent = File::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let name = CString::new(
            path.file_name()
                .ok_or(io::ErrorKind::InvalidInput)?
                .as_bytes(),
        )?;
        Self::open_at(path.to_owned(), name, parent)
    }

    fn open_at(path: PathBuf, name: CString, parent: File) -> io::Result<Self> {
        let directory = open_child_directory(parent.as_raw_fd(), &name, libc::O_RDONLY)?;
        Ok(Self {
            path,
            name,
            parent,
            directory,
            remove_on_drop: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Return false when a file on the workspace filesystem is still busy.
    pub fn remove(&mut self) -> io::Result<bool> {
        match remove_tree(self.parent.as_raw_fd(), &self.name, &self.directory) {
            Ok(()) => {
                self.remove_on_drop = false;
                Ok(true)
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::EBUSY | libc::ENOTEMPTY)) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = self.remove();
        }
    }
}

fn current_entry(parent: RawFd, name: &CStr, expected: &Metadata) -> io::Result<bool> {
    let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            entry.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let entry = unsafe { entry.assume_init() };
    #[cfg(target_os = "linux")]
    let device = entry.st_dev;
    #[cfg(target_os = "macos")]
    let device = entry.st_dev as u64;
    if entry.st_mode & libc::S_IFMT != libc::S_IFDIR
        || (device, entry.st_ino) != (expected.dev(), expected.ino())
    {
        return Err(io::Error::other("service storage changed before cleanup"));
    }
    Ok(true)
}

fn unlink(parent: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

fn remove_tree(parent: RawFd, name: &CStr, directory: &File) -> io::Result<()> {
    let identity = directory.metadata()?;
    if !current_entry(parent, name, &identity)? {
        return Ok(());
    }
    let mut entries = DirectoryEntries::open(directory)?;
    let fd = entries.as_raw_fd();
    while let Some((name, kind)) = entries.next_entry()? {
        if matches!(kind, libc::DT_DIR | libc::DT_UNKNOWN) {
            match open_child_directory(fd, name, libc::O_RDONLY) {
                Ok(child) => {
                    remove_tree(fd, name, &child)?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
                }
                Err(error) => return Err(error),
            }
        }
        unlink(fd, name, 0)?;
    }
    if current_entry(parent, name, &identity)? {
        unlink(parent, name, libc::AT_REMOVEDIR)?;
    }
    Ok(())
}
