//! File operations for the local loop. Without io_uring, one lazy bounded
//! worker owns blocking calls; payload and namespace caches stay on the loop.
use std::{
    cell::RefCell,
    fs, io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

type Job = Box<dyn FnOnce() + Send>;
struct Worker {
    sender: Option<tokio::sync::mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    exited: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Worker {
    fn new() -> io::Result<Self> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Job>(16);
        let (done, exited) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("git-on-file-io".into())
            .spawn(move || {
                while let Some(job) = receiver.blocking_recv() {
                    job();
                }
                let _ = done.send(());
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            exited: Some(exited),
        })
    }
    async fn finish(mut self) -> io::Result<()> {
        self.sender.take();
        let _ = self.exited.take().unwrap().await;
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("file I/O worker panicked"))
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct Io {
    native: bool,
    worker: RefCell<Option<Worker>>,
}
impl Io {
    pub(crate) fn new(native: bool) -> Rc<Self> {
        Rc::new(Self {
            native,
            worker: RefCell::new(None),
        })
    }
    #[cfg(test)]
    pub(crate) fn worker_started(&self) -> bool {
        self.worker.borrow().is_some()
    }
    pub(crate) async fn finish(&self) -> io::Result<()> {
        let worker = self.worker.borrow_mut().take();
        if let Some(worker) = worker {
            worker.finish().await?;
        }
        Ok(())
    }
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let sender = {
            let mut worker = self.worker.borrow_mut();
            if worker.is_none() {
                *worker = Some(Worker::new()?);
            }
            worker.as_ref().unwrap().sender.as_ref().unwrap().clone()
        };
        let (done, wait) = tokio::sync::oneshot::channel();
        sender
            .send(Box::new(move || {
                let _ = done.send(work());
            }))
            .await
            .map_err(|_| io::Error::other("file I/O worker stopped"))?;
        wait.await
            .map_err(|_| io::Error::other("file I/O worker stopped"))?
    }
    pub(crate) fn shared(self: &Rc<Self>, file: Arc<fs::File>) -> File {
        if self.native {
            File::Native(tokio_uring::fs::File::from_shared_std(file))
        } else {
            File::Fallback {
                file,
                io: self.clone(),
            }
        }
    }
    pub(crate) fn owned(self: &Rc<Self>, file: fs::File) -> File {
        if self.native {
            File::Native(tokio_uring::fs::File::from_std(file))
        } else {
            self.shared(Arc::new(file))
        }
    }
    pub(crate) async fn open_backing(
        &self,
        path: &Path,
        create: bool,
    ) -> io::Result<Arc<fs::File>> {
        if self.native {
            let file = tokio_uring::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(create)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .await?;
            file.try_into_std()
                .map(Arc::new)
                .map_err(|_| io::Error::other("native open still shared"))
        } else {
            let path = path.to_owned();
            self.blocking(move || {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(create)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)
                    .map(Arc::new)
            })
            .await
        }
    }
    pub(crate) async fn open_directory(self: &Rc<Self>, path: PathBuf) -> io::Result<File> {
        if self.native {
            tokio_uring::fs::File::open(path).await.map(File::Native)
        } else {
            let file = self.blocking(move || fs::File::open(path)).await?;
            Ok(self.owned(file))
        }
    }
    pub(crate) async fn open_raw(
        self: &Rc<Self>,
        directory: &File,
        name: &str,
        create: bool,
    ) -> io::Result<File> {
        if self.native {
            let File::Native(directory) = directory else {
                unreachable!("mixed file runtimes")
            };
            tokio_uring::fs::OpenOptions::new()
                .read(true)
                .write(create)
                .create_new(create)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open_at(directory, name)
                .await
                .map(File::Native)
        } else {
            let File::Fallback {
                file: directory, ..
            } = directory
            else {
                unreachable!("mixed file runtimes")
            };
            let directory = directory.clone();
            let name = std::ffi::CString::new(name)?;
            let file = self
                .blocking(move || {
                    use std::os::fd::FromRawFd;
                    let flags = libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK
                        | if create {
                            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
                        } else {
                            libc::O_RDONLY
                        };
                    let fd =
                        unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
                    if fd < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(unsafe { fs::File::from_raw_fd(fd) })
                    }
                })
                .await?;
            Ok(self.owned(file))
        }
    }
    pub(crate) async fn remove_file(&self, path: PathBuf) -> io::Result<()> {
        if self.native {
            tokio_uring::fs::remove_file(path).await
        } else {
            self.blocking(move || fs::remove_file(path)).await
        }
    }
}

