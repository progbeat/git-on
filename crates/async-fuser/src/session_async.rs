//! Async filesystem session
//!
//! A session runs a filesystem implementation while it is being mounted to a specific mount
//! point. A session begins by mounting the filesystem and ends by unmounting it. While the
//! filesystem is mounted, the session loop receives, dispatches and replies to kernel requests
//! for filesystem operations under its mount point.

use std::io;
use std::path::Path;
use std::sync::Arc;

use log::error;
use nix::unistd::Uid;
use nix::unistd::geteuid;

use crate::Config;
use crate::KernelConfig;
use crate::MountOption;
use crate::Request;
use crate::SessionACL;
use crate::channel_async::AsyncChannel;
use crate::lib_async::AsyncFilesystem;
use crate::ll;
use crate::ll::AnyRequest;
use crate::ll::Version;
use crate::ll::fuse_abi as abi;
use crate::ll::reply::Response;
use crate::ll::request_async::AsyncRequestWithSender;
use crate::mnt::AsyncMount;
use crate::mnt::mount_options::check_option_conflicts;
use crate::read_buf::FuseReadBuf;
use crate::session::MAX_WRITE_SIZE;

/// Calls `destroy` on drop.
#[derive(Debug)]
pub(crate) struct AsyncSessionGuard<FS: AsyncFilesystem> {
    pub(crate) fs: Option<FS>,
}

/// Calls `destroy` on drop to do any user-specific cleanup.
impl<FS: AsyncFilesystem> AsyncSessionGuard<FS> {
    fn destroy(&mut self) {
        if let Some(mut fs) = self.fs.take() {
            fs.destroy();
        }
    }
}

/// Calls `destroy` on drop to do any user-specific cleanup.
impl<FS: AsyncFilesystem> Drop for AsyncSessionGuard<FS> {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Builder for [`AsyncSession`]. This is used to construct an instance of [`AsyncSession`]
/// within an asynchronous context.
#[derive(Default, Debug)]
pub struct AsyncSessionBuilder<FS: AsyncFilesystem> {
    filesystem: Option<FS>,
    mountpoint: Option<std::path::PathBuf>,
    options: Option<Config>,
}

impl<FS: AsyncFilesystem> AsyncSessionBuilder<FS> {
    /// Create a new builder for [`AsyncSession`].
    pub fn new() -> Self {
        Self {
            filesystem: None,
            mountpoint: None,
            options: None,
        }
    }

    /// Set the filesystem implementation for this session. This is required.
    pub fn filesystem(mut self, fs: FS) -> Self {
        self.filesystem = Some(fs);
        self
    }

    /// Set the mountpoint for this session. This is required.
    pub fn mountpoint(mut self, mountpoint: impl AsRef<Path>) -> Self {
        self.mountpoint = Some(mountpoint.as_ref().to_owned());
        self
    }

    /// Set the options for this session. This is required.
    pub fn options(mut self, options: Config) -> io::Result<Self> {
        check_option_conflicts(&options)?;
        if options.n_threads.unwrap_or(1) != 1 || options.clone_fd {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local sessions require one channel",
            ));
        }

        // validate permissions options
        if options.mount_options.contains(&MountOption::AutoUnmount)
            && options.acl == SessionACL::Owner
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "auto_unmount requires acl != Owner".to_string(),
            ));
        }

        self.options = Some(options);
        Ok(self)
    }

    /// Build the session. This will mount the filesystem and return an `AsyncSession` if successful.
    pub async fn build(self) -> io::Result<AsyncSession<FS>> {
        let filesystem = self.filesystem.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "`filesystem` is required")
        })?;
        let mountpoint = self.mountpoint.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "`mountpoint` is required")
        })?;
        let options = self
            .options
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "`options` are required"))?;

        AsyncSession::init(filesystem, mountpoint, &options).await
    }
}

/// The async session data structure
#[derive(Debug)]
pub struct AsyncSession<FS: AsyncFilesystem> {
    /// Filesystem operation access and drop guard.
    pub(crate) guard: AsyncSessionGuard<FS>,
    /// Communication channel to the kernel driver
    pub(crate) ch: AsyncChannel,
    /// Whether to restrict access to owner, root + owner, or unrestricted
    /// Used to implement `allow_root` and `auto_unmount`
    pub(crate) allowed: SessionACL,
    /// User that launched the fuser process
    pub(crate) session_owner: Uid,
    /// FUSE protocol version, as reported by the kernel.
    /// The field is set to `Some` when the init message is received.
    pub(crate) proto_version: Option<Version>,
    /// Config options for this session, used for debugging and for
    /// feature gating in the future.
    pub(crate) config: Config,
    request_buffer_size: usize,
    mount: Option<AsyncMount>,
}

