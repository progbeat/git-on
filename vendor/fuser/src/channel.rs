#[cfg(any(not(target_os = "macos"), feature = "abi-7-40"))]
use std::os::fd::BorrowedFd;
use std::{fs::File, io, os::unix::prelude::AsRawFd, sync::Arc};

use libc::{c_int, c_void, size_t};

#[cfg(feature = "abi-7-40")]
use crate::passthrough::BackingId;
use crate::reply::ReplySender;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug)]
pub struct Channel(Transport);

#[derive(Clone, Debug)]
enum Transport {
    Device(Arc<File>),
    #[cfg(target_os = "macos")]
    Native(Arc<crate::mnt::native::NativeChannel>),
}

#[cfg(not(target_os = "macos"))]
use std::os::fd::AsFd;
#[cfg(not(target_os = "macos"))]
impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.0 {
            Transport::Device(file) => file.as_fd(),
        }
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<File>) -> Self {
        Self(Transport::Device(device))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn from_native(channel: Arc<crate::mnt::native::NativeChannel>) -> Self {
        Self(Transport::Native(channel))
    }

    /// Receives data up to the capacity of the given buffer (can block).
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let file = match &self.0 {
            Transport::Device(file) => file,
            #[cfg(target_os = "macos")]
            Transport::Native(channel) => return channel.receive(buffer),
        };
        let rc = unsafe {
            libc::read(
                file.as_raw_fd(),
                buffer.as_ptr() as *mut c_void,
                buffer.len() as size_t,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc as usize)
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone())
    }
}

#[derive(Clone, Debug)]
pub struct ChannelSender(Transport);

impl ReplySender for ChannelSender {
    fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let file = match &self.0 {
            Transport::Device(file) => file,
            #[cfg(target_os = "macos")]
            Transport::Native(channel) => return channel.send(bufs),
        };
        let rc = unsafe {
            libc::writev(
                file.as_raw_fd(),
                bufs.as_ptr() as *const libc::iovec,
                bufs.len() as c_int,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc as usize);
            Ok(())
        }
    }

    #[cfg(feature = "abi-7-40")]
    fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        match &self.0 {
            Transport::Device(file) => BackingId::create(file, fd),
            #[cfg(target_os = "macos")]
            Transport::Native(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Linux passthrough is unavailable on FSKit",
            )),
        }
    }
}
