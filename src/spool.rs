//! Invocation-only temporary files, split into bounded reopenable parts without
//! retaining a descriptor per part or imposing an aggregate storage ceiling.
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

pub(crate) struct Files {
    parts: Option<Arc<Directory>>,
}
pub(crate) struct Directory {
    fd: File,
    _owner: tempfile::TempDir,
    bytes: u64,
    next: AtomicU64,
}
impl Files {
    pub(crate) fn size_limit() -> io::Result<Option<u64>> {
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        if unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, limit.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let limit = unsafe { limit.assume_init() }.rlim_cur;
        Ok((limit != libc::RLIM_INFINITY).then_some(limit))
    }
    pub(crate) fn new(path: &Path) -> io::Result<Self> {
        // Part size is an implementation detail, never a total-storage limit.
        // Also avoid aggregating a huge run into the host's maximum file size.
        Self::with_limit(
            path,
            Some(Self::size_limit()?.unwrap_or(u64::MAX).min(1 << 30)),
        )
    }
    pub(crate) fn with_limit(path: &Path, limit: Option<u64>) -> io::Result<Self> {
        let parts = if let Some(bytes) = limit {
            if bytes == 0 {
                return Err(io::Error::from_raw_os_error(libc::EFBIG));
            }
            let owner = tempfile::Builder::new()
                .prefix("raw-parts-")
                .tempdir_in(path)?;
            let fd = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(owner.path())?;
            Some(Arc::new(Directory {
                fd,
                _owner: owner,
                bytes,
                next: AtomicU64::new(0),
            }))
        } else {
            None
        };
        Ok(Self { parts })
    }
    pub(crate) fn file(&self, path: &Path) -> io::Result<Spool> {
        Ok(if let Some(directory) = &self.parts {
            Spool::Parts {
                directory: directory.clone(),
                id: directory.next.fetch_add(1, Ordering::Relaxed),
                state: Mutex::new(Parts {
                    length: 0,
                    cursor: 0,
                    cached: None,
                }),
            }
        } else {
            Spool::Single(tempfile::tempfile_in(path)?)
        })
    }
}
pub(crate) enum Spool {
    Single(File),
    Parts {
        directory: Arc<Directory>,
        id: u64,
        state: Mutex<Parts>,
    },
}
pub(crate) struct Parts {
    length: u64,
    cursor: u64,
    // One descriptor per logical file, independently of bytes/part count.
    cached: Option<(u64, File)>,
}
fn part_name(id: u64, part: u64) -> CString {
    CString::new(format!("{id:x}-{part:x}")).expect("hexadecimal part name")
}
fn open_part<'a>(
    directory: &Directory,
    id: u64,
    state: &'a mut Parts,
    part: u64,
    create: bool,
) -> io::Result<Option<&'a File>> {
    if state
        .cached
        .as_ref()
        .is_none_or(|(index, _)| *index != part)
    {
        // Drop before opening the next part, keeping the descriptor bound even
        // when an unrelated operation has filled the process descriptor table.
        state.cached = None;
        let flags = libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        let mut created = false;
        let mut fd = unsafe {
            libc::openat(
                directory.fd.as_raw_fd(),
                part_name(id, part).as_ptr(),
                flags
                    | if create {
                        libc::O_CREAT | libc::O_EXCL
                    } else {
                        0
                    },
                0o600,
            )
        };
        if fd >= 0 && create {
            created = true;
        }
        if fd < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
            fd = unsafe {
                libc::openat(
                    directory.fd.as_raw_fd(),
                    part_name(id, part).as_ptr(),
                    flags,
                )
            };
        }
        if fd < 0 {
            let error = io::Error::last_os_error();
            if !create && error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error);
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::other("invalid raw spool part"));
        }
        if created {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        state.cached = Some((part, file));
    }
    Ok(state.cached.as_ref().map(|(_, file)| file))
}
impl Spool {
    pub(crate) fn len(&self) -> io::Result<u64> {
        match self {
            Self::Single(file) => Ok(file.metadata()?.len()),
            Self::Parts { state, .. } => Ok(state.lock().unwrap_or_else(|e| e.into_inner()).length),
        }
    }
    #[cfg(test)]
    pub(crate) fn set_len(&self, length: u64) -> io::Result<()> {
        match self {
            Self::Single(file) => file.set_len(length),
            Self::Parts {
                directory,
                id,
                state,
            } => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                let old = state.length;
                if length < old {
                    for part in length.div_ceil(directory.bytes)..old.div_ceil(directory.bytes) {
                        if state
                            .cached
                            .as_ref()
                            .is_some_and(|(index, _)| *index == part)
                        {
                            state.cached = None;
                        }
                        if unsafe {
                            libc::unlinkat(
                                directory.fd.as_raw_fd(),
                                part_name(*id, part).as_ptr(),
                                0,
                            )
                        } != 0
                        {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::NotFound {
                                return Err(error);
                            }
                        }
                    }
                    if !length.is_multiple_of(directory.bytes) {
                        if let Some(file) =
                            open_part(directory, *id, &mut state, length / directory.bytes, false)?
                        {
                            file.set_len(length % directory.bytes)?;
                        }
                    }
                }
                // Missing parts represent sparse zeroes, as in an ordinary file.
                state.length = length;
                Ok(())
            }
        }
    }
    pub(crate) fn read_at(&self, mut bytes: &mut [u8], mut offset: u64) -> io::Result<usize> {
        match self {
            Self::Single(file) => file.read_at(bytes, offset),
            Self::Parts {
                directory,
                id,
                state,
            } => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                let total = bytes.len().min(
                    state
                        .length
                        .saturating_sub(offset)
                        .try_into()
                        .unwrap_or(usize::MAX),
                );
                bytes = &mut bytes[..total];
                while !bytes.is_empty() {
                    let within = offset % directory.bytes;
                    let count = bytes
                        .len()
                        .min((directory.bytes - within).try_into().unwrap_or(usize::MAX));
                    if let Some(file) =
                        open_part(directory, *id, &mut state, offset / directory.bytes, false)?
                    {
                        let mut used = 0;
                        while used < count {
                            let n = file.read_at(&mut bytes[used..count], within + used as u64)?;
                            if n == 0 {
                                bytes[used..count].fill(0);
                                break;
                            }
                            used += n;
                        }
                    } else {
                        bytes[..count].fill(0)
                    }
                    bytes = &mut bytes[count..];
                    offset += count as u64;
                }
                Ok(total)
            }
        }
    }
    pub(crate) fn read_exact_at(&self, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.read_at(bytes, offset)?;
            if count == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            bytes = &mut bytes[count..];
            offset += count as u64;
        }
        Ok(())
    }
    pub(crate) fn write_all_at(&self, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
        match self {
            Self::Single(file) => file.write_all_at(bytes, offset),
            Self::Parts {
                directory,
                id,
                state,
            } => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))?;
                while !bytes.is_empty() {
                    let within = offset % directory.bytes;
                    let count = bytes
                        .len()
                        .min((directory.bytes - within).try_into().unwrap_or(usize::MAX));
                    open_part(directory, *id, &mut state, offset / directory.bytes, true)?
                        .unwrap()
                        .write_all_at(&bytes[..count], within)?;
                    offset += count as u64;
                    state.length = state.length.max(offset);
                    bytes = &bytes[count..];
                }
                Ok(())
            }
        }
    }
}
impl Seek for &Spool {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Spool::Single(file) => (&*file).seek(position),
            Spool::Parts { state, .. } => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                let value = match position {
                    SeekFrom::Start(n) => i128::from(n),
                    SeekFrom::Current(n) => i128::from(state.cursor) + i128::from(n),
                    SeekFrom::End(n) => i128::from(state.length) + i128::from(n),
                };
                state.cursor = value
                    .try_into()
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
                Ok(state.cursor)
            }
        }
    }
}
impl Write for &Spool {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Spool::Single(file) => (&*file).write(bytes),
            Spool::Parts { state, .. } => {
                let offset = state.lock().unwrap_or_else(|e| e.into_inner()).cursor;
                self.write_all_at(bytes, offset)?;
                state.lock().unwrap_or_else(|e| e.into_inner()).cursor += bytes.len() as u64;
                Ok(bytes.len())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        if let Self::Parts {
            directory,
            id,
            state,
        } = self
        {
            let state = state.get_mut().unwrap_or_else(|e| e.into_inner());
            state.cached = None;
            for part in 0..state.length.div_ceil(directory.bytes) {
                unsafe {
                    libc::unlinkat(directory.fd.as_raw_fd(), part_name(*id, part).as_ptr(), 0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn part_reopen_rejects_links_and_uses_the_owned_directory_identity() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::with_limit(root.path(), Some(17)).unwrap();
        let file = files.file(root.path()).unwrap();
        file.write_all_at(b"owned", 0).unwrap();
        let Spool::Parts {
            directory,
            id,
            state,
        } = &file
        else {
            panic!("expected parts")
        };
        let original = directory._owner.path().to_path_buf();
        let part = original.join(part_name(*id, 0).to_str().unwrap());
        let sentinel = root.path().join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        state.lock().unwrap().cached = None;
        std::fs::remove_file(&part).unwrap();
        std::fs::hard_link(&sentinel, &part).unwrap();
        assert!(file.write_all_at(b"bad", 0).is_err());
        std::fs::remove_file(&part).unwrap();
        std::os::unix::fs::symlink(&sentinel, &part).unwrap();
        assert!(file.write_all_at(b"bad", 0).is_err());
        std::fs::remove_file(&part).unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        file.write_all_at(b"next", 17).unwrap();
        assert!(moved.join(part_name(*id, 1).to_str().unwrap()).exists());
        assert!(std::fs::read_dir(&original).unwrap().next().is_none());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"unchanged");
        drop(file);
        drop(files);
        std::fs::remove_dir_all(moved).unwrap();
    }
    #[test]
    fn finite_parts_support_ranges_sparse_growth_truncation_and_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::with_limit(root.path(), Some(17)).unwrap();
        let file = files.file(root.path()).unwrap();
        let bytes: Vec<_> = (0..100).collect();
        (&file).write_all(&bytes).unwrap();
        let mut read = [0; 43];
        file.read_exact_at(&mut read, 14).unwrap();
        assert_eq!(read, bytes[14..57]);
        file.set_len(20).unwrap();
        file.set_len(100).unwrap();
        let mut read = [255; 100];
        file.read_exact_at(&mut read, 0).unwrap();
        assert_eq!(&read[..20], &bytes[..20]);
        assert_eq!(&read[20..], &[0; 80]);
        file.write_all_at(b"end", 98).unwrap();
        assert_eq!(file.len().unwrap(), 101);
        drop(file);
        drop(files);
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }
}