impl<FS: AsyncFilesystem> AsyncSession<FS> {
    /// Access the filesystem before handing the session to its event loop.
    pub fn filesystem_mut(&mut self) -> &mut FS {
        self.guard.fs.as_mut().expect("filesystem was destroyed")
    }
    /// Obtain a managed registrar for this connection's backing files.
    pub fn backing_registrar(&self) -> crate::AsyncBackingRegistrar {
        crate::AsyncBackingRegistrar::new(&self.ch.0)
    }

    /// Wrap an already mounted FUSE descriptor and negotiate its protocol.
    /// The caller owns the mount; this session owns the descriptor and requests.
    pub async fn from_fd(
        filesystem: FS,
        fd: std::os::fd::OwnedFd,
        acl: SessionACL,
        config: Config,
    ) -> io::Result<Self> {
        let device = crate::dev_fuse_async::AsyncDevFuse::from_file(std::fs::File::from(fd))?;
        let mut session = Self {
            guard: AsyncSessionGuard {
                fs: Some(filesystem),
            },
            ch: AsyncChannel::new(Arc::new(device)),
            allowed: acl,
            session_owner: geteuid(),
            proto_version: None,
            config,
            request_buffer_size: MAX_WRITE_SIZE + 4096,
            mount: None,
        };
        if let Err(error) = session.handshake().await {
            let _ = session.filesystem_mut().shutdown().await;
            return Err(error);
        }
        Ok(session)
    }

