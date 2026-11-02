//! Local native I/O ownership. Active handles and requests share the original
//! standard descriptor; idle/closed inodes do not accumulate a retained FD.
use std::{
    cell::RefCell,
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
};
use tokio_uring::fs::File;

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
        // Closing the queue follows all submitted work. Await its exit before
        // joining so a slow filesystem cannot block the event loop on shutdown.
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

type Files = HashMap<u64, (usize, Weak<File>)>;
type Gates = HashMap<u64, Weak<tokio::sync::Mutex<()>>>;
#[derive(Default)]
pub(crate) struct Backing {
    files: RefCell<Files>,
    handles: RefCell<HashMap<u64, Rc<File>>>,
    gates: RefCell<Gates>,
    // Only operations unavailable through the supported io_uring ABI use this
    // one lazily-created, bounded actor. RAM hits never enter its queue.
    worker: RefCell<Option<Worker>>,
}
pub(crate) struct Lease<'a> {
    owner: &'a Backing,
    ino: u64,
    file: Option<Rc<File>>,
}
impl Lease<'_> {
    pub(crate) fn file(&self) -> &File {
        self.file.as_ref().unwrap()
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let ptr = Rc::as_ptr(self.file.as_ref().unwrap());
        self.file.take();
        let mut files = self.owner.files.borrow_mut();
        if files
            .get(&self.ino)
            .is_some_and(|(_, weak)| weak.as_ptr() == ptr && weak.strong_count() == 0)
        {
            files.remove(&self.ino);
        }
    }
}
pub(crate) struct Gate<'a> {
    owner: &'a Backing,
    ino: u64,
    pub(crate) mutex: Rc<tokio::sync::Mutex<()>>,
}
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        if Rc::strong_count(&self.mutex) == 1 {
            let mut gates = self.owner.gates.borrow_mut();
            if gates
                .get(&self.ino)
                .is_some_and(|weak| weak.as_ptr() == Rc::as_ptr(&self.mutex))
            {
                gates.remove(&self.ino);
            }
        }
    }
}
impl Backing {
    #[cfg(test)]
    pub(crate) fn retained(&self) -> (usize, usize, usize) {
        (
            self.files.borrow().len(),
            self.handles.borrow().len(),
            self.gates.borrow().len(),
        )
    }
    pub(crate) async fn finish(&mut self) -> io::Result<()> {
        self.handles.get_mut().clear();
        self.files.get_mut().clear();
        self.gates.get_mut().clear();
        if let Some(worker) = self.worker.get_mut().take() {
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
    pub(crate) async fn open(path: &Path, create: bool) -> io::Result<Arc<fs::File>> {
        use std::os::unix::fs::OpenOptionsExt;
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
    }
    pub(crate) async fn truncate(&self, file: Arc<fs::File>, size: u64) -> io::Result<()> {
        self.blocking(move || file.set_len(size)).await
    }
    pub(crate) async fn set_times(
        &self,
        file: Arc<fs::File>,
        times: fs::FileTimes,
    ) -> io::Result<()> {
        self.blocking(move || file.set_times(times)).await
    }
    pub(crate) async fn statistics(&self, path: PathBuf) -> io::Result<crate::backend::Statistics> {
        self.blocking(move || {
            crate::filesystem::Overlay::statistics_at(&path).map_err(io::Error::from_raw_os_error)
        })
        .await
    }
    pub(crate) fn lease(&self, ino: u64, file: Arc<fs::File>) -> Lease<'_> {
        let key = Arc::as_ptr(&file) as usize;
        let mut files = self.files.borrow_mut();
        let file = files
            .get(&ino)
            .filter(|(owner, _)| *owner == key)
            .and_then(|(_, weak)| weak.upgrade())
            .unwrap_or_else(|| {
                let file = Rc::new(File::from_shared_std(file));
                files.insert(ino, (key, Rc::downgrade(&file)));
                file
            });
        Lease {
            owner: self,
            ino,
            file: Some(file),
        }
    }
    pub(crate) fn opened(&self, ino: u64, fh: u64, file: Arc<fs::File>) {
        let lease = self.lease(ino, file);
        self.handles
            .borrow_mut()
            .insert(fh, lease.file.as_ref().unwrap().clone());
    }
    pub(crate) fn released(&self, ino: u64, fh: u64) {
        self.handles.borrow_mut().remove(&fh);
        let mut files = self.files.borrow_mut();
        if files
            .get(&ino)
            .is_some_and(|(_, weak)| weak.strong_count() == 0)
        {
            files.remove(&ino);
        }
    }
    pub(crate) fn gate(&self, ino: u64) -> Gate<'_> {
        let mut gates = self.gates.borrow_mut();
        let mutex = gates.get(&ino).and_then(Weak::upgrade).unwrap_or_else(|| {
            let mutex = Rc::new(tokio::sync::Mutex::new(()));
            gates.insert(ino, Rc::downgrade(&mutex));
            mutex
        });
        Gate {
            owner: self,
            ino,
            mutex,
        }
    }
    pub(crate) async fn read(file: &File, offset: u64, count: usize) -> io::Result<Vec<u8>> {
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
    /// A short write followed by ENOSPC/EFBIG reports the visible progress, just
    /// as the synchronous path does. The owned copy outlives cancellation.
    pub(crate) async fn write(file: &File, offset: u64, data: &[u8]) -> io::Result<usize> {
        use tokio_uring::buf::BoundedBuf;
        let mut data = data.to_vec();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn abandoned_native_opens_release_fds_before_and_after_completion() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("abandoned");
        fs::write(&path, b"contents").unwrap();
        let count = || {
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == path))
                .count()
        };
        for phase in ["submitted", "completed", "shutdown"] {
            tokio_uring::start(async {
                for _ in 0..16 {
                    let mut open = Box::pin(File::open(&path));
                    poll_fn(|cx| {
                        assert!(open.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    if phase == "completed" {
                        // Drive the CQ before dropping the unconsumed result.
                        File::open(&path).await.unwrap().close().await.unwrap();
                    }
                    drop(open);
                }
                if phase != "shutdown" {
                    File::open(&path).await.unwrap().close().await.unwrap();
                }
            });
            assert_eq!(count(), 0, "phase={phase}");
        }
    }
    #[test]
    fn failed_native_runtime_initialization_releases_tokio_fds() {
        const CHILD: &str = "GIT_ON_TEST_FAILED_RUNTIME_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let count = || fs::read_dir("/proc/self/fd").unwrap().count();
            let before = count();
            for _ in 0..4 {
                assert!(tokio_uring::Runtime::new(tokio_uring::builder().entries(0)).is_err());
            }
            assert_eq!(count(), before);
        } else {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "backing_async::tests::failed_native_runtime_initialization_releases_tokio_fds",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    #[test]
    fn shared_native_owner_keeps_one_fd_and_closed_handles_do_not_accumulate() {
        tokio_uring::start(async {
            let file = Arc::new(tempfile::tempfile().unwrap());
            let fd = file.as_raw_fd();
            let native = File::from_shared_std(file.clone());
            assert_eq!(native.as_raw_fd(), fd);
            Backing::write(&native, 0, b"contents").await.unwrap();
            native.close().await.unwrap();
            assert_eq!(file.metadata().unwrap().len(), 8);
            let native = File::from_shared_std(file.clone());
            let mut request = Box::pin(native.read_at(Vec::with_capacity(8), 0));
            std::future::poll_fn(|cx| {
                use std::future::Future;
                assert!(request.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(request);
            drop(native);
            assert_eq!(
                Arc::strong_count(&file),
                2,
                "cancelled native operation must own the FD until CQE"
            );
            let probe = File::from_std(tempfile::tempfile().unwrap());
            probe.statx().await.unwrap();
            probe.close().await.unwrap();
            assert_eq!(Arc::strong_count(&file), 1);
            let backing = Backing::default();
            for ino in 1..129 {
                backing.opened(ino, ino, file.clone());
                let lease = backing.lease(ino, file.clone());
                assert_eq!(lease.file().as_raw_fd(), fd);
                backing.released(ino, ino);
                assert_eq!(
                    Backing::read(lease.file(), 0, 8).await.unwrap(),
                    b"contents"
                );
                drop(lease);
                assert!(backing.files.borrow().is_empty());
                let gate = backing.gate(ino);
                let lock = gate.mutex.lock().await;
                drop(lock);
                drop(gate);
                assert!(backing.gates.borrow().is_empty());
            }
            assert!(backing.handles.borrow().is_empty());
            drop(backing);
            assert_eq!(Arc::strong_count(&file), 1);
            let native = File::from_std(tempfile::tempfile().unwrap());
            let fd = native.as_raw_fd();
            let standard = native.try_into_std().expect("unique fd transfer");
            assert_eq!(standard.as_raw_fd(), fd);
            standard.metadata().unwrap();
        });
    }
}
