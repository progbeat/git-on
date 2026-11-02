use log::error;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use nix::errno::Errno;

#[cfg(feature = "async")]
use crate::dev_fuse_async::AsyncDevFuse;
#[cfg(target_os = "linux")]
use crate::dev_fuse_async::set_nonblocking;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

/// A raw async communication channel to the FUSE kernel driver. This uses [`AsyncDevFuse`] under the hood and
/// provides async APIs for receiving data.
#[cfg(feature = "async")]
#[derive(Debug, Clone)]
pub(crate) struct AsyncChannel(pub(crate) Arc<AsyncDevFuse>);

impl AsFd for AsyncChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsyncChannel {
    /// Create a new async communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<AsyncDevFuse>) -> Self {
        Self(device)
    }

    /// Receives data up to the capacity of the given buffer.
    async fn receive(&self, buffer: &mut [u8]) -> tokio::io::Result<usize> {
        loop {
            let mut guard = self
                .0
                .0
                .ready(tokio::io::Interest::READABLE | tokio::io::Interest::ERROR)
                .await?;
            match guard.try_io(|inner| {
                nix::unistd::read(inner.get_ref(), buffer)
                    .map_err(|e| tokio::io::Error::from_raw_os_error(e as i32))
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Receives data up to the capacity of the given buffer (can block),
    /// retrying on errors that are safe to retry (ENOENT, EINTR, EAGAIN).
    ///
    /// - ENOENT: Operation interrupted. According to FUSE, this is safe to retry.
    /// - EINTR: Interrupted system call, retry.
    /// - EAGAIN: Explicitly instructed to try again.
    pub(crate) async fn receive_retrying(&self, buffer: &mut [u8]) -> Result<usize, Errno> {
        loop {
            match self.receive(buffer).await {
                Ok(size) => return Ok(size),
                Err(e) => match nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(0)) {
                    Errno::ENOENT | Errno::EINTR | Errno::EAGAIN => continue,
                    errno => return Err(errno),
                },
            }
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> AsyncChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        AsyncChannelSender(self.0.clone())
    }

    /// Clone the FUSE device fd using FUSE_DEV_IOC_CLONE ioctl.
    ///
    /// This creates a new fd that can read FUSE requests independently,
    /// enabling true parallel request processing. The kernel distributes
    /// requests across all cloned fds.
    ///
    /// Requires Linux 4.5+. Returns an error on older kernels or non-Linux.
    #[cfg(target_os = "linux")]
    pub(crate) async fn clone_fd(&self) -> tokio::io::Result<AsyncChannel> {
        // FUSE_DEV_IOC_CLONE requires a fresh /dev/fuse fd as the target.
        let new_dev = AsyncDevFuse::open().await?;
        let mut source_fd = self.0.0.as_raw_fd() as u32;

        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse,
        // source_fd is valid for the lifetime of this call.
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)
                .map_err(tokio::io::Error::from)?;
        }

        // Set the new fd to non-blocking mode, which is required for async operation
        set_nonblocking(new_dev.0.get_ref())
            .map_err(|e| tokio::io::Error::new(e.kind(), format!("set_nonblocking: {e}")))?;

        Ok(AsyncChannel::new(Arc::new(new_dev)))
    }
}

/// Object for sending data to an [`AsyncChannel`]. This can be safely cloned and sent to
/// other threads and is non-blocking.
#[cfg(feature = "async")]
#[derive(Clone, Debug)]
pub(crate) struct AsyncChannelSender(Arc<AsyncDevFuse>);

impl AsyncChannelSender {
    pub(crate) async fn send(&self, bufs: &[std::io::IoSlice<'_>]) -> tokio::io::Result<()> {
        loop {
            let mut guard = self
                .0
                .0
                .ready(tokio::io::Interest::WRITABLE | tokio::io::Interest::ERROR)
                .await?;

            match guard.try_io(|inner| {
                nix::sys::uio::writev(inner.get_ref(), bufs)
                    .inspect_err(|e| error!("Error writing to FUSE device: {e:?}"))
                    .map_err(tokio::io::Error::from)
            }) {
                Ok(rc) => {
                    let written = rc?;
                    if written != bufs.iter().map(|b| b.len()).sum::<usize>() {
                        return Err(tokio::io::Error::new(
                            tokio::io::ErrorKind::WriteZero,
                            "short FUSE reply",
                        ));
                    }
                    return Ok(());
                }
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{io::IoSlice, os::fd::OwnedFd, time::Duration};

    #[tokio::test(flavor = "current_thread")]
    async fn read_response_keeps_local_owner_while_reply_waits_for_writability() {
        use crate::ll::reply::Response;
        use std::{
            future::{Future, poll_fn},
            rc::Rc,
            task::Poll,
        };
        let (device, peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        device.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        while device.send(&[0; 1024]).is_ok() {}
        assert_eq!(
            device.send(b"full").unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let fd: OwnedFd = device.into();
        let channel = AsyncChannel::new(Arc::new(
            AsyncDevFuse::from_file(std::fs::File::from(fd)).unwrap(),
        ));
        let owner = Rc::new(());
        let response =
            crate::reply_async::ReadResponse::new(b"snapshot".to_vec()).with_owner(owner.clone());
        let mut send = Box::pin(async move {
            response
                .send_reply(&channel.sender(), crate::RequestId(1))
                .await
        });
        poll_fn(|cx| {
            assert!(send.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(Rc::strong_count(&owner), 2);
        let mut bytes = [0; 2048];
        while peer.recv(&mut bytes).is_ok() {}
        tokio::time::timeout(Duration::from_secs(2), send.as_mut())
            .await
            .unwrap()
            .unwrap();
        drop(send);
        assert_eq!(Rc::strong_count(&owner), 1);
        let size = peer.recv(&mut bytes).unwrap();
        assert_eq!(&bytes[size - 8..size], b"snapshot");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_reply_is_an_error_in_release_builds_too() {
        let (device, peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let fd: OwnedFd = device.into();
        let channel = AsyncChannel::new(Arc::new(
            AsyncDevFuse::from_file(std::fs::File::from(fd)).unwrap(),
        ));
        drop(peer);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            channel.sender().send(&[IoSlice::new(b"reply")]),
        )
        .await
        .expect("disconnected peer did not wake the reply transport");
        assert!(
            result.is_err(),
            "failed write was reported as a successful reply"
        );
    }
}