    /// Create a new session and mount the given async filesystem to the given mountpoint.
    ///
    /// # Errors
    /// Returns an error if the options are incorrect, or if the fuse device can't be mounted.
    async fn init<P: AsRef<Path>>(
        filesystem: FS,
        mountpoint: P,
        options: &Config,
    ) -> io::Result<Self> {
        let mountpoint = mountpoint.as_ref();

        // mount (async)
        let mut mount = AsyncMount::new();
        mount = mount
            .mount(mountpoint, &options.mount_options, options.acl)
            .await?;
        let file = mount.dev_fuse().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to get FUSE device file descriptor from mount",
            )
        })?;
        let ch = AsyncChannel::new(file.clone());

        let mut session = AsyncSession {
            guard: AsyncSessionGuard {
                fs: Some(filesystem),
            },
            ch,
            allowed: options.acl,
            session_owner: geteuid(),
            proto_version: None,
            config: options.clone(),
            request_buffer_size: MAX_WRITE_SIZE + 4096,
            mount: Some(mount),
        };

        if let Err(error) = session.handshake().await {
            let _ = session.filesystem_mut().shutdown().await;
            // Complete cleanup while the runtime still drives native/helper I/O.
            if let Some(mut mount) = session.mount.take() {
                if let Err(cleanup) = mount.unmount().await {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{error}; unmount: {cleanup}"),
                    ));
                }
            }
            return Err(error);
        }

        Ok(session)
    }

    /// Run the session async loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem.
    ///
    /// # Errors
    /// Returns any final error when the session comes to an end.
    pub async fn run(self) -> io::Result<()> {
        let AsyncSession {
            guard,
            ch,
            allowed,
            session_owner,
            proto_version: _,
            config,
            request_buffer_size,
            mut mount,
        } = self;
        let mut filesystem = std::rc::Rc::new(guard);

        let invalid_config = config.n_threads.unwrap_or(1) != 1 || config.clone_fd;
        let event_loop = AsyncSessionEventLoop {
            thread_name: "fuser-local".into(),
            filesystem: filesystem.clone(),
            ch,
            allowed,
            session_owner,
            request_buffer_size,
        };
        let reply = if invalid_config {
            Err(io::Error::other("local sessions require one channel"))
        } else {
            event_loop.event_loop().await
        };
        drop(event_loop);

        // Clean up the filesystem
        let Some(filesystem) = std::rc::Rc::get_mut(&mut filesystem) else {
            return Err(io::Error::other(
                "BUG: must have one refcount for filesystem",
            ));
        };
        let shutdown = filesystem
            .fs
            .as_mut()
            .expect("filesystem was destroyed")
            .shutdown()
            .await;
        filesystem.destroy();
        let cleanup = if let Some(mount) = &mut mount {
            mount.unmount().await
        } else {
            Ok(())
        };
        reply.and(shutdown).and(cleanup)
    }

    /// Perform the initial handshake with the kernel, which involves receiving the init message,
    /// replying with the kernel config, and setting the protocol version for this session. This must be
    /// called before any other communication with the kernel can be done.
    async fn handshake(&mut self) -> io::Result<()> {
        let mut handshake_buffer = FuseReadBuf::with_capacity(4096);
        let buf = handshake_buffer.as_mut();
        let sender = self.ch.sender();
        let Some(fs) = &mut self.guard.fs else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Filesystem was not available during handshake",
            ));
        };

        // Keep checking for an init message from the kernel until we get one with a supported version,
        // at which point we reply to finish the handshake and return
        loop {
            let size = match self.ch.receive_retrying(buf).await {
                Ok(size) => size,
                Err(nix::errno::Errno::ENODEV) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "FUSE device disconnected during handshake",
                    ));
                }
                Err(err) => return Err(err.into()),
            };
            let request = AnyRequest::try_from(&buf[..size])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            // Convert the handshake request from the kernel to a usable operation
            let init = match request.operation() {
                Ok(ll::Operation::Init(init)) => init,
                Ok(_) => {
                    error!("Received non-init FUSE operation before init: {}", request);
                    ll::ResponseErrno(ll::Errno::EIO)
                        .send_reply(&sender, request.unique())
                        .await
                        .map_err(|e| {
                            io::Error::new(e.kind(), format!("send handshake error reply: {e}"))
                        })?;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Received non-init FUSE operation during handshake",
                    ));
                }
                Err(_) => {
                    ll::ResponseErrno(ll::Errno::EIO)
                        .send_reply(&sender, request.unique())
                        .await
                        .map_err(|e| {
                            io::Error::new(e.kind(), format!("send handshake error reply: {e}"))
                        })?;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Failed to parse FUSE operation",
                    ));
                }
            };
            let v = init.version();

            // Validate version support
            if v.0 > abi::FUSE_KERNEL_VERSION {
                init.reply_version_only()
                    .send_reply(&sender, request.unique())
                    .await?;
                continue;
            }
            if v < Version(7, 6) {
                ll::ResponseErrno(ll::Errno::EPROTO)
                    .send_reply(&sender, request.unique())
                    .await
                    .map_err(|e| {
                        io::Error::new(e.kind(), format!("send handshake error reply: {e}"))
                    })?;
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Unsupported FUSE ABI version {v}"),
                ));
            }

            // Construct kernel config from the init message user init() implementation and reply with it to finish
            // the handshake
            let mut config = KernelConfig::new(init.capabilities(), init.max_readahead(), v);
            if let Err(error) = fs
                .init(Request::ref_cast(request.header()), &mut config)
                .await
            {
                let errno = ll::Errno::from_i32(error.raw_os_error().unwrap_or(0));
                ll::ResponseErrno(errno)
                    .send_reply(&sender, request.unique())
                    .await
                    .map_err(|e| {
                        io::Error::new(e.kind(), format!("send handshake error reply: {e}"))
                    })?;
                return Err(error);
            }
            self.proto_version = Some(v);
            self.request_buffer_size = config.max_write as usize + 4096;
            let response = init.reply(&config);
            response
                .send_reply(&sender, request.unique())
                .await
                .map_err(|e| io::Error::new(e.kind(), format!("send init reply: {e}")))?;
            return Ok(());
        }
    }
}

pub(crate) struct AsyncSessionEventLoop<FS: AsyncFilesystem> {
    /// Cache thread name for faster `debug!`.
    pub(crate) thread_name: String,
    pub(crate) filesystem: std::rc::Rc<AsyncSessionGuard<FS>>,
    pub(crate) ch: AsyncChannel,
    pub(crate) allowed: SessionACL,
    pub(crate) session_owner: Uid,
    request_buffer_size: usize,
}

impl<FS: AsyncFilesystem> Clone for AsyncSessionEventLoop<FS> {
    fn clone(&self) -> Self {
        Self {
            thread_name: self.thread_name.clone(),
            filesystem: self.filesystem.clone(),
            ch: self.ch.clone(),
            allowed: self.allowed,
            session_owner: self.session_owner,
            request_buffer_size: self.request_buffer_size,
        }
    }
}

