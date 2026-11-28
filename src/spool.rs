//! Open run-local files relative to their owned directories.
use std::{
    ffi::CStr,
    fs::{File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
};

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_NEXT_FILE_METADATA: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static FAIL_NEXT_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn private_file_metadata(file: &File) -> io::Result<Metadata> {
    #[cfg(test)]
    if FAIL_NEXT_FILE_METADATA.replace(false) {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::other("invalid private file"));
    }
    Ok(metadata)
}

/// Open relative to the owned directory without following links or blocking on
/// special files. Return the checked metadata so callers can also check length
/// without another fstat.
pub(crate) fn open_private_file(
    directory: &File,
    name: &CStr,
    flags: libc::c_int,
) -> io::Result<(File, Metadata)> {
    let file = open_file_at(directory, name, flags)?;
    let metadata = private_file_metadata(&file)?;
    Ok((file, metadata))
}

pub(crate) fn open_file_at(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn remove_file_at(directory: &File, name: &CStr) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_UNLINK.replace(false) {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    loop {
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::NotFound => return Ok(()),
            io::ErrorKind::Interrupted => (),
            _ => return Err(error),
        }
    }
}