pub(crate) enum File {
    Native(tokio_uring::fs::File),
    Fallback { file: Arc<fs::File>, io: Rc<Io> },
}
impl AsRawFd for File {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Self::Native(file) => file.as_raw_fd(),
            Self::Fallback { file, .. } => file.as_raw_fd(),
        }
    }
}
pub(crate) enum WriteBytes {
    Owned(Vec<u8>),
    Shared(crate::raw_blob_async::Bytes),
}
impl From<Vec<u8>> for WriteBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Owned(bytes)
    }
}
impl From<crate::raw_blob_async::Bytes> for WriteBytes {
    fn from(bytes: crate::raw_blob_async::Bytes) -> Self {
        Self::Shared(bytes)
    }
}
impl File {
    /// The raw-store anchor is a read-only directory, with no buffered writes.
    /// Closing an idle anchor must not start a worker or require a ring context.
    pub(crate) async fn close_directory_anchor(self) -> io::Result<()> {
        match self {
            Self::Native(file) => match file.try_into_std() {
                Ok(file) => {
                    drop(file);
                    Ok(())
                }
                // An abandoned openat may still retain the anchor until CQE.
                Err(file) => file.close().await,
            },
            Self::Fallback { file, .. } => {
                drop(file);
                Ok(())
            }
        }
    }
    pub(crate) async fn close(self) -> io::Result<()> {
        match self {
            Self::Native(file) => file.close().await,
            Self::Fallback { file, io } => {
                io.blocking(move || {
                    drop(file);
                    Ok(())
                })
                .await
            }
        }
    }
    pub(crate) async fn statx(&self) -> io::Result<libc::statx> {
        match self {
            Self::Native(file) => file.statx().await,
            Self::Fallback { file, io } => {
                let file = file.clone();
                io.blocking(move || {
                    let m = file.metadata()?;
                    // Use the portable metadata syscall path on older kernels;
                    // only these basic fields are consumed by the overlay.
                    let mut out: libc::statx = unsafe { std::mem::zeroed() };
                    out.stx_mask = libc::STATX_BASIC_STATS;
                    out.stx_mode = m.mode() as u16;
                    out.stx_nlink = m
                        .nlink()
                        .try_into()
                        .map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
                    out.stx_uid = m.uid();
                    out.stx_gid = m.gid();
                    out.stx_ino = m.ino();
                    out.stx_size = m.size();
                    out.stx_blocks = m.blocks();
                    out.stx_blksize = m.blksize() as u32;
                    out.stx_atime.tv_sec = m.atime();
                    out.stx_atime.tv_nsec = m.atime_nsec() as u32;
                    out.stx_mtime.tv_sec = m.mtime();
                    out.stx_mtime.tv_nsec = m.mtime_nsec() as u32;
                    out.stx_ctime.tv_sec = m.ctime();
                    out.stx_ctime.tv_nsec = m.ctime_nsec() as u32;
                    Ok(out)
                })
                .await
            }
        }
    }
    pub(crate) async fn sync_all(&self) -> io::Result<()> {
        self.sync(false).await
    }
    pub(crate) async fn sync_data(&self) -> io::Result<()> {
        self.sync(true).await
    }
    async fn sync(&self, data: bool) -> io::Result<()> {
        match self {
            Self::Native(file) => {
                if data {
                    file.sync_data().await
                } else {
                    file.sync_all().await
                }
            }
            Self::Fallback { file, io } => {
                let file = file.clone();
                io.blocking(move || {
                    if data {
                        file.sync_data()
                    } else {
                        file.sync_all()
                    }
                })
                .await
            }
        }
    }
    pub(crate) async fn write_all_at(
        &self,
        bytes: impl Into<WriteBytes>,
        offset: u64,
    ) -> io::Result<()> {
        let bytes = bytes.into();
        match self {
            Self::Native(file) => match bytes {
                WriteBytes::Owned(bytes) => file.write_all_at(bytes, offset).await.0,
                WriteBytes::Shared(bytes) => file.write_all_at(bytes, offset).await.0,
            },
            Self::Fallback { file, io } => {
                let bytes = match bytes {
                    WriteBytes::Owned(bytes) => bytes,
                    WriteBytes::Shared(bytes) => bytes.as_slice().to_vec(),
                };
                let file = file.clone();
                io.blocking(move || file.write_all_at(&bytes, offset)).await
            }
        }
    }
    pub(crate) async fn read_exact_at(
        &self,
        data: Vec<u8>,
        offset: u64,
    ) -> (io::Result<()>, Vec<u8>) {
        match self {
            Self::Native(file) => file.read_exact_at(data, offset).await,
            Self::Fallback { file, io } => {
                let file = file.clone();
                io.blocking(move || {
                    let mut data = data;
                    let result = file.read_exact_at(&mut data, offset);
                    Ok((result, data))
                })
                .await
                .unwrap_or_else(|error| (Err(error), Vec::new()))
            }
        }
    }
    pub(crate) async fn read_up_to(&self, offset: u64, count: usize) -> io::Result<Vec<u8>> {
        match self {
            Self::Native(file) => {
                use tokio_uring::buf::BoundedBuf;
                let mut data = Vec::with_capacity(count);
                while data.len() < count {
                    let done = data.len();
                    let (result, buf) = file
                        .read_at(data.slice(done..count), offset + done as u64)
                        .await;
                    data = buf.into_inner();
                    match result {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(data)
            }
            Self::Fallback { file, io } => {
                let file = file.clone();
                io.blocking(move || {
                    let mut data = vec![0; count];
                    let mut done = 0;
                    while done < count {
                        match file.read_at(&mut data[done..], offset + done as u64) {
                            Ok(0) => break,
                            Ok(n) => done += n,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    data.truncate(done);
                    Ok(data)
                })
                .await
            }
        }
    }
    pub(crate) async fn write_partial(&self, offset: u64, mut data: Vec<u8>) -> io::Result<usize> {
        match self {
            Self::Native(file) => {
                use tokio_uring::buf::BoundedBuf;
                let mut done = 0;
                while done < data.len() {
                    let (result, buf) = file
                        .write_at(data.slice(done..), offset + done as u64)
                        .submit()
                        .await;
                    data = buf.into_inner();
                    match result {
                        Ok(0) if done == 0 => return Err(io::Error::from_raw_os_error(libc::EIO)),
                        Ok(0) => break,
                        Ok(n) => done += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if done == 0 => return Err(e),
                        Err(_) => break,
                    }
                }
                Ok(done)
            }
            Self::Fallback { file, io } => {
                let file = file.clone();
                io.blocking(move || {
                    let mut done = 0;
                    while done < data.len() {
                        match file.write_at(&data[done..], offset + done as u64) {
                            Ok(0) if done == 0 => {
                                return Err(io::Error::from_raw_os_error(libc::EIO))
                            }
                            Ok(0) => break,
                            Ok(n) => done += n,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) if done == 0 => return Err(e),
                            Err(_) => break,
                        }
                    }
                    Ok(done)
                })
                .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{poll_fn, Future},
        task::Poll,
        time::Duration,
    };

    #[test]
    fn cancelled_fallback_jobs_keep_owned_fds_until_worker_drain() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owned");
        fs::write(&path, b"original bytes").unwrap();
        let file = Arc::new(fs::File::open(&path).unwrap());
        let weak = Arc::downgrade(&file);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let file = io.shared(file);
                let (entered, waiting) = tokio::sync::oneshot::channel();
                let (resume, paused) = std::sync::mpsc::channel();
                let mut blocker = Box::pin(io.blocking(move || {
                    entered.send(()).unwrap();
                    paused
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(io::Error::other)
                }));
                poll_fn(|cx| {
                    assert!(blocker.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                waiting.await.unwrap();
                let mut read = Box::pin(file.read_up_to(0, 100));
                poll_fn(|cx| {
                    assert!(read.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(read);
                drop(file);
                assert!(
                    weak.upgrade().is_some(),
                    "cancelled read released its queued descriptor"
                );
                let mut open = Box::pin(io.open_backing(&path, false));
                poll_fn(|cx| {
                    assert!(open.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(open);
                resume.send(()).unwrap();
                blocker.await.unwrap();
                io.finish().await.unwrap();
                assert!(
                    weak.upgrade().is_none(),
                    "cancelled job retained its descriptor after drain"
                );
                assert!(!io.worker_started());
            });
        let still_open = fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == path))
            .count();
        assert_eq!(
            still_open, 0,
            "cancelled fallback open leaked its returned descriptor"
        );
    }
}
