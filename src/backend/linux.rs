//! Local asynchronous FUSE adapter. Protocol and request ownership are in the
//! pinned local async-fuser crate; namespace state belongs to this event loop.
use super::{FileAttr, FileType, SetAttributes, TimeOrNow};
use crate::{
    filesystem::{FsResult, Overlay, ReadonlyLookup},
    policy::WritePolicy,
};
use fuser::{
    self as f,
    reply_async::{
        AttrResponse, CreateResponse, DataResponse, DirectoryResponse, EntryResponse, OpenResponse,
        ReadResponse, StatfsResponse, WriteResponse,
    },
};
use std::{
    cell::{RefCell, RefMut},
    collections::HashMap,
    ffi::OsStr,
    path::Path,
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, SystemTime},
};
const TTL: Duration = Duration::ZERO;
const PINNED_TTL: Duration = Duration::from_secs(86400);
pub struct Fuse {
    state: RefCell<State>,
}
struct State {
    core: Overlay,
    objects: Option<Rc<crate::snapshot_async::Snapshot>>,
    no_open: bool,
    cache_directories: bool,
    enabled: bool,
    registrar: Option<f::AsyncBackingRegistrar>,
    backings: HashMap<u64, Weak<f::AsyncBackingId>>,
    handles: HashMap<u64, (u64, Arc<f::AsyncBackingId>)>,
    #[cfg(feature = "test-metrics")]
    test_uncached_source: bool,
}
fn result<T>(r: FsResult<T>) -> Result<T, f::Errno> {
    r.map_err(f::Errno::from_i32)
}
impl Fuse {
    pub fn new(core: Overlay) -> Self {
        Self {
            state: RefCell::new(State {
                core,
                objects: None,
                no_open: false,
                cache_directories: false,
                enabled: false,
                registrar: None,
                backings: HashMap::new(),
                handles: HashMap::new(),
                #[cfg(feature = "test-metrics")]
                test_uncached_source: std::env::var_os("GIT_ON_TEST_UNCACHED_SOURCE").is_some(),
            }),
        }
    }
    fn state(&self) -> RefMut<'_, State> {
        self.state.borrow_mut()
    }
    /// Construct the native local object path after entering its I/O runtime.
    pub fn with_async_source(core: Overlay) -> std::io::Result<Self> {
        let objects = if core.read_only() {
            core.snapshot()
                .async_source()
                .map_err(std::io::Error::other)?
                .map(Rc::new)
        } else {
            None
        };
        let mut filesystem = Self::new(core);
        filesystem.state.get_mut().objects = objects;
        Ok(filesystem)
    }
    async fn lookup_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
    ) -> Result<FileAttr, f::Errno> {
        let mut pin = None;
        loop {
            let next = result(self.state().core.readonly_lookup(parent, name))?;
            match next {
                ReadonlyLookup::Ready(attr) => return Ok(attr),
                ReadonlyLookup::Tree { path, oid } => {
                    pin.get_or_insert_with(|| self.pin(parent));
                    let tree = objects.tree(oid).await.map_err(|_| f::Errno::EIO)?;
                    result(self.state().core.prepare_readonly_directory(path, tree))?;
                }
                ReadonlyLookup::Header { ino, entry } => {
                    pin.get_or_insert_with(|| self.pin(parent));
                    let size = objects.size(entry.oid).await.map_err(|_| f::Errno::EIO)?;
                    return result(self.state().core.complete_readonly_lookup(ino, entry, size));
                }
            }
        }
    }
    fn pin(&self, ino: u64) -> RequestPin<'_> {
        self.state().core.pin_request(ino);
        RequestPin {
            filesystem: self,
            ino,
        }
    }
    pub fn set_backing_registrar(&mut self, registrar: f::AsyncBackingRegistrar) {
        self.state.get_mut().registrar = Some(registrar);
    }
}
struct RequestPin<'a> {
    filesystem: &'a Fuse,
    ino: u64,
}
impl Drop for RequestPin<'_> {
    fn drop(&mut self) {
        self.filesystem.state().core.unpin_request(self.ino);
    }
}
impl State {
    fn retain_source_data(&self, ino: u64) -> bool {
        #[cfg(feature = "test-metrics")]
        if self.test_uncached_source {
            return false;
        }
        self.core.unchanged_source_data(ino)
    }
    fn source_cacheable(&self, ino: u64) -> bool {
        #[cfg(feature = "test-metrics")]
        if self.test_uncached_source {
            return false;
        }
        self.core.protected_source(ino)
    }
    fn ttl(&self, ino: u64) -> Duration {
        if self.source_cacheable(ino) {
            PINNED_TTL
        } else {
            TTL
        }
    }
    fn backing(&mut self, ino: u64) -> Option<Arc<f::AsyncBackingId>> {
        if !self.enabled {
            return None;
        }
        let file = self.core.eligible_backing(ino)?;
        let id = if let Some(id) = self.backings.get(&ino).and_then(Weak::upgrade) {
            id
        } else {
            let id = Arc::new(self.registrar.as_ref()?.open(file).ok()?);
            self.backings.insert(ino, Arc::downgrade(&id));
            id
        };
        self.core.enable_external_io(ino);
        Some(id)
    }
    fn release(&mut self, ino: u64, handle: u64) -> FsResult<()> {
        self.core.release_handle(ino, handle)?;
        self.handles.remove(&handle);
        if self
            .backings
            .get(&ino)
            .is_some_and(|id| id.strong_count() == 0)
        {
            self.backings.remove(&ino);
        }
        Ok(())
    }
}
fn kind(k: FileType) -> f::FileType {
    match k {
        FileType::Directory => f::FileType::Directory,
        FileType::RegularFile => f::FileType::RegularFile,
        FileType::Symlink => f::FileType::Symlink,
        FileType::NamedPipe => f::FileType::NamedPipe,
        FileType::CharDevice => f::FileType::CharDevice,
        FileType::BlockDevice => f::FileType::BlockDevice,
        FileType::Socket => f::FileType::Socket,
    }
}
fn attr(a: FileAttr) -> f::FileAttr {
    f::FileAttr {
        ino: f::INodeNo(a.ino),
        size: a.size,
        blocks: a.blocks,
        atime: a.atime,
        mtime: a.mtime,
        ctime: a.ctime,
        crtime: a.crtime,
        kind: kind(a.kind),
        perm: a.perm,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: a.rdev,
        blksize: a.blksize,
        flags: a.flags,
    }
}
fn time(t: f::TimeOrNow) -> TimeOrNow {
    match t {
        f::TimeOrNow::Now => TimeOrNow::Now,
        f::TimeOrNow::SpecificTime(t) => TimeOrNow::SpecificTime(t),
    }
}
#[allow(clippy::too_many_arguments)]
impl f::AsyncFilesystem for Fuse {
    async fn shutdown(&mut self) -> std::io::Result<()> {
        let state = self.state.get_mut();
        if let Some(objects) = state.objects.take() {
            let objects = Rc::try_unwrap(objects).map_err(|_| {
                std::io::Error::other("object requests still owned during shutdown")
            })?;
            let (metrics, failure) = objects.finish().await.map_err(std::io::Error::other)?;
            state
                .core
                .snapshot()
                .record_async_observation(metrics, failure);
        }
        Ok(())
    }
    async fn init(&mut self, _: &f::Request, config: &mut f::KernelConfig) -> std::io::Result<()> {
        // Bound each owned request buffer while allowing arbitrary file sizes.
        // Large writes are split into ordinary 128 KiB protocol requests.
        config
            .set_max_write(128 * 1024)
            .map_err(|_| std::io::Error::other("invalid FUSE write limit"))?;
        let state = self.state.get_mut();
        state.cache_directories =
            state.core.read_only() && config.kernel_abi() >= f::Version(7, 28);
        #[cfg(feature = "test-metrics")]
        {
            state.cache_directories &= !state.test_uncached_source;
        }
        let no_open = state.core.read_only();
        #[cfg(feature = "test-metrics")]
        let no_open = no_open && !state.test_uncached_source;
        state.no_open = no_open
            && config
                .add_capabilities(f::InitFlags::FUSE_NO_OPEN_SUPPORT)
                .is_ok();
        state.enabled = crate::platform::has_sys_admin()
            && config
                .add_capabilities(f::InitFlags::FUSE_PASSTHROUGH)
                .is_ok();
        if state.enabled {
            state.enabled = config.set_max_stack_depth(2).is_ok();
        }
        Ok(())
    }
    async fn lookup(
        &self,
        _: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
    ) -> Result<EntryResponse, f::Errno> {
        let objects = {
            let state = self.state();
            #[cfg(feature = "test-metrics")]
            state.core.record_fuse_request("lookup");
            state.objects.clone()
        };
        let a = if let Some(objects) = objects {
            self.lookup_local(&objects, parent.0, name).await?
        } else {
            result(self.state().core.lookup_entry(parent.0, name))?
        };
        Ok(EntryResponse::new(
            self.state().ttl(a.ino),
            attr(a),
            f::Generation(0),
        ))
    }
    async fn forget(&self, _: &f::Request, ino: f::INodeNo, count: u64) {
        self.state().core.forget_inode(ino.0, count);
    }
    async fn getattr(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: Option<f::FileHandle>,
    ) -> Result<AttrResponse, f::Errno> {
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("getattr");
        let a = result(if state.no_open && fh == Some(f::FileHandle(0)) {
            state.core.readonly_file_attributes(ino.0)
        } else {
            state.core.attributes(ino.0, fh.map(|h| h.0))
        })?;
        Ok(AttrResponse::new(state.ttl(a.ino), attr(a)))
    }
    async fn setattr(
        &self,
        req: &f::Request,
        ino: f::INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<f::TimeOrNow>,
        mtime: Option<f::TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<f::FileHandle>,
        crtime: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        flags: Option<f::BsdFileFlags>,
    ) -> Result<AttrResponse, f::Errno> {
        let a = result(self.state().core.set_attributes(
            ino.0,
            req.uid(),
            SetAttributes {
                mode,
                owner: uid,
                group: gid,
                size,
                atime: atime.map(time),
                mtime: mtime.map(time),
                ctime,
                fh: fh.map(|h| h.0),
                crtime,
                flags: flags.map(|f| f.bits()),
            },
        ))?;
        Ok(AttrResponse::new(TTL, attr(a)))
    }
    async fn readlink(&self, _: &f::Request, ino: f::INodeNo) -> Result<DataResponse, f::Errno> {
        let objects = self.state().objects.clone();
        if let Some(objects) = objects {
            let (oid, size) = result(self.state().core.readonly_link(ino.0))?;
            let bytes = objects
                .read_link(oid, size)
                .await
                .map_err(|_| f::Errno::EIO)?;
            let (data, range) = crate::raw_blob_async::RawBlobs::into_local_parts(bytes);
            return Ok(DataResponse::from_local_slice(
                data,
                range.start,
                range.len(),
            ));
        }
        Ok(DataResponse::new(result(
            self.state().core.read_link(ino.0),
        )?))
    }
    async fn mknod(
        &self,
        req: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _: u32,
    ) -> Result<EntryResponse, f::Errno> {
        #[allow(clippy::useless_conversion)]
        let regular = mode & u32::from(libc::S_IFMT);
        #[allow(clippy::useless_conversion)]
        if regular != 0 && regular != u32::from(libc::S_IFREG) {
            return Err(f::Errno::EPERM);
        }
        let a = result(self.state().core.create_node(
            parent.0,
            name,
            FileType::RegularFile,
            mode & !umask,
            req.uid(),
            req.gid(),
            Vec::new(),
        ))?;
        Ok(EntryResponse::new(TTL, attr(a), f::Generation(0)))
    }
    async fn mkdir(
        &self,
        req: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
    ) -> Result<EntryResponse, f::Errno> {
        let a = result(self.state().core.create_node(
            parent.0,
            name,
            FileType::Directory,
            mode & !umask,
            req.uid(),
            req.gid(),
            Vec::new(),
        ))?;
        Ok(EntryResponse::new(TTL, attr(a), f::Generation(0)))
    }
    async fn unlink(
        &self,
        _: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
    ) -> Result<(), f::Errno> {
        result(self.state().core.remove(parent.0, name, false))
    }
    async fn rmdir(
        &self,
        _: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
    ) -> Result<(), f::Errno> {
        result(self.state().core.remove(parent.0, name, true))
    }
    async fn symlink(
        &self,
        req: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
        target: &Path,
    ) -> Result<EntryResponse, f::Errno> {
        use std::os::unix::ffi::OsStrExt;
        let a = result(self.state().core.create_node(
            parent.0,
            name,
            FileType::Symlink,
            0o777,
            req.uid(),
            req.gid(),
            target.as_os_str().as_bytes().to_vec(),
        ))?;
        Ok(EntryResponse::new(TTL, attr(a), f::Generation(0)))
    }
    async fn rename(
        &self,
        _: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
        newparent: f::INodeNo,
        newname: &OsStr,
        flags: f::RenameFlags,
    ) -> Result<(), f::Errno> {
        result(
            self.state()
                .core
                .rename_path(parent.0, name, newparent.0, newname, flags.bits()),
        )
    }
    async fn link(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        parent: f::INodeNo,
        name: &OsStr,
    ) -> Result<EntryResponse, f::Errno> {
        let a = result(self.state().core.link_node(ino.0, parent.0, name))?;
        Ok(EntryResponse::new(TTL, attr(a), f::Generation(0)))
    }
    async fn open(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        flags: f::OpenFlags,
    ) -> Result<OpenResponse, f::Errno> {
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("open");
        if state.no_open {
            if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
                return Err(f::Errno::EROFS);
            }
            result(state.core.readonly_file_attributes(ino.0))?;
            return Err(f::Errno::ENOSYS);
        }
        let fh = result(state.core.open_file(ino.0, flags.0))?;
        let cache =
            if state.retain_source_data(ino.0) && flags.0 & libc::O_ACCMODE == libc::O_RDONLY {
                f::FopenFlags::FOPEN_KEEP_CACHE
                    | if state.source_cacheable(ino.0) {
                        f::FopenFlags::FOPEN_NOFLUSH
                    } else {
                        f::FopenFlags::empty()
                    }
            } else {
                f::FopenFlags::empty()
            };
        let response = OpenResponse::new(f::FileHandle(fh), cache);
        if let Some(id) = state.backing(ino.0) {
            state.handles.insert(fh, (ino.0, id.clone()));
            #[cfg(feature = "test-metrics")]
            state.core.record_passthrough();
            return Ok(response.with_backing(id));
        }
        Ok(response)
    }
    async fn create(
        &self,
        req: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
    ) -> Result<CreateResponse, f::Errno> {
        let mut state = self.state();
        let a = result(state.core.create_node(
            parent.0,
            name,
            FileType::RegularFile,
            mode & !umask,
            req.uid(),
            req.gid(),
            Vec::new(),
        ))?;
        let fh = match state.core.open_file(a.ino, flags) {
            Ok(fh) => fh,
            Err(error) => {
                state.core.forget_inode(a.ino, 1);
                return Err(f::Errno::from_i32(error));
            }
        };
        let response = CreateResponse::new(
            TTL,
            attr(a),
            f::Generation(0),
            f::FileHandle(fh),
            f::FopenFlags::empty(),
        );
        if let Some(id) = state.backing(a.ino) {
            state.handles.insert(fh, (a.ino, id.clone()));
            #[cfg(feature = "test-metrics")]
            state.core.record_passthrough();
            return Ok(response.with_backing(id));
        }
        Ok(response)
    }
    async fn read(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        offset: u64,
        size: u32,
        flags: f::OpenFlags,
        _: Option<f::LockOwner>,
    ) -> Result<ReadResponse, f::Errno> {
        i64::try_from(offset).map_err(|_| f::Errno::EINVAL)?;
        let prepared = {
            let state = self.state();
            #[cfg(feature = "test-metrics")]
            state.core.record_fuse_request("read");
            if let Some(objects) = &state.objects {
                let fh = if state.no_open && fh.0 == 0 {
                    if flags.0 & libc::O_ACCMODE == libc::O_WRONLY {
                        return Err(f::Errno::EBADF);
                    }
                    None
                } else {
                    Some(fh.0)
                };
                Some((
                    objects.clone(),
                    result(state.core.readonly_read(ino.0, fh, offset, size))?,
                ))
            } else {
                None
            }
        };
        if let Some((objects, (oid, length, count))) = prepared {
            if count == 0 {
                return Ok(ReadResponse::new(Vec::new()));
            }
            let bytes = objects
                .read(oid, length, offset, count)
                .await
                .map_err(|_| f::Errno::EIO)?;
            let (data, range) = crate::raw_blob_async::RawBlobs::into_local_parts(bytes);
            return Ok(ReadResponse::from_local_slice(
                data,
                range.start,
                range.len(),
            ));
        }
        let mut state = self.state();
        let offset = i64::try_from(offset).map_err(|_| f::Errno::EINVAL)?;
        let data = if state.no_open && fh.0 == 0 {
            if flags.0 & libc::O_ACCMODE == libc::O_WRONLY {
                return Err(f::Errno::EBADF);
            }
            state.core.read_without_handle(ino.0, offset, size)
        } else {
            state.core.read_data(ino.0, fh.0, offset, size)
        };
        Ok(ReadResponse::new(result(data)?))
    }
    async fn write(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: f::WriteFlags,
        flags: f::OpenFlags,
        _: Option<f::LockOwner>,
    ) -> Result<WriteResponse, f::Errno> {
        let offset = i64::try_from(offset).map_err(|_| f::Errno::EINVAL)?;
        Ok(WriteResponse::new(result(self.state().core.write_data(
            ino.0,
            fh.0,
            offset,
            data,
            write_flags.bits(),
            flags.0,
        ))?))
    }
    async fn flush(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        _: f::LockOwner,
    ) -> Result<(), f::Errno> {
        let state = self.state();
        if state.no_open && fh.0 == 0 {
            result(state.core.readonly_file_attributes(ino.0))?;
            Err(f::Errno::ENOSYS)
        } else {
            result(state.core.validate_handle(ino.0, fh.0))
        }
    }
    async fn release(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        _: f::OpenFlags,
        _: Option<f::LockOwner>,
        _: bool,
    ) -> Result<(), f::Errno> {
        result(self.state().release(ino.0, fh.0))
    }
    async fn fsync(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        datasync: bool,
    ) -> Result<(), f::Errno> {
        let mut state = self.state();
        result(if state.no_open && fh.0 == 0 {
            state.core.sync_readonly_file(ino.0, datasync)
        } else {
            state.core.sync_file(ino.0, fh.0, datasync)
        })
    }
    async fn opendir(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        flags: f::OpenFlags,
    ) -> Result<OpenResponse, f::Errno> {
        let objects = self.state().objects.clone();
        let mut pin = None;
        if let Some(objects) = &objects {
            let demand = result(self.state().core.readonly_directory_demand(ino.0))?;
            if let Some((path, oid)) = demand {
                pin = Some(self.pin(ino.0));
                let tree = objects.tree(oid).await.map_err(|_| f::Errno::EIO)?;
                result(self.state().core.prepare_readonly_directory(path, tree))?;
            }
        }
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("opendir");
        let h = result(if objects.is_some() {
            state.core.open_prepared_directory(ino.0, flags.0)
        } else {
            state.core.open_directory(ino.0, flags.0)
        })?;
        let response = OpenResponse::new(
            f::FileHandle(h),
            if state.cache_directories {
                f::FopenFlags::FOPEN_CACHE_DIR | f::FopenFlags::FOPEN_KEEP_CACHE
            } else {
                f::FopenFlags::empty()
            },
        );
        drop(state);
        drop(pin);
        Ok(response)
    }
    async fn readdir(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        size: u32,
        offset: u64,
    ) -> Result<DirectoryResponse, f::Errno> {
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("readdir");
        let mut response = DirectoryResponse::new(size as usize);
        result(
            state
                .core
                .read_directory(ino.0, fh.0, offset, |i, k, n, c| {
                    response.add(f::INodeNo(i), c, kind(k), n)
                }),
        )?;
        Ok(response)
    }
    async fn releasedir(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        _: f::OpenFlags,
    ) -> Result<(), f::Errno> {
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("releasedir");
        result(state.release(ino.0, fh.0))
    }
    async fn fsyncdir(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        _: bool,
    ) -> Result<(), f::Errno> {
        result(self.state().core.sync_directory(ino.0, fh.0))
    }
    async fn statfs(&self, _: &f::Request, _: f::INodeNo) -> Result<StatfsResponse, f::Errno> {
        let s = result(self.state().core.statistics())?;
        Ok(StatfsResponse::new(
            s.blocks,
            s.free,
            s.available,
            s.files,
            s.free_files,
            s.block_size,
            255,
            s.fragment_size,
        ))
    }
    async fn access(
        &self,
        req: &f::Request,
        ino: f::INodeNo,
        mask: f::AccessFlags,
    ) -> Result<(), f::Errno> {
        result(
            self.state()
                .core
                .check_access(ino.0, req.uid(), req.gid(), mask.bits()),
        )
    }
}
/// Start the one local event loop. Startup channel use is confined to mount
/// negotiation; ordinary callbacks never cross a worker queue.
pub fn mount(
    core: Overlay,
    path: &Path,
    id: &str,
    policy: WritePolicy,
) -> std::io::Result<std::thread::JoinHandle<std::io::Result<()>>> {
    let path = path.to_owned();
    let id = id.to_owned();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("git-on-fuse".into())
        .spawn(move || {
            let native_runtime = tokio_uring::Runtime::new(&tokio_uring::builder()).ok();
            let native = native_runtime.is_some();
            let work = async move {
                let mut config = f::Config::default();
                config.mount_options = vec![
                    f::MountOption::FSName(format!("git-on:{id}")),
                    f::MountOption::Subtype("git-on".into()),
                    f::MountOption::NoSuid,
                    f::MountOption::NoDev,
                    f::MountOption::DefaultPermissions,
                ];
                if policy == WritePolicy::None {
                    config.mount_options.push(f::MountOption::RO);
                }
                let filesystem = if native {
                    Fuse::with_async_source(core)?
                } else {
                    Fuse::new(core)
                };
                let mut session = match f::AsyncSessionBuilder::new()
                    .filesystem(filesystem)
                    .mountpoint(&path)
                    .options(config)?
                    .build()
                    .await
                {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return Ok(());
                    }
                };
                let registrar = session.backing_registrar();
                session.filesystem_mut().set_backing_registrar(registrar);
                if ready_tx.send(Ok(())).is_err() {
                    return Ok(());
                }
                session.run().await
            };
            if let Some(runtime) = native_runtime {
                runtime.block_on(work)
            } else {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(work)
            }
        })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => match thread.join() {
            Ok(Err(error)) => Err(error),
            _ => Err(std::io::Error::other(
                "async FUSE mount thread ended during startup",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use f::AsyncFilesystem;
    use git2::Repository;

    #[test]
    fn readonly_prepared_opendir_recreates_forgotten_parent_without_sync_source_reads() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let leaf = repo.treebuilder(None).unwrap().write().unwrap();
        let mut parent_tree = repo.treebuilder(None).unwrap();
        parent_tree.insert("child", leaf, 0o040000).unwrap();
        let parent_tree = parent_tree.write().unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("parent", parent_tree, 0o040000).unwrap();
        let root = root.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "async-parent",
        )
        .unwrap();
        let overlay = Overlay::with_write_policy(
            snapshot.clone(),
            temp.path().join("upper"),
            WritePolicy::None,
        )
        .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_async_source(overlay).unwrap();
                let objects = fs.state().objects.clone().unwrap();
                let parent = fs
                    .lookup_local(&objects, 1, OsStr::new("parent"))
                    .await
                    .unwrap()
                    .ino;
                let child = fs
                    .lookup_local(&objects, parent, OsStr::new("child"))
                    .await
                    .unwrap()
                    .ino;
                fs.state().core.forget_inode(parent, 1);
                let (path, oid) = fs
                    .state()
                    .core
                    .readonly_directory_demand(child)
                    .unwrap()
                    .unwrap();
                let tree = objects.tree(oid).await.unwrap();
                fs.state()
                    .core
                    .prepare_readonly_directory(path, tree)
                    .unwrap();
                let handle = fs
                    .state()
                    .core
                    .open_prepared_directory(child, libc::O_RDONLY)
                    .unwrap();
                let mut rows = Vec::new();
                fs.state()
                    .core
                    .read_directory(child, handle, 0, |ino, _, name, _| {
                        rows.push((ino, name.to_os_string()));
                        false
                    })
                    .unwrap();
                assert_eq!(rows, vec![(child, ".".into()), (parent, "..".into())]);
                assert_eq!(
                    snapshot.metrics().tree_reads,
                    0,
                    "opendir fell back to the synchronous loader"
                );
                fs.state().core.release_handle(child, handle).unwrap();
                drop(objects);
                fs.shutdown().await.unwrap();
                assert_eq!(snapshot.metrics().tree_reads, 3);
            });
    }

    #[test]
    fn readonly_adapter_ram_hits_progress_while_cold_lookup_waits_and_shutdown_joins_source() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let warm = repo.blob(b"verified warm contents").unwrap();
        let cold = repo.blob(b"cold contents").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("warm", warm, 0o100644).unwrap();
        tree.insert("cold", cold, 0o100644).unwrap();
        let oid = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&oid.to_string()),
            "async-adapter",
        )
        .unwrap();
        let overlay = Overlay::with_write_policy(
            snapshot.clone(),
            temp.path().join("upper"),
            WritePolicy::None,
        )
        .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_async_source(overlay).unwrap();
                let objects = fs
                    .state()
                    .objects
                    .clone()
                    .expect("readonly mounted operations must use the local async source");
                let attr = fs
                    .lookup_local(&objects, 1, OsStr::new("warm"))
                    .await
                    .unwrap();
                assert_eq!(
                    objects
                        .read(warm, attr.size, 0, 100)
                        .await
                        .unwrap()
                        .as_slice(),
                    b"verified warm contents"
                );
                assert_eq!(objects.tested_demands(), 3);
                let resume = objects.tested_pause().await;
                let cold_lookup = fs.lookup_local(&objects, 1, OsStr::new("cold"));
                let hit = async {
                    let hot = fs
                        .lookup_local(&objects, 1, OsStr::new("warm"))
                        .await
                        .unwrap();
                    assert_eq!(hot.ino, attr.ino);
                    assert_eq!(
                        objects
                            .read(warm, hot.size, 0, 100)
                            .await
                            .unwrap()
                            .as_slice(),
                        b"verified warm contents"
                    );
                    // If a core borrow or a blocking decoder wait survives await,
                    // this independent hit cannot release the paused source.
                    resume.send(()).unwrap();
                };
                let (cold, ()) = tokio::join!(cold_lookup, hit);
                cold.unwrap();
                assert_eq!(
                    objects.tested_demands(),
                    4,
                    "a warm request submitted extra source work"
                );
                assert_eq!(
                    snapshot.metrics().blob_loads,
                    0,
                    "legacy synchronous payload path was used"
                );
                assert_eq!(
                    snapshot.metrics().header_reads,
                    0,
                    "legacy synchronous metadata path was used"
                );
                drop(objects);
                fs.shutdown().await.unwrap();
                let metrics = snapshot.metrics();
                assert_eq!(
                    (metrics.tree_reads, metrics.header_reads, metrics.blob_loads),
                    (1, 2, 1)
                );
                assert_eq!(
                    (
                        metrics.retained_raw_blob_entries,
                        metrics.retained_raw_blob_bytes
                    ),
                    (0, 0)
                );
            });
    }
}
