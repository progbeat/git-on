//! Local file ownership. Active handles and requests share the original
//! standard descriptor; idle/closed inodes do not accumulate a retained FD.
use crate::file_async::{File, Io};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
};

type Files = HashMap<u64, (usize, Weak<File>)>;
type Gates = HashMap<u64, std::sync::Weak<tokio::sync::RwLock<()>>>;
pub(crate) struct Backing {
    files: RefCell<Files>,
    handles: RefCell<HashMap<u64, Rc<File>>>,
    gates: Rc<RefCell<Gates>>,
    pub(crate) io: Rc<Io>,
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
pub(crate) struct Gate {
    gates: Rc<RefCell<Gates>>,
    ino: u64,
    pub(crate) lock: Arc<tokio::sync::RwLock<()>>,
}
impl Drop for Gate {
    fn drop(&mut self) {
        if Arc::strong_count(&self.lock) == 1 {
            self.gates.borrow_mut().remove(&self.ino);
        }
    }
}
pub(crate) struct ReadToken {
    _gate: Gate,
    guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
}
impl Drop for ReadToken {
    fn drop(&mut self) {
        self.guard.take();
    }
}
impl Gate {
    pub(crate) async fn read(self) -> ReadToken {
        let guard = self.lock.clone().read_owned().await;
        ReadToken {
            _gate: self,
            guard: Some(guard),
        }
    }
}
impl Default for Backing {
    fn default() -> Self {
        Self::new(Io::new(true))
    }
}
impl Backing {
    pub(crate) fn new(io: Rc<Io>) -> Self {
        Self {
            files: RefCell::default(),
            handles: RefCell::default(),
            gates: Rc::default(),
            io,
        }
    }

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
        self.gates.borrow_mut().clear();
        self.io.finish().await
    }
    pub(crate) async fn open(&self, path: &Path, create: bool) -> io::Result<Arc<fs::File>> {
        self.io.open_backing(path, create).await
    }
    pub(crate) async fn truncate(&self, file: Arc<fs::File>, size: u64) -> io::Result<()> {
        self.io.blocking(move || file.set_len(size)).await
    }
    pub(crate) async fn set_times(
        &self,
        file: Arc<fs::File>,
        times: fs::FileTimes,
    ) -> io::Result<()> {
        self.io.blocking(move || file.set_times(times)).await
    }
    pub(crate) async fn statistics(&self, path: PathBuf) -> io::Result<crate::backend::Statistics> {
        self.io
            .blocking(move || {
                crate::filesystem::Overlay::statistics_at(&path)
                    .map_err(io::Error::from_raw_os_error)
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
                let file = Rc::new(self.io.shared(file));
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
    pub(crate) fn gate(&self, ino: u64) -> Gate {
        let mut gates = self.gates.borrow_mut();
        let mutex = gates
            .get(&ino)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| {
                let lock = Arc::new(tokio::sync::RwLock::new(()));
                gates.insert(ino, Arc::downgrade(&lock));
                lock
            });
        Gate {
            gates: self.gates.clone(),
            ino,
            lock: mutex,
        }
    }
    pub(crate) async fn read(file: &File, offset: u64, count: usize) -> io::Result<Vec<u8>> {
        file.read_up_to(offset, count).await
    }
    /// Preserve a short write's visible progress, including EFBIG/ENOSPC.
    pub(crate) async fn write(file: &File, offset: u64, data: &[u8]) -> io::Result<usize> {
        file.write_partial(offset, data.to_vec()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use tokio_uring::fs::File as NativeFile;
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
                    let mut open = Box::pin(NativeFile::open(&path));
                    poll_fn(|cx| {
                        assert!(open.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    if phase == "completed" {
                        // Drive the CQ before dropping the unconsumed result.
                        NativeFile::open(&path)
                            .await
                            .unwrap()
                            .close()
                            .await
                            .unwrap();
                    }
                    drop(open);
                }
                if phase != "shutdown" {
                    NativeFile::open(&path)
                        .await
                        .unwrap()
                        .close()
                        .await
                        .unwrap();
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
            let native = NativeFile::from_shared_std(file.clone());
            assert_eq!(native.as_raw_fd(), fd);
            native
                .write_all_at(b"contents".to_vec(), 0)
                .await
                .0
                .unwrap();
            native.close().await.unwrap();
            assert_eq!(file.metadata().unwrap().len(), 8);
            let native = NativeFile::from_shared_std(file.clone());
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
            let probe = NativeFile::from_std(tempfile::tempfile().unwrap());
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
                let lock = gate.lock.write().await;
                drop(lock);
                drop(gate);
                assert!(backing.gates.borrow().is_empty());
            }
            assert!(backing.handles.borrow().is_empty());
            drop(backing);
            assert_eq!(Arc::strong_count(&file), 1);
            let native = NativeFile::from_std(tempfile::tempfile().unwrap());
            let fd = native.as_raw_fd();
            let standard = native.try_into_std().expect("unique fd transfer");
            assert_eq!(standard.as_raw_fd(), fd);
            standard.metadata().unwrap();
        });
    }
}
