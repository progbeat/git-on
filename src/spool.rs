//! Validated access to private backing files.
use std::{
    ffi::CStr,
    fs::{File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
};

pub(crate) fn private_file_metadata(file: &File) -> io::Result<Metadata> {
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
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = private_file_metadata(&file)?;
    Ok((file, metadata))
}