// Active requests retain their receive buffers and native local futures until
// the reply finishes. Cache both allocations for later requests. The ordinary
// immediately-ready path reuses one slot; only suspended operations grow the
// pool. This bounds active work, not the size or number of filesystem objects.
const MAX_IN_FLIGHT: usize = 16;
struct Requests<F> {
    slots: Vec<std::pin::Pin<Box<Option<F>>>>,
    active: Vec<usize>,
    free: Vec<usize>,
}
impl<F: std::future::Future<Output = io::Result<FuseReadBuf>>> Requests<F> {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            active: Vec::new(),
            free: Vec::new(),
        }
    }
    fn insert(&mut self, future: F) {
        let index = if let Some(index) = self.free.pop() {
            self.slots[index].as_mut().set(Some(future));
            index
        } else {
            self.slots.push(Box::pin(Some(future)));
            self.slots.len() - 1
        };
        self.active.push(index);
    }
    fn poll_completed(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<FuseReadBuf>> {
        for position in 0..self.active.len() {
            let index = self.active[position];
            let result = self.slots[index].as_mut().as_pin_mut().unwrap().poll(cx);
            if let std::task::Poll::Ready(result) = result {
                // Drop a completed future in place before reusing its storage.
                self.slots[index].as_mut().set(None);
                self.active.swap_remove(position);
                self.free.push(index);
                return std::task::Poll::Ready(result);
            }
        }
        std::task::Poll::Pending
    }
}

impl<FS: AsyncFilesystem> AsyncSessionEventLoop<FS> {
    async fn dispatch_owned(
        &self,
        mut buffer: FuseReadBuf,
        size: usize,
    ) -> io::Result<FuseReadBuf> {
        // Parse once, directly from the owned receive buffer. No copied packet,
        // reparsed header or spawned worker is needed for a suspended callback.
        let request = AsyncRequestWithSender::new(self.ch.sender(), &buffer.as_mut()[..size])?;
        request.dispatch(self).await;
        Ok(buffer)
    }

    async fn event_loop(&self) -> io::Result<()> {
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };
        let mut requests = Requests::new();
        let mut buffers = Vec::new();
        let result = async {
            loop {
                // Poll existing work before taking another buffer. Immediately
                // ready callbacks therefore recycle without allocating again.
                poll_fn(|cx| {
                    while let Poll::Ready(result) = requests.poll_completed(cx) {
                        buffers.push(result?);
                    }
                    if requests.active.len() < MAX_IN_FLIGHT {
                        Poll::Ready(Ok::<_, io::Error>(()))
                    } else {
                        Poll::Pending
                    }
                })
                .await?;
                let mut buffer = buffers
                    .pop()
                    .unwrap_or_else(|| FuseReadBuf::with_capacity(self.request_buffer_size));
                let size = {
                    let receive = self.ch.receive_retrying(buffer.as_mut());
                    let mut receive = std::pin::pin!(receive);
                    poll_fn(|cx| {
                        while let Poll::Ready(result) = requests.poll_completed(cx) {
                            buffers.push(result?);
                        }
                        receive
                            .as_mut()
                            .poll(cx)
                            .map(|r| r.map_err(io::Error::from))
                    })
                    .await
                };
                let size = match size {
                    Ok(0) => break Ok(()),
                    Ok(size) => size,
                    Err(error) if error.raw_os_error() == Some(libc::ENODEV) => break Ok(()),
                    Err(error) => break Err(error),
                };
                // DESTROY is an ordering barrier. Drain all owned callbacks and
                // replies before acknowledging it, then release the filesystem.
                let bytes = &buffer.as_mut()[..size];
                if bytes.get(4..8) == Some(38u32.to_ne_bytes().as_slice()) {
                    while !requests.active.is_empty() {
                        let completed = poll_fn(|cx| requests.poll_completed(cx)).await?;
                        buffers.push(completed);
                    }
                    let request = AsyncRequestWithSender::new(self.ch.sender(), bytes)?;
                    if !matches!(request.request.operation(), Ok(ll::Operation::Destroy(_))) {
                        break Err(io::Error::other("invalid FUSE_DESTROY"));
                    }
                    request
                        .reply_empty()
                        .await
                        .map_err(|e| io::Error::other(format!("{e:?}")))?;
                    break Ok(());
                }
                requests.insert(self.dispatch_owned(buffer, size));
            }
        }
        .await;
        // EOF, unmount and malformed input also stop intake before destroy.
        // Pending request futures remain owned and are settled before FS drops.
        let mut drain_error = None;
        while !requests.active.is_empty() {
            if let Err(error) = poll_fn(|cx| requests.poll_completed(cx)).await {
                drain_error.get_or_insert(error);
            }
        }
        result.and(drain_error.map_or(Ok(()), Err))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    #[derive(Debug)]
    struct Empty;
    impl AsyncFilesystem for Empty {}

    #[test]
    fn builder_preserves_native_mountpoint_bytes() {
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/native-\xff directory".to_vec(),
        ));
        let builder = AsyncSessionBuilder::new()
            .filesystem(Empty)
            .mountpoint(&path);
        let represented = std::path::Path::new(builder.mountpoint.as_ref().unwrap());
        assert_eq!(
            represented.as_os_str().as_bytes(),
            path.as_os_str().as_bytes()
        );
    }
}
