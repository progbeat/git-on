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

#[cfg(test)]
std::thread_local! {
    pub(crate) static NEXT_METADATA_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
}

pub(crate) fn private_file_metadata(file: &File) -> io::Result<Metadata> {
    #[cfg(test)]
    if let Some(error) = NEXT_METADATA_ERROR.take() {
        return Err(io::Error::from_raw_os_error(error));
    }
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::other("invalid private file"));
    }
    Ok(metadata)
}

/// Open relative to the owned directory without following links or blocking on
/// special files. The caller must check the descriptor before using it.
pub(crate) fn open_private_descriptor(
    directory: &File,
    name: &CStr,
    flags: libc::c_int,
) -> io::Result<File> {
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

/// Return the checked metadata so callers can check length without another fstat.
pub(crate) fn open_private_file(
    directory: &File,
    name: &CStr,
    flags: libc::c_int,
) -> io::Result<(File, Metadata)> {
    let file = open_private_descriptor(directory, name, flags)?;
    let metadata = private_file_metadata(&file)?;
    Ok((file, metadata))
}
