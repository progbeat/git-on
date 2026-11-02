//! Managed backing registrations for local asynchronous sessions.
use crate::{BackingId, dev_fuse_async::AsyncDevFuse, ll::ioctl::fuse_dev_ioc_backing_close};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::{Arc, Weak};

/// Registers backing files on the live FUSE connection without owning it.
#[derive(Clone, Debug)]
pub struct AsyncBackingRegistrar {
    channel: Weak<AsyncDevFuse>,
}
impl AsyncBackingRegistrar {
    pub(crate) fn new(channel: &Arc<AsyncDevFuse>) -> Self {
        Self {
            channel: Arc::downgrade(channel),
        }
    }
    /// Register a file until the returned managed ID is dropped.
    pub fn open(&self, file: impl AsFd) -> std::io::Result<AsyncBackingId> {
        let channel = self
            .channel
            .upgrade()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENODEV))?;
        Ok(AsyncBackingId {
            channel: Arc::downgrade(&channel),
            id: BackingId::create_raw(channel.as_fd(), file)?,
        })
    }
}
/// A registration retained until all handles and pending replies release it.
#[derive(Debug)]
pub struct AsyncBackingId {
    channel: Weak<AsyncDevFuse>,
    pub(crate) id: u32,
}
impl Drop for AsyncBackingId {
    fn drop(&mut self) {
        if let Some(channel) = self.channel.upgrade() {
            if let Err(error) = unsafe { fuse_dev_ioc_backing_close(channel.as_raw_fd(), &self.id) }
            {
                log::error!("Failed to close async backing fd: {error}");
            }
        }
    }
}
