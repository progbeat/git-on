//! Local asynchronous FUSE adapter. Protocol and request ownership are in the
//! pinned local async-fuser crate; namespace state belongs to this event loop.
use super::{FileAttr, FileType, SetAttributes, TimeOrNow};
use crate::{
    filesystem::{Authorizations, FsResult, Overlay, SourceLookup},
    policy::WritePolicy,
};
use fuser::{
    self as f,
    reply_async::{
        AttrResponse, CreateResponse, DataResponse, DirectoryPlusResponse, DirectoryResponse,
        EntryResponse, OpenResponse, ReadResponse, StatfsResponse, WriteResponse,
    },
};
use std::{
    cell::{RefCell, RefMut},
    collections::HashMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, SystemTime},
};
const TTL: Duration = Duration::ZERO;
const PINNED_TTL: Duration = Duration::from_secs(86400);
pub struct Fuse {
    state: RefCell<State>,
    policy: crate::policy::LocalPolicy,
    namespace: Arc<tokio::sync::RwLock<()>>,
    io: crate::backing_async::Backing,
    native_all: bool,
}
struct State {
    core: Overlay,
    objects: Option<Rc<crate::snapshot_async::Snapshot>>,
    no_open: bool,
    no_opendir: bool,
    cache_directories: bool,
    directory_plus: bool,
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
            native_all: false,
            io: crate::backing_async::Backing::default(),
            policy: crate::policy::LocalPolicy::default(),
            namespace: Arc::new(tokio::sync::RwLock::new(())),
            state: RefCell::new(State {
                core,
                objects: None,
                no_open: false,
                no_opendir: false,
                cache_directories: false,
                directory_plus: false,
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
    async fn backing_file(&self, ino: u64) -> Result<Arc<std::fs::File>, f::Errno> {
        let file = self.state().core.shared_backing(ino);
        if let Some(file) = file {
            return Ok(file);
        }
        let path = result(self.state().core.backing_path(ino))?;
        self.io.open(&path, false).await.map_err(f::Errno::from)
    }
    async fn prepare_removal(&self, ino: u64) -> Result<Option<PathBuf>, f::Errno> {
        let path = result(self.state().core.removal_backing(ino))?;
        if path.is_some() && self.state().core.shared_backing(ino).is_none() {
            let file = self.backing_file(ino).await?;
            result(self.state().core.retain_backing(ino, file))?;
        }
        Ok(path)
    }
    /// Caller owns this inode's exclusive gate. Source reads retain shared
    /// ownership through their wire reply, so old pages cannot arrive later.
    async fn copy_up_local(&self, ino: u64, preserve: bool) -> Result<(), f::Errno> {
        let plan = result(self.state().core.plan_copy_up(ino))?;
        let Some(plan) = plan else {
            return Ok(());
        };
        let objects = self.state().objects.clone().ok_or(f::Errno::EIO)?;
        let mut bytes = Vec::new();
        let file = if plan.kind == FileType::RegularFile {
            let file = self
                .io
                .open(&plan.path, true)
                .await
                .map_err(f::Errno::from)?;
            let native = self.io.lease(ino, file.clone());
            let copied = async {
                if preserve {
                    let mut offset = 0;
                    while offset < plan.size {
                        let data = objects
                            .read(
                                plan.oid,
                                plan.size,
                                offset,
                                (plan.size - offset).min(64 * 1024) as u32,
                            )
                            .await
                            .map_err(|_| f::Errno::EIO)?;
                        if data.as_slice().is_empty() {
                            return Err(f::Errno::EIO);
                        }
                        let length = data.as_slice().len();
                        native
                            .file()
                            .write_all_at(data, offset)
                            .await
                            .map_err(f::Errno::from)?;
                        offset += length as u64;
                    }
                }
                if plan.unlinked {
                    self.io
                        .io
                        .remove_file(plan.path.clone())
                        .await
                        .map_err(f::Errno::from)?;
                }
                Ok(())
            }
            .await;
            if let Err(error) = copied {
                drop(native);
                drop(file);
                let _ = self.io.io.remove_file(plan.path.clone()).await;
                return Err(error);
            }
            Some(file)
        } else {
            if plan.kind == FileType::Symlink {
                bytes = objects
                    .read_link(plan.oid, plan.size)
                    .await
                    .map_err(|_| f::Errno::EIO)?
                    .as_slice()
                    .to_vec();
            }
            None
        };
        let handles = result(self.state().core.commit_copy_up(ino, file.clone(), bytes))?;
        if let Some(file) = file {
            for fh in handles {
                self.io.opened(ino, fh, file.clone());
            }
        }
        Ok(())
    }
    async fn refresh_local(&self, ino: u64) -> Result<(), f::Errno> {
        if result(self.state().core.external_backing(ino))? {
            let file = self.backing_file(ino).await?;
            let file = self.io.lease(ino, file);
            let metadata = file.file().statx().await.map_err(f::Errno::from)?;
            result(self.state().core.update_io_metadata(ino, &metadata, false))?;
        }
        Ok(())
    }
    async fn attributes_local(&self, ino: u64, fh: Option<u64>) -> Result<FileAttr, f::Errno> {
        if !self.state().core.private_regular(ino) {
            return result(self.state().core.prepared_attributes(ino, fh));
        }
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        self.attributes_local_unlocked(ino, fh).await
    }
    async fn attributes_local_unlocked(
        &self,
        ino: u64,
        fh: Option<u64>,
    ) -> Result<FileAttr, f::Errno> {
        result(self.state().core.prepared_attributes(ino, fh))?;
        if self.state().core.private_regular(ino) {
            self.refresh_local(ino).await?;
        }
        let attr = result(self.state().core.prepared_attributes(ino, fh));
        attr
    }
    async fn pin_all_attributes(
        &self,
        ino: u64,
        fh: Option<u64>,
    ) -> Result<(FileAttr, crate::backing_async::ReadToken), f::Errno> {
        let owner = self.io.gate(ino).read().await;
        // Re-read after acquiring ownership: a writer may have copied up this
        // inode while this request waited, making its attributes uncacheable.
        let attr = self.attributes_local_unlocked(ino, fh).await?;
        Ok((attr, owner))
    }
    async fn all_attributes_response(
        &self,
        ino: u64,
        fh: Option<u64>,
    ) -> Result<AttrResponse, f::Errno> {
        let _pin = self.pin(ino);
        let namespace = self.namespace.clone().read_owned().await;
        let (a, inode) = self.pin_all_attributes(ino, fh).await?;
        let ttl = self.state().all_attribute_ttl(ino);
        let reply = AttrResponse::new(ttl, attr(a)).with_owner((namespace, inode));
        Ok(reply)
    }
    async fn open_local(&self, ino: u64, flags: i32) -> Result<u64, f::Errno> {
        let private = result(self.state().core.validate_open(ino, flags))?;
        let all = self.native_all;
        let mutating = flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0;
        if !private && (!all || !mutating) {
            // No await separates validation from handle insertion. A concurrent
            // copy-up will include this new handle in its short commit.
            return result(self.state().core.open_prepared_file(ino, flags, None));
        }
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        if all && (flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0) {
            self.copy_up_local(ino, flags & libc::O_TRUNC == 0).await?;
        }
        if !self.state().core.private_regular(ino) {
            return result(self.state().core.open_prepared_file(ino, flags, None));
        }
        self.refresh_local(ino).await?;
        let file = self.backing_file(ino).await?;
        if flags & libc::O_TRUNC != 0 {
            self.io
                .truncate(file.clone(), 0)
                .await
                .map_err(f::Errno::from)?;
            let lease = self.io.lease(ino, file.clone());
            let metadata = lease.file().statx().await.map_err(f::Errno::from)?;
            result(self.state().core.update_io_metadata(ino, &metadata, true))?;
        }
        let fh = result(
            self.state()
                .core
                .open_prepared_file(ino, flags, Some(file.clone())),
        )?;
        self.io.opened(ino, fh, file);
        Ok(fh)
    }
    async fn set_attributes_local(
        &self,
        ino: u64,
        caller_uid: u32,
        update: SetAttributes,
    ) -> Result<FileAttr, f::Errno> {
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        result(
            self.state()
                .core
                .validate_set_attributes(ino, caller_uid, update),
        )?;
        if update.size.is_some_and(|size| size > i64::MAX as u64) {
            return Err(f::Errno::EFBIG);
        }
        if update.size.is_some()
            && result(self.state().core.prepared_attributes(ino, None))?.kind
                != FileType::RegularFile
        {
            return Err(f::Errno::EINVAL);
        }
        self.copy_up_local(ino, update.size != Some(0)).await?;
        let regular =
            result(self.state().core.prepared_attributes(ino, None))?.kind == FileType::RegularFile;
        let file = if regular
            && (update.size.is_some() || update.atime.is_some() || update.mtime.is_some())
        {
            Some(self.backing_file(ino).await?)
        } else {
            None
        };
        if let Some(size) = update.size {
            let file = file.as_ref().unwrap();
            self.io
                .truncate(file.clone(), size)
                .await
                .map_err(f::Errno::from)?;
            let lease = self.io.lease(ino, file.clone());
            let metadata = lease.file().statx().await.map_err(f::Errno::from)?;
            result(self.state().core.update_io_metadata(ino, &metadata, true))?;
        }
        let attr = result(self.state().core.apply_set_attributes(ino, update))?;
        if regular && (update.atime.is_some() || update.mtime.is_some()) {
            let mut times = std::fs::FileTimes::new();
            if update.atime.is_some() {
                times = times.set_accessed(attr.atime);
            }
            if update.mtime.is_some() {
                times = times.set_modified(attr.mtime);
            }
            self.io
                .set_times(file.as_ref().unwrap().clone(), times)
                .await
                .map_err(f::Errno::from)?;
        }
        if result(self.state().core.external_backing(ino))? {
            let file = if let Some(file) = file {
                file
            } else {
                self.backing_file(ino).await?
            };
            let lease = self.io.lease(ino, file);
            let metadata = lease.file().statx().await.map_err(f::Errno::from)?;
            result(self.state().core.update_io_metadata(ino, &metadata, false))?;
        }
        let attr = result(self.state().core.prepared_attributes(ino, None));
        attr
    }
    async fn read_backing(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, f::Errno> {
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        self.read_backing_unlocked(ino, fh, offset, size).await
    }
    async fn read_backing_unlocked(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, f::Errno> {
        let plan = result(self.state().core.prepare_read_io(ino, fh))?;
        let file = self.io.lease(ino, plan.file);
        if plan.external {
            let metadata = file.file().statx().await.map_err(f::Errno::from)?;
            result(self.state().core.update_io_metadata(ino, &metadata, false))?;
        }
        let count = result(self.state().core.read_io_size(ino, offset, size))?;
        crate::backing_async::Backing::read(file.file(), offset, count)
            .await
            .map_err(f::Errno::from)
    }
    async fn read_file(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
        flags: i32,
    ) -> Result<ReadResponse, f::Errno> {
        i64::try_from(offset).map_err(|_| f::Errno::EINVAL)?;
        let all = self.native_all;
        let _pin = all.then(|| self.pin(ino));
        let owner = if all {
            Some(self.io.gate(ino).read().await)
        } else {
            None
        };
        let own = |response: ReadResponse| {
            if let Some(owner) = owner {
                response.with_owner(owner)
            } else {
                response
            }
        };
        let prepared = {
            let state = self.state();
            #[cfg(feature = "test-metrics")]
            state.core.record_fuse_request("read");
            if let Some(objects) = &state.objects {
                let fh = if state.no_open && fh == 0 {
                    if flags & libc::O_ACCMODE == libc::O_WRONLY {
                        return Err(f::Errno::EBADF);
                    }
                    result(state.core.readonly_file_attributes(ino))?;
                    None
                } else {
                    Some(fh)
                };
                result(state.core.source_read(ino, fh, offset, size))?
                    .map(|plan| (objects.clone(), plan))
            } else {
                None
            }
        };
        if let Some((objects, (oid, length, count))) = prepared {
            if count == 0 {
                return Ok(own(ReadResponse::new(Vec::new())));
            }
            let bytes = objects
                .read(oid, length, offset, count)
                .await
                .map_err(|_| f::Errno::EIO)?;
            let (data, range) = crate::raw_blob_async::RawBlobs::into_local_parts(bytes);
            return Ok(own(ReadResponse::from_local_slice(
                data,
                range.start,
                range.len(),
            )));
        }
        if self.state().objects.is_some() {
            let data = if all {
                self.read_backing_unlocked(ino, fh, offset, size).await?
            } else {
                self.read_backing(ino, fh, offset, size).await?
            };
            return Ok(own(ReadResponse::new(data)));
        }
        let mut state = self.state();
        let offset = i64::try_from(offset).map_err(|_| f::Errno::EINVAL)?;
        let data = if state.no_open && fh == 0 {
            if flags & libc::O_ACCMODE == libc::O_WRONLY {
                return Err(f::Errno::EBADF);
            }
            state.core.read_without_handle(ino, offset, size)
        } else {
            state.core.read_data(ino, fh, offset, size)
        };
        Ok(ReadResponse::new(result(data)?))
    }
    async fn write_backing(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
    ) -> Result<u32, f::Errno> {
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        let plan = result(self.state().core.prepare_write_io(ino, fh))?;
        let file = self.io.lease(ino, plan.file);
        if plan.external {
            let metadata = file.file().statx().await.map_err(f::Errno::from)?;
            result(self.state().core.update_io_metadata(ino, &metadata, false))?;
        }
        let offset =
            result(
                self.state()
                    .core
                    .write_io_offset(ino, offset, data.len(), write_flags, flags),
            )?;
        if data.is_empty() {
            return Ok(0);
        }
        let written = crate::backing_async::Backing::write(file.file(), offset, data)
            .await
            .map_err(f::Errno::from)?;
        let metadata = file.file().statx().await.map_err(f::Errno::from)?;
        result(self.state().core.update_io_metadata(ino, &metadata, true))?;
        if write_flags & 1 == 0 {
            if flags & libc::O_SYNC == libc::O_SYNC {
                file.file().sync_all().await.map_err(f::Errno::from)?;
            } else if flags & libc::O_DSYNC != 0 {
                file.file().sync_data().await.map_err(f::Errno::from)?;
            }
        }
        Ok(written as u32)
    }
    async fn sync_backing(
        &self,
        ino: u64,
        fh: Option<u64>,
        datasync: bool,
    ) -> Result<(), f::Errno> {
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        let file = result(self.state().core.prepare_sync_io(ino, fh))?;
        if let Some(file) = file {
            let file = self.io.lease(ino, file);
            if datasync {
                file.file().sync_data().await
            } else {
                file.file().sync_all().await
            }
            .map_err(f::Errno::from)?;
        }
        Ok(())
    }
    fn release_prepared_local(&self, ino: u64, fh: u64) -> Result<(), f::Errno> {
        let reply = {
            let mut state = self.state();
            let result = state.core.release_prepared_handle(ino, fh);
            if result.is_ok() {
                state.release_registration(ino, fh);
            }
            result
        };
        if reply.is_ok() {
            self.io.released(ino, fh);
        }
        result(reply)
    }
    async fn release_local(&self, ino: u64, fh: u64) -> Result<(), f::Errno> {
        let source = self.state().core.unchanged_source_data(ino);
        if source {
            // Closing this description changes no source bytes or attributes.
            // Copy-up enumerates the remaining live handles at its short commit.
            return self.release_prepared_local(ino, fh);
        }
        let _pin = self.pin(ino);
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        result(self.state().core.validate_handle(ino, fh))?;
        let plan = self.state().core.metadata_io(ino);
        if let Some(plan) = plan.filter(|p| p.external) {
            let file = self.io.lease(ino, plan.file);
            if let Ok(metadata) = file.file().statx().await {
                let _ = self.state().core.update_io_metadata(ino, &metadata, false);
            }
        }
        self.release_prepared_local(ino, fh)
    }
    /// Construct the local source after entering the io_uring runtime.
    pub fn with_async_source(core: Overlay) -> std::io::Result<Self> {
        Self::with_source_io(core, crate::file_async::Io::new(true))
    }
    /// Keep the same loop-owned source/cache when io_uring is unavailable.
    pub fn with_fallback_source(core: Overlay) -> std::io::Result<Self> {
        Self::with_source_io(core, crate::file_async::Io::new(false))
    }
    fn with_source_io(core: Overlay, io: Rc<crate::file_async::Io>) -> std::io::Result<Self> {
        let objects = core
            .snapshot()
            .async_source(io.clone())
            .map_err(std::io::Error::other)?
            .map(Rc::new);
        let mut filesystem = Self::new(core);
        filesystem.io = crate::backing_async::Backing::new(io);
        filesystem.native_all =
            objects.is_some() && filesystem.state.get_mut().core.write_policy() == WritePolicy::All;
        if objects.is_some() && !filesystem.state.get_mut().core.read_only() {
            filesystem.state.get_mut().core.enable_local_source();
        }
        filesystem.state.get_mut().objects = objects;
        Ok(filesystem)
    }
    async fn lookup_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
    ) -> Result<FileAttr, f::Errno> {
        if self.native_all {
            let _namespace = self.namespace.read().await;
            return self.lookup_local_inner(objects, parent, name).await;
        }
        self.lookup_local_inner(objects, parent, name).await
    }
    async fn lookup_all_response(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
    ) -> Result<EntryResponse, f::Errno> {
        let _pin = self.pin(parent);
        let owner = self.namespace.clone().read_owned().await;
        let a = self.lookup_local_inner(objects, parent, name).await?;
        let (a, inode_owner) = if self.state().all_attribute_ttl(a.ino) != TTL {
            let (a, owner) = self.pin_all_attributes(a.ino, None).await?;
            (a, Some(owner))
        } else {
            (a, None)
        };
        #[cfg(feature = "test-metrics")]
        let entry_ttl = if self.state().test_uncached_source {
            TTL
        } else {
            PINNED_TTL
        };
        #[cfg(not(feature = "test-metrics"))]
        let entry_ttl = PINNED_TTL;
        let reply = EntryResponse::new(
            self.state().all_attribute_ttl(a.ino),
            attr(a),
            f::Generation(0),
        )
        .with_entry_ttl(entry_ttl)
        .with_owner((owner, inode_owner));
        Ok(reply)
    }
    async fn lookup_local_inner(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
    ) -> Result<FileAttr, f::Errno> {
        let mut pin = None;
        loop {
            let next = result(self.state().core.source_lookup(parent, name))?;
            match next {
                SourceLookup::Ready(attr) => return Ok(attr),
                SourceLookup::Path => {
                    pin.get_or_insert_with(|| self.pin(parent));
                    let attr = {
                        let path = result(self.state().core.child_path(parent, name))?;
                        let ino = self.ensure_local(objects, &path).await?;
                        let attr = result(self.state().core.finish_source_lookup(ino))?;
                        attr
                    };
                    match self.attributes_local(attr.ino, None).await {
                        Ok(attr) => return Ok(attr),
                        Err(error) => {
                            self.state().core.forget_inode(attr.ino, 1);
                            return Err(error);
                        }
                    }
                }
                SourceLookup::Private(attr) => match self.attributes_local(attr.ino, None).await {
                    Ok(attr) => return Ok(attr),
                    Err(error) => {
                        self.state().core.forget_inode(attr.ino, 1);
                        return Err(error);
                    }
                },
                SourceLookup::Tree { path, oid } => {
                    pin.get_or_insert_with(|| self.pin(parent));
                    let tree = objects.tree(oid).await.map_err(|_| f::Errno::EIO)?;
                    result(self.state().core.prepare_readonly_directory(path, tree))?;
                }
                SourceLookup::Header { ino, entry, path } => {
                    pin.get_or_insert_with(|| self.pin(parent));
                    let size = objects.size(entry.oid).await.map_err(|_| f::Errno::EIO)?;
                    return result(
                        self.state()
                            .core
                            .complete_source_lookup(ino, entry, size, path),
                    );
                }
            }
        }
    }
    async fn open_directory_local(&self, ino: u64, flags: i32) -> Result<u64, f::Errno> {
        let objects = self.state().objects.clone();
        let mut pin = None;
        if let Some(objects) = &objects {
            let demand = result(self.state().core.readonly_directory_demand(ino))?;
            if let Some((path, oid)) = demand {
                pin = Some(self.pin(ino));
                if let Some(oid) = oid {
                    let tree = objects.tree(oid).await.map_err(|_| f::Errno::EIO)?;
                    result(self.state().core.prepare_readonly_directory(path, tree))?;
                } else {
                    self.prepare_directory(objects, &path).await?;
                }
            }
        }
        let mut state = self.state();
        let h = result(if objects.is_some() && state.core.read_only() {
            state.core.open_prepared_directory(ino, flags)
        } else {
            state.core.open_directory(ino, flags)
        })?;
        drop(state);
        drop(pin);
        Ok(h)
    }
    async fn directory_plus_local(
        &self,
        ino: u64,
        fh: u64,
        size: u32,
        offset: u64,
    ) -> Result<DirectoryPlusResponse, f::Errno> {
        struct Entry {
            ino: u64,
            kind: FileType,
            name: OsString,
            cookie: u64,
            header: Option<git2::Oid>,
            deferred: bool,
            attr: Option<FileAttr>,
        }
        let objects = {
            let state = self.state();
            if !state.directory_plus {
                return Err(f::Errno::ENOSYS);
            }
            state.objects.clone().ok_or(f::Errno::ENOSYS)?
        };
        let namespace = self.namespace.clone().read_owned().await;
        let _pin = self.pin(ino);
        let size = (size as usize).min(128 * 1024);
        let mut remaining = size;
        let mut entries = Vec::new();
        result(
            self.state()
                .core
                .read_directory(ino, fh, offset, |ino, kind, name, cookie| {
                    let Some(left) = remaining.checked_sub(DirectoryPlusResponse::entry_size(name))
                    else {
                        return true;
                    };
                    remaining = left;
                    entries.push(Entry {
                        ino,
                        kind,
                        name: name.to_owned(),
                        cookie,
                        header: None,
                        deferred: false,
                        attr: None,
                    });
                    false
                }),
        )?;
        let placeholder = result(self.state().core.prepared_attributes(ino, None))?;
        let mutable_namespace = !self.state().core.read_only();
        let mut oids = Vec::new();
        for entry in &mut entries {
            if entry.cookie > 2 {
                entry.header = self.state().core.directory_header(ino, &entry.name);
                // Linux probes the first page with PLUS even for names-only
                // callers. Only return already available file attributes there.
                // Later adaptive PLUS pages follow observed metadata demand.
                // Existing metadata-only leaves can use ordinary revalidation
                // without another lookup reference here. After content has
                // been prepared, stay conservative about read-invalidated atime.
                let metadata_only = mutable_namespace
                    && self
                        .state()
                        .core
                        .source_leaf_oid(entry.ino)
                        .is_some_and(|oid| !objects.contents_prepared(oid));
                entry.deferred = metadata_only
                    || offset == 0
                        && (entry.header.is_some_and(|oid| !objects.header_ready(oid))
                            || self.state().core.private_regular(entry.ino));
                if !entry.deferred {
                    oids.extend(entry.header);
                }
            }
        }
        let failed = objects
            .directory_headers(&oids)
            .await
            .unwrap_or_else(|_| oids.into_iter().collect());
        // Complete lookups before taking attribute read ownership. A later
        // alias of a private inode may itself need its exclusive refresh gate.
        for entry in &mut entries {
            if entry.cookie <= 2
                || entry.deferred
                || entry.header.is_some_and(|oid| failed.contains(&oid))
            {
                continue;
            }
            if let Ok(attr) = self.lookup_local_inner(&objects, ino, &entry.name).await {
                if attr.ino == entry.ino {
                    entry.attr = Some(attr);
                } else {
                    // Old open views may contain a removed/replaced binding.
                    // Keep its listed name without publishing a new binding.
                    self.state().core.forget_inode(attr.ino, 1);
                }
            }
        }
        let mut owners = Vec::new();
        if self.native_all {
            let mut attributes = HashMap::new();
            for entry in &mut entries {
                let Some(attr) = entry.attr else { continue };
                if let std::collections::hash_map::Entry::Vacant(slot) = attributes.entry(attr.ino)
                {
                    let attr = if self.state().all_attribute_ttl(attr.ino) != TTL {
                        match self.pin_all_attributes(attr.ino, None).await {
                            Ok((attr, owner)) => {
                                owners.push(owner);
                                Some(attr)
                            }
                            Err(_) => None,
                        }
                    } else {
                        Some(attr)
                    };
                    slot.insert(attr);
                }
                entry.attr = attributes[&attr.ino];
                if entry.attr.is_none() {
                    self.state().core.forget_inode(attr.ino, 1);
                }
            }
        }
        let mut response = DirectoryPlusResponse::new(size);
        for entry in entries {
            let (node, attributes, ttls) = if let Some(attr) = entry.attr {
                let ttl = if self.native_all {
                    self.state().all_attribute_ttl(attr.ino)
                } else {
                    self.state().ttl(attr.ino)
                };
                let entry_ttl = if self.native_all { PINNED_TTL } else { ttl };
                #[cfg(feature = "test-metrics")]
                let entry_ttl = if self.state().test_uncached_source {
                    TTL
                } else {
                    entry_ttl
                };
                (attr.ino, attr, (entry_ttl, ttl))
            } else {
                let mut attr = placeholder;
                attr.ino = entry.ino;
                attr.kind = entry.kind;
                (0, attr, (TTL, TTL))
            };
            assert!(!response.add_with_ttls(
                f::INodeNo(node),
                entry.cookie,
                &entry.name,
                ttls,
                &attr(attributes),
                f::Generation(0)
            ));
        }
        Ok(response.with_owner((namespace, owners)))
    }
    async fn prepare_directory(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        source: &Path,
    ) -> Result<(), f::Errno> {
        if self.state().core.source_directory_ready(source) {
            return Ok(());
        }
        let tree = objects.directory(source).await.map_err(|_| f::Errno::EIO)?;
        result(
            self.state()
                .core
                .prepare_source_directory(source.to_path_buf(), tree),
        )
    }
    async fn prepare_parent(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        path: &Path,
    ) -> Result<(), f::Errno> {
        let source = self.state().core.source_path(path);
        if let Some(source) = source {
            self.prepare_directory(objects, source.parent().ok_or(f::Errno::EIO)?)
                .await?;
        }
        Ok(())
    }
    async fn ensure_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        path: &Path,
    ) -> Result<u64, f::Errno> {
        if let Some(ino) = self.state().core.cached_inode(path) {
            return Ok(ino);
        }
        let source = self
            .state()
            .core
            .source_path(path)
            .ok_or(f::Errno::ENOENT)?;
        let entry = objects.entry(&source).await.map_err(|_| f::Errno::EIO)?;
        self.prepare_parent(objects, path).await?;
        let mut state = self.state();
        if let Some(ino) = state.core.cached_inode(path) {
            return Ok(ino);
        }
        result(
            state
                .core
                .install_source(path, source, entry.ok_or(f::Errno::ENOENT)?),
        )
    }
    async fn authorize(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        paths: Vec<(PathBuf, bool)>,
    ) -> Result<Authorizations, f::Errno> {
        let mode = self.state().core.write_policy();
        let mut decisions = Authorizations::new();
        for (path, directory) in paths {
            if decisions.contains_key(&path) {
                continue;
            }
            let allowed = self
                .policy
                .check(objects, mode, &path, directory)
                .await
                .map_err(|_| f::Errno::EIO)?;
            decisions.insert(path, (directory, allowed));
        }
        Ok(decisions)
    }
    #[allow(clippy::too_many_arguments)]
    async fn create_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
        kind: FileType,
        mode: u32,
        uid: u32,
        gid: u32,
        bytes: Vec<u8>,
        open_flags: Option<i32>,
    ) -> Result<(FileAttr, Option<u64>), f::Errno> {
        let _namespace = self.namespace.write().await;
        let _pin = self.pin(parent);
        if self.state().core.read_only() {
            return Err(f::Errno::EROFS);
        }
        let path = result(self.state().core.child_path(parent, name))?;
        self.prepare_parent(objects, &path).await?;
        let decisions = self
            .authorize(objects, vec![(path, kind == FileType::Directory)])
            .await?;
        let plan = result(self.state().core.with_authorizations(decisions, |core| {
            core.reserve_creation(parent, name, kind, mode, uid, gid, bytes)
        }))?;
        let file = if kind == FileType::RegularFile {
            let path = self.state().core.creation_path(&plan);
            Some(self.io.open(&path, true).await.map_err(f::Errno::from)?)
        } else {
            None
        };
        let a = self
            .state()
            .core
            .create_prepared_node(plan, file.clone().filter(|_| open_flags.is_some()));
        let fh = if let Some(flags) = open_flags {
            let fh = result(
                self.state()
                    .core
                    .open_prepared_file(a.ino, flags, file.clone()),
            )?;
            if let Some(file) = file {
                self.io.opened(a.ino, fh, file);
            }
            Some(fh)
        } else {
            None
        };
        Ok((a, fh))
    }
    #[allow(clippy::too_many_arguments)]
    async fn create_node(
        &self,
        parent: u64,
        name: &OsStr,
        kind: FileType,
        mode: u32,
        uid: u32,
        gid: u32,
        bytes: Vec<u8>,
    ) -> Result<FileAttr, f::Errno> {
        let objects = self.state().objects.clone();
        if let Some(objects) = objects {
            self.create_local(&objects, parent, name, kind, mode, uid, gid, bytes, None)
                .await
                .map(|(attr, _)| attr)
        } else {
            result(
                self.state()
                    .core
                    .create_node(parent, name, kind, mode, uid, gid, bytes),
            )
        }
    }
    async fn remove_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
        directory: bool,
    ) -> Result<(), f::Errno> {
        let _namespace = self.namespace.write().await;
        let _parent = self.pin(parent);
        if self.state().core.read_only() {
            return Err(f::Errno::EROFS);
        }
        let path = result(self.state().core.child_path(parent, name))?;
        let ino = self.ensure_local(objects, &path).await?;
        let _inode = self.pin(ino);
        result(self.state().core.validate_writable(ino))?;
        let kind = result(self.state().core.prepared_attributes(ino, None))?.kind;
        let source = self.state().core.source_path(&path);
        if kind == FileType::Directory {
            if let Some(source) = source {
                self.prepare_directory(objects, &source).await?;
            }
        }
        let decisions = self
            .authorize(objects, vec![(path, kind == FileType::Directory)])
            .await?;
        let plan = result(
            self.state()
                .core
                .with_authorizations(decisions, |core| core.plan_remove(parent, name, directory)),
        )?;
        let gate = self.io.gate(plan.ino);
        let _guard = gate.lock.write().await;
        let path = self.prepare_removal(plan.ino).await?;
        result(self.state().core.commit_remove(plan))?;
        if let Some(path) = path {
            let _ = self.io.io.remove_file(path).await;
        }
        Ok(())
    }
    async fn rename_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
    ) -> Result<(), f::Errno> {
        let _namespace = self.namespace.write().await;
        if flags & !1 != 0 {
            return Err(f::Errno::EINVAL);
        }
        let _parent = self.pin(parent);
        let _newparent = self.pin(newparent);
        if self.state().core.read_only() {
            return Err(f::Errno::EROFS);
        }
        let from = result(self.state().core.child_path(parent, name))?;
        let to = result(self.state().core.child_path(newparent, newname))?;
        let ino = self.ensure_local(objects, &from).await?;
        let _inode = self.pin(ino);
        result(self.state().core.validate_writable(ino))?;
        let destination = match self.ensure_local(objects, &to).await {
            Ok(ino) => Some(ino),
            Err(e) if i32::from(e) == libc::ENOENT => None,
            Err(e) => return Err(e),
        };
        let _target = destination.map(|ino| self.pin(ino));
        if let Some(destination) = destination {
            let kind = result(self.state().core.prepared_attributes(destination, None))?.kind;
            let source = self.state().core.source_path(&to);
            if kind == FileType::Directory {
                if let Some(source) = source {
                    self.prepare_directory(objects, &source).await?;
                }
            }
        }
        let paths = result(
            self.state()
                .core
                .rename_policy_paths(parent, name, newparent, newname),
        )?;
        let decisions = self.authorize(objects, paths).await?;
        let plan = result(self.state().core.with_authorizations(decisions, |core| {
            core.plan_rename(parent, name, newparent, newname, flags)
        }))?;
        let Some(plan) = plan else {
            return Ok(());
        };
        let first = self
            .io
            .gate(plan.destination.map_or(plan.ino, |dst| dst.min(plan.ino)));
        let second = plan
            .destination
            .filter(|dst| *dst != plan.ino)
            .map(|dst| self.io.gate(dst.max(plan.ino)));
        let _first = first.lock.write().await;
        let _second = if let Some(second) = &second {
            Some(second.lock.write().await)
        } else {
            None
        };
        self.copy_up_local(plan.ino, true).await?;
        let path = if let Some(dst) = plan.destination {
            self.prepare_removal(dst).await?
        } else {
            None
        };
        result(self.state().core.commit_rename(plan))?;
        if let Some(path) = path {
            let _ = self.io.io.remove_file(path).await;
        }
        Ok(())
    }
    async fn link_local(
        &self,
        objects: &crate::snapshot_async::Snapshot,
        ino: u64,
        parent: u64,
        name: &OsStr,
    ) -> Result<FileAttr, f::Errno> {
        let _namespace = self.namespace.write().await;
        let _inode = self.pin(ino);
        let _parent = self.pin(parent);
        if self.state().core.read_only() {
            return Err(f::Errno::EROFS);
        }
        let path = result(self.state().core.child_path(parent, name))?;
        let kind = result(self.state().core.prepared_attributes(ino, None))?.kind;
        result(self.state().core.validate_writable(ino))?;
        self.prepare_parent(objects, &path).await?;
        let decisions = self
            .authorize(objects, vec![(path, kind == FileType::Directory)])
            .await?;
        let path = result(
            self.state()
                .core
                .with_authorizations(decisions, |core| core.plan_link(ino, parent, name)),
        )?;
        let gate = self.io.gate(ino);
        let _guard = gate.lock.write().await;
        self.refresh_local(ino).await?;
        self.copy_up_local(ino, true).await?;
        result(self.state().core.commit_link(ino, path))?;
        let reply = result(self.state().core.finish_source_lookup(ino));
        reply
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
    fn all_attribute_ttl(&self, ino: u64) -> Duration {
        #[cfg(feature = "test-metrics")]
        if self.test_uncached_source {
            return TTL;
        }
        if self.core.cacheable_all_attributes(ino) {
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
        self.release_registration(ino, handle);
        Ok(())
    }
    fn release_registration(&mut self, ino: u64, handle: u64) {
        self.handles.remove(&handle);
        if self
            .backings
            .get(&ino)
            .is_some_and(|id| id.strong_count() == 0)
        {
            self.backings.remove(&ino);
        }
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
        // Raw reader/directory closes may still use the shared fallback worker.
        // Finish source ownership before closing that worker's intake.
        let source = async {
            let state = self.state.get_mut();
            if let Some(objects) = state.objects.take() {
                let objects = Rc::try_unwrap(objects).map_err(|_| {
                    std::io::Error::other("object requests still owned during shutdown")
                })?;
                let finished = objects.finish().await.map_err(std::io::Error::other)?;
                state
                    .core
                    .snapshot()
                    .record_async_observation(finished.metrics, finished.remote);
                if let Some(error) = finished.error {
                    return Err(error);
                }
            }
            Ok(())
        }
        .await;
        let io = self.io.finish().await;
        source.and(io)
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
        // Entirely immutable directories need neither per-open state nor
        // release RPCs. Unadvertised kernels keep the normal handle path.
        state.no_opendir = state.cache_directories
            && config
                .add_capabilities(f::InitFlags::FUSE_NO_OPENDIR_SUPPORT)
                .is_ok();
        let no_open = state.core.read_only();
        #[cfg(feature = "test-metrics")]
        let no_open = no_open && !state.test_uncached_source;
        state.no_open = no_open
            && config
                .add_capabilities(f::InitFlags::FUSE_NO_OPEN_SUPPORT)
                .is_ok();
        state.directory_plus = state
            .objects
            .as_ref()
            .is_some_and(|source| source.supports_directory_headers())
            && config.kernel_abi() >= f::Version(7, 21)
            && config
                .add_capabilities(
                    f::InitFlags::FUSE_DO_READDIRPLUS | f::InitFlags::FUSE_READDIRPLUS_AUTO,
                )
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
        if self.native_all {
            return self
                .lookup_all_response(objects.as_deref().unwrap(), parent.0, name)
                .await;
        }
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
        let (native, anonymous) = {
            let state = self.state();
            #[cfg(feature = "test-metrics")]
            state.core.record_fuse_request("getattr");
            let anonymous = if fh == Some(f::FileHandle(0)) && (state.no_open || state.no_opendir) {
                let attributes = result(state.core.prepared_attributes(ino.0, None))?;
                match attributes.kind {
                    FileType::Directory if state.no_opendir => Some(attributes),
                    FileType::RegularFile if state.no_open => Some(attributes),
                    FileType::Directory if state.no_open => return Err(f::Errno::EISDIR),
                    _ if state.no_open => return Err(f::Errno::ELOOP),
                    _ => return Err(f::Errno::EBADF),
                }
            } else {
                None
            };
            (state.objects.is_some(), anonymous)
        };
        if self.native_all && self.state().all_attribute_ttl(ino.0) != TTL {
            return self.all_attributes_response(ino.0, fh.map(|h| h.0)).await;
        }
        let a = if let Some(attributes) = anonymous {
            attributes
        } else if native {
            self.attributes_local(ino.0, fh.map(|h| h.0)).await?
        } else {
            result(self.state().core.attributes(ino.0, fh.map(|h| h.0)))?
        };
        Ok(AttrResponse::new(self.state().ttl(a.ino), attr(a)))
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
        let private = {
            let state = self.state();
            state.objects.is_some()
                && (state.core.private_regular(ino.0)
                    || state.core.write_policy() == WritePolicy::All)
        };
        let update = SetAttributes {
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
        };
        let a = if private {
            self.set_attributes_local(ino.0, req.uid(), update).await?
        } else {
            result(self.state().core.set_attributes(ino.0, req.uid(), update))?
        };
        Ok(AttrResponse::new(TTL, attr(a)))
    }
    async fn readlink(&self, _: &f::Request, ino: f::INodeNo) -> Result<DataResponse, f::Errno> {
        let objects = self.state().objects.clone();
        if let Some(objects) = objects {
            let source = result(self.state().core.source_link(ino.0))?;
            if let Some((oid, size)) = source {
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
        let a = self
            .create_node(
                parent.0,
                name,
                FileType::RegularFile,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
            )
            .await?;
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
        let a = self
            .create_node(
                parent.0,
                name,
                FileType::Directory,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
            )
            .await?;
        Ok(EntryResponse::new(TTL, attr(a), f::Generation(0)))
    }
    async fn unlink(
        &self,
        _: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
    ) -> Result<(), f::Errno> {
        let objects = self.state().objects.clone();
        if let Some(objects) = objects {
            return self.remove_local(&objects, parent.0, name, false).await;
        }
        result(self.state().core.remove(parent.0, name, false))
    }
    async fn rmdir(
        &self,
        _: &f::Request,
        parent: f::INodeNo,
        name: &OsStr,
    ) -> Result<(), f::Errno> {
        let objects = self.state().objects.clone();
        if let Some(objects) = objects {
            return self.remove_local(&objects, parent.0, name, true).await;
        }
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
        let a = self
            .create_node(
                parent.0,
                name,
                FileType::Symlink,
                0o777,
                req.uid(),
                req.gid(),
                target.as_os_str().as_bytes().to_vec(),
            )
            .await?;
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
        let objects = self.state().objects.clone();
        if let Some(objects) = objects {
            return self
                .rename_local(&objects, parent.0, name, newparent.0, newname, flags.bits())
                .await;
        }
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
        let objects = self.state().objects.clone();
        let a = if let Some(objects) = objects {
            self.link_local(&objects, ino.0, parent.0, name).await?
        } else {
            result(self.state().core.link_node(ino.0, parent.0, name))?
        };
        Ok(EntryResponse::new(TTL, attr(a), f::Generation(0)))
    }
    async fn open(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        flags: f::OpenFlags,
    ) -> Result<OpenResponse, f::Errno> {
        let native = {
            let state = self.state();
            #[cfg(feature = "test-metrics")]
            state.core.record_fuse_request("open");
            if state.no_open {
                if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
                    return Err(f::Errno::EROFS);
                }
                result(state.core.readonly_file_attributes(ino.0))?;
                return Err(f::Errno::ENOSYS);
            }
            state.objects.is_some()
        };
        let fh = if native {
            self.open_local(ino.0, flags.0).await?
        } else {
            result(self.state().core.open_file(ino.0, flags.0))?
        };
        let mut state = self.state();
        let cache =
            if state.retain_source_data(ino.0) && flags.0 & libc::O_ACCMODE == libc::O_RDONLY {
                f::FopenFlags::FOPEN_KEEP_CACHE
                    | if state.source_cacheable(ino.0) || self.native_all {
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
        let objects = self.state().objects.clone();
        let (a, opened) = if let Some(objects) = objects {
            self.create_local(
                &objects,
                parent.0,
                name,
                FileType::RegularFile,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
                Some(flags),
            )
            .await?
        } else {
            (
                self.create_node(
                    parent.0,
                    name,
                    FileType::RegularFile,
                    mode & !umask,
                    req.uid(),
                    req.gid(),
                    Vec::new(),
                )
                .await?,
                None,
            )
        };
        let mut state = self.state();
        let fh = match opened
            .map(Ok)
            .unwrap_or_else(|| state.core.open_file(a.ino, flags))
        {
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
        self.read_file(ino.0, fh.0, offset, size, flags.0).await
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
        let signed_offset = i64::try_from(offset).map_err(|_| f::Errno::EINVAL)?;
        if self.state().objects.is_some() {
            return self
                .write_backing(ino.0, fh.0, offset, data, write_flags.bits(), flags.0)
                .await
                .map(WriteResponse::new);
        }
        let offset = signed_offset;
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
        let native = self.state().objects.is_some();
        if native {
            return self.release_local(ino.0, fh.0).await;
        }
        result(self.state().release(ino.0, fh.0))
    }
    async fn fsync(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        datasync: bool,
    ) -> Result<(), f::Errno> {
        if self.state().objects.is_some() {
            let fh = if self.state().no_open && fh.0 == 0 {
                None
            } else {
                Some(fh.0)
            };
            return self.sync_backing(ino.0, fh, datasync).await;
        }
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
        #[cfg(feature = "test-metrics")]
        self.state().core.record_fuse_request("opendir");
        if self.state().no_opendir {
            result(self.state().core.readonly_directory_demand(ino.0))?;
            return Err(f::Errno::ENOSYS);
        }
        let h = self.open_directory_local(ino.0, flags.0).await?;
        let state = self.state();
        let response = OpenResponse::new(
            f::FileHandle(h),
            if state.cache_directories {
                f::FopenFlags::FOPEN_CACHE_DIR | f::FopenFlags::FOPEN_KEEP_CACHE
            } else {
                f::FopenFlags::empty()
            },
        );
        drop(state);
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
        // The kernel omits OPENDIR/RELEASEDIR on this immutable namespace.
        // Keep a temporary stream for the existing page/cookie/lifetime code.
        let temporary = self.state().no_opendir && fh.0 == 0;
        let handle = if temporary {
            self.open_directory_local(ino.0, libc::O_RDONLY).await?
        } else {
            fh.0
        };
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("readdir");
        let mut response = DirectoryResponse::new(size as usize);
        let read = result(
            state
                .core
                .read_directory(ino.0, handle, offset, |i, k, n, c| {
                    response.add(f::INodeNo(i), c, kind(k), n)
                }),
        );
        let close = if temporary {
            result(state.core.release_prepared_handle(ino.0, handle))
        } else {
            Ok(())
        };
        read.and(close)?;
        Ok(response)
    }
    async fn readdirplus(
        &self,
        _: &f::Request,
        ino: f::INodeNo,
        fh: f::FileHandle,
        size: u32,
        offset: u64,
    ) -> Result<DirectoryPlusResponse, f::Errno> {
        #[cfg(feature = "test-metrics")]
        self.state().core.record_fuse_request("readdirplus");
        let temporary = self.state().no_opendir && fh.0 == 0;
        let handle = if temporary {
            self.open_directory_local(ino.0, libc::O_RDONLY).await?
        } else {
            fh.0
        };
        let response = self.directory_plus_local(ino.0, handle, size, offset).await;
        let close = if temporary {
            result(self.state().core.release_prepared_handle(ino.0, handle))
        } else {
            Ok(())
        };
        response.and_then(|response| close.map(|()| response))
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
        let anonymous = self.state().no_opendir && fh.0 == 0;
        if anonymous {
            result(self.state().core.readonly_directory_demand(ino.0))?;
        }
        if self.state().objects.is_some() {
            if !anonymous {
                result(self.state().core.validate_handle(ino.0, fh.0))?;
            }
            let _pin = self.pin(ino.0);
            let path = self.state().core.backing_directory().to_owned();
            let file = self
                .io
                .io
                .open_directory(path)
                .await
                .map_err(f::Errno::from)?;
            let sync = file.sync_all().await;
            let close = file.close().await;
            sync.and(close).map_err(f::Errno::from)?;
            return Ok(());
        }
        if anonymous {
            let path = self.state().core.backing_directory().to_owned();
            std::fs::File::open(path)
                .and_then(|f| f.sync_all())
                .map_err(f::Errno::from)
        } else {
            result(self.state().core.sync_directory(ino.0, fh.0))
        }
    }
    async fn statfs(&self, _: &f::Request, _: f::INodeNo) -> Result<StatfsResponse, f::Errno> {
        let s = if self.state().objects.is_some() {
            let path = self.state().core.backing_directory().to_owned();
            self.io.statistics(path).await.map_err(f::Errno::from)?
        } else {
            result(self.state().core.statistics())?
        };
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
/// negotiation; resident source callbacks never cross a worker queue.
pub fn mount(
    core: Overlay,
    path: &Path,
    id: &str,
    policy: WritePolicy,
) -> std::io::Result<std::thread::JoinHandle<std::io::Result<()>>> {
    mount_with_runtime(core, path, id, policy, true)
}
fn mount_with_runtime(
    core: Overlay,
    path: &Path,
    id: &str,
    policy: WritePolicy,
    prefer_native: bool,
) -> std::io::Result<std::thread::JoinHandle<std::io::Result<()>>> {
    let path = path.to_owned();
    let id = id.to_owned();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("git-on-fuse".into())
        .spawn(move || {
            let native_runtime = if prefer_native {
                tokio_uring::Runtime::new(&tokio_uring::builder()).ok()
            } else {
                None
            };
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
                    Fuse::with_fallback_source(core)?
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
    fn directory_plus_header_failure_keeps_the_name_and_prepares_readable_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let missing = git2::Oid::from_bytes(&[9; 20]).unwrap();
        let good = repo.blob(b"good").unwrap();
        let mut bytes = b"100644 broken\0".to_vec();
        bytes.extend_from_slice(missing.as_bytes());
        bytes.extend_from_slice(b"100644 good\0");
        bytes.extend_from_slice(good.as_bytes());
        let root = repo
            .odb()
            .unwrap()
            .write(git2::ObjectType::Tree, &bytes)
            .unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "plus-errors",
        )
        .unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::All)
                .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_fallback_source(overlay).unwrap();
                fs.state().directory_plus = true;
                let objects = fs.state().objects.clone().unwrap();
                fs.prepare_directory(&objects, Path::new("")).await.unwrap();
                let h = fs.state().core.open_directory(1, libc::O_RDONLY).unwrap();
                let before = objects.tested_demands();
                let reply = fs.directory_plus_local(1, h, 4096, 2).await.unwrap();
                assert_eq!(objects.tested_demands(), before + 1);
                assert!(fs.state().core.cached_inode(Path::new("broken")).is_none());
                assert!(fs.state().core.cached_inode(Path::new("good")).is_some());
                assert_eq!(objects.size(good).await.unwrap(), 4);
                assert!(!fs.io.io.worker_started());
                drop(reply);
                fs.state().core.release_handle(1, h).unwrap();
                drop(objects);
                fs.shutdown().await.unwrap();
            });
    }
    #[test]
    fn directory_plus_bounds_header_work_and_holds_source_reply_ownership() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let first = repo.blob(b"first").unwrap();
        let second = repo.blob(b"second").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("first", first, 0o100644).unwrap();
        tree.insert("second", second, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "plus-owner",
        )
        .unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::All)
                .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_fallback_source(overlay).unwrap();
                fs.state().directory_plus = true;
                let objects = fs.state().objects.clone().unwrap();
                fs.prepare_directory(&objects, Path::new("")).await.unwrap();
                let h = fs.state().core.open_directory(1, libc::O_RDONLY).unwrap();
                let before = objects.tested_demands();
                drop(fs.directory_plus_local(1, h, 4096, 0).await.unwrap());
                assert_eq!(
                    objects.tested_demands(),
                    before,
                    "the cold first page fetched file headers for a names-only caller"
                );
                assert!(fs.state().core.cached_inode(Path::new("first")).is_none());
                drop(fs.directory_plus_local(1, h, 159, 2).await.unwrap());
                assert_eq!(
                    objects.tested_demands(),
                    before,
                    "a non-fitting row caused a header demand"
                );
                let reply = fs.directory_plus_local(1, h, 160, 2).await.unwrap();
                assert_eq!(objects.tested_demands(), before + 1);
                assert!(!fs.io.io.worker_started());
                let ino = fs.state().core.cached_inode(Path::new("first")).unwrap();
                let mut writer = Box::pin(fs.open_local(ino, libc::O_RDWR));
                poll_fn(|cx| {
                    assert!(
                        writer.as_mut().poll(cx).is_pending(),
                        "copy-up passed an older cached directory attribute reply"
                    );
                    Poll::Ready(())
                })
                .await;
                assert!(
                    fs.namespace.try_write().is_err(),
                    "namespace changed before READDIRPLUS reply completion"
                );
                drop(reply);
                let writing = writer.as_mut().await.unwrap();
                drop(writer);
                fs.release_local(ino, writing).await.unwrap();
                assert_eq!(objects.size(second).await.unwrap(), 6);
                assert_eq!(
                    objects.tested_demands(),
                    before + 3,
                    "the bounded first page unexpectedly prefetched its next row"
                );
                fs.state().core.release_handle(1, h).unwrap();
                drop(objects);
                fs.shutdown().await.unwrap();
            });
    }

    #[test]
    fn remote_all_policies_serve_ram_while_transport_waits_without_file_worker() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("server.git")).unwrap();
        repo.config()
            .unwrap()
            .set_bool("uploadpack.allowFilter", true)
            .unwrap();
        repo.config()
            .unwrap()
            .set_bool("uploadpack.allowAnySHA1InWant", true)
            .unwrap();
        let bytes = b"cached remote data";
        let warm = repo.blob(bytes).unwrap();
        let cold = repo.blob(b"another remote object").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", warm, 0o100644).unwrap();
        tree.insert("other", cold, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "fixture",
            &repo.find_tree(root).unwrap(),
            &[],
        )
        .unwrap();
        let source = format!("file://{}", repo.path().display());
        for policy in [
            WritePolicy::None,
            WritePolicy::Ignored,
            WritePolicy::Untracked,
            WritePolicy::All,
        ] {
            let run = temp.path().join(format!("{policy:?}"));
            std::fs::create_dir(&run).unwrap();
            let snapshot = crate::snapshot::Snapshot::load_source_with_pin_record(
                temp.path(),
                Some(OsStr::new(&source)),
                OsStr::new("HEAD"),
                false,
                &run,
                "remote-loop",
                |_, _, _| Ok(()),
            )
            .unwrap();
            let observer = snapshot.clone();
            let overlay = Overlay::with_write_policy(snapshot, run.join("upper"), policy).unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut fs = Fuse::with_fallback_source(overlay).unwrap();
                    let objects = fs
                        .state()
                        .objects
                        .clone()
                        .expect("remote source bypassed the loop cache");
                    let ino = fs
                        .lookup_local(&objects, 1, OsStr::new("source"))
                        .await
                        .unwrap()
                        .ino;
                    let reader = fs.open_local(ino, libc::O_RDONLY).await.unwrap();
                    let result = fs
                        .read_file(ino, reader, 0, 100, libc::O_RDONLY)
                        .await
                        .unwrap();
                    drop(result);
                    let resume = objects.tested_pause().await;
                    let mut cold_size = Box::pin(objects.size(cold));
                    poll_fn(|cx| {
                        assert!(cold_size.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    let demands = objects.tested_demands();
                    let mut read = Box::pin(fs.read_file(ino, reader, 0, 100, libc::O_RDONLY));
                    let response = poll_fn(|cx| match read.as_mut().poll(cx) {
                        Poll::Ready(result) => Poll::Ready(result.unwrap()),
                        Poll::Pending => panic!("RAM read waited for the remote transport"),
                    })
                    .await;
                    assert_eq!(objects.tested_demands(), demands);
                    assert!(!fs.io.io.worker_started());
                    drop(response);
                    drop(read);
                    resume.send(()).unwrap();
                    assert_eq!(cold_size.await.unwrap(), 21);
                    let data = objects
                        .read(warm, bytes.len() as u64, 0, 100)
                        .await
                        .unwrap();
                    assert_eq!(data.as_slice(), bytes);
                    drop(data);
                    fs.state()
                        .core
                        .release_prepared_handle(ino, reader)
                        .unwrap();
                    fs.io.released(ino, reader);
                    drop(objects);
                    fs.shutdown().await.unwrap();
                    assert!(!fs.io.io.worker_started());
                });
            let metrics = observer.metrics();
            assert_eq!(metrics.blob_loads, 1);
            assert_eq!(metrics.header_reads, 2);
            assert_eq!(metrics.remote_raw_blob_imports, 2);
            assert_eq!(metrics.retained_raw_blob_entries, 0);
            assert!(observer.remote_failure().is_none());
        }
    }

    #[test]
    fn readonly_source_close_progresses_while_another_read_reply_orders_copy_up() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let bytes = b"source bytes";
        let oid = repo.blob(bytes).unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", oid, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "source-close",
        )
        .unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::All)
                .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_fallback_source(overlay).unwrap();
                let objects = fs.state().objects.clone().unwrap();
                let ino = fs
                    .lookup_local(&objects, 1, OsStr::new("source"))
                    .await
                    .unwrap()
                    .ino;
                let first = fs.open_local(ino, libc::O_RDONLY).await.unwrap();
                let second = fs.open_local(ino, libc::O_RDONLY).await.unwrap();
                fs.state().core.forget_inode(ino, 1);
                let reply = fs
                    .read_file(ino, first, 0, 100, libc::O_RDONLY)
                    .await
                    .unwrap();
                let mut close = Box::pin(fs.release_local(ino, second));
                let ready = poll_fn(|cx| Poll::Ready(close.as_mut().poll(cx))).await;
                let progressed = ready.is_ready();
                if let Poll::Ready(result) = ready {
                    result.unwrap();
                }
                assert!(!fs.io.io.worker_started());
                let mut writer = Box::pin(fs.open_local(ino, libc::O_RDWR));
                poll_fn(|cx| {
                    assert!(
                        writer.as_mut().poll(cx).is_pending(),
                        "copy-up overtook the source READ reply"
                    );
                    Poll::Ready(())
                })
                .await;
                drop(reply);
                if !progressed {
                    close.as_mut().await.unwrap();
                }
                drop(close);
                let writing = writer.as_mut().await.unwrap();
                drop(writer);
                assert_eq!(
                    fs.state().core.validate_handle(ino, second),
                    Err(libc::EBADF)
                );
                assert_eq!(
                    fs.io.retained().1,
                    2,
                    "copy-up retained the closed source description"
                );
                assert_eq!(fs.read_backing(ino, first, 0, 100).await.unwrap(), bytes);
                fs.release_local(ino, first).await.unwrap();
                fs.release_local(ino, writing).await.unwrap();
                assert_eq!(fs.io.retained(), (0, 0, 0));
                drop(objects);
                fs.shutdown().await.unwrap();
                assert!(
                    progressed,
                    "readonly source close waited for a different handle's READ reply"
                );
            });
    }

    #[test]
    fn all_attribute_replies_order_copy_up_and_parent_mutations_without_blocking_readonly_open() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let oid = repo.blob(b"original").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", oid, 0o100644).unwrap();
        tree.insert("sibling", oid, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "attribute-owner",
        )
        .unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::All)
                .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_fallback_source(overlay).unwrap();
                let objects = fs.state().objects.clone().unwrap();
                let ino = fs
                    .lookup_local(&objects, 1, OsStr::new("source"))
                    .await
                    .unwrap()
                    .ino;
                let sibling = fs
                    .lookup_local(&objects, 1, OsStr::new("sibling"))
                    .await
                    .unwrap()
                    .ino;
                objects.read(oid, 8, 0, 100).await.unwrap();
                let response = fs.all_attributes_response(ino, None).await.unwrap();
                assert!(
                    fs.namespace.try_write().is_err(),
                    "attribute reply lost namespace ownership"
                );
                assert!(
                    fs.io.gate(ino).lock.try_write().is_err(),
                    "attribute reply lost inode ownership"
                );
                let lookup = fs
                    .lookup_all_response(&objects, 1, OsStr::new("source"))
                    .await
                    .unwrap();
                let mut readonly = Box::pin(fs.open_local(ino, libc::O_RDONLY));
                let reader = poll_fn(|cx| match readonly.as_mut().poll(cx) {
                    Poll::Ready(result) => Poll::Ready(result.unwrap()),
                    Poll::Pending => panic!("readonly source open waited for its metadata reply"),
                })
                .await;
                drop(readonly);
                let mut writer = Box::pin(fs.open_local(ino, libc::O_RDWR));
                poll_fn(|cx| {
                    assert!(writer.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                let independent = fs.open_local(sibling, libc::O_RDONLY).await.unwrap();
                drop(
                    fs.read_file(sibling, independent, 0, 100, libc::O_RDONLY)
                        .await
                        .unwrap(),
                );
                drop(response);
                poll_fn(|cx| {
                    assert!(
                        writer.as_mut().poll(cx).is_pending(),
                        "LOOKUP attributes lost inode ownership"
                    );
                    Poll::Ready(())
                })
                .await;
                drop(lookup);
                let writer = writer.await.unwrap();
                assert_eq!(fs.state().all_attribute_ttl(ino), TTL);
                fs.write_backing(ino, writer, 0, b"changed!", 0, libc::O_RDWR)
                    .await
                    .unwrap();
                assert_eq!(
                    fs.read_backing(ino, reader, 0, 100).await.unwrap(),
                    b"changed!"
                );
                let parent = fs.all_attributes_response(1, None).await.unwrap();
                let mut rename = Box::pin(fs.rename_local(
                    &objects,
                    1,
                    OsStr::new("sibling"),
                    1,
                    OsStr::new("moved"),
                    0,
                ));
                poll_fn(|cx| {
                    assert!(rename.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(parent);
                rename.await.unwrap();
                assert_eq!(
                    fs.state().all_attribute_ttl(1),
                    PINNED_TTL,
                    "mutable directory metadata stays kernel-coherent"
                );
                for (ino, fh) in [(ino, reader), (ino, writer), (sibling, independent)] {
                    fs.state().core.release_prepared_handle(ino, fh).unwrap();
                    fs.io.released(ino, fh);
                }
                drop(objects);
                fs.shutdown().await.unwrap();
            });
    }

    #[test]
    #[ignore = "requires real FUSE; executed explicitly by real-fuse CI"]
    fn fallback_real_mount_all_policies() {
        use std::process::Command;
        assert!(Path::new("/dev/fuse").exists(), "real FUSE is required");
        let temp = tempfile::Builder::new()
            .prefix("git-on-fallback-mount-")
            .tempdir()
            .unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".gitignore", repo.blob(b"generated*\n").unwrap(), 0o100644)
            .unwrap();
        tree.insert(
            "source",
            repo.blob(b"committed source\n").unwrap(),
            0o100644,
        )
        .unwrap();
        let large: Vec<_> = (0..2 * 65536 + 17).map(|i| (i % 251) as u8).collect();
        let large_oid = repo.blob(&large).unwrap();
        tree.insert("large", large_oid, 0o100644).unwrap();
        let root = tree.write().unwrap();
        for (policy, name) in [
            (WritePolicy::None, "none"),
            (WritePolicy::Ignored, "ignored"),
            (WritePolicy::Untracked, "untracked"),
            (WritePolicy::All, "all"),
        ] {
            let snapshot =
                crate::snapshot::Snapshot::load(repo.path(), OsStr::new(&root.to_string()), name)
                    .unwrap();
            let mountpoint = temp.path().join(format!("mount-{name}"));
            std::fs::create_dir(&mountpoint).unwrap();
            let upper = temp.path().join(format!("upper-{name}"));
            let overlay = Overlay::with_write_policy(snapshot.clone(), upper, policy).unwrap();
            let session = mount_with_runtime(overlay, &mountpoint, name, policy, false).unwrap();
            let result = Command::new("timeout")
                .args([
                    "--kill-after=5s",
                    "20s",
                    "python3",
                    "-I",
                    "-S",
                    "-c",
                    r#"
import errno, os, sys
from pathlib import Path
policy = sys.argv[1]
source = Path('source')
assert source.read_bytes() == b'committed source\n'
assert source.read_bytes() == b'committed source\n'
assert Path('large').read_bytes() == bytes(i % 251 for i in range(2 * 65536 + 17))
if policy == 'none':
 try: Path('generated').write_bytes(b'forbidden')
 except OSError as error: assert error.errno == errno.EROFS, error
 else: raise AssertionError('readonly create succeeded')
else:
 if policy != 'all':
  try: source.write_bytes(b'forbidden')
  except OSError as error: assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM), error
  else: raise AssertionError('protected write succeeded')
 path = Path('generated')
 with path.open('w+b', buffering=0) as f:
  assert f.write(b'abc') == 3
  assert os.pwrite(f.fileno(), b'tail', 70000) == 4
  assert os.pread(f.fileno(), 5, 69998) == b'\0\0tai'
  os.ftruncate(f.fileno(), 32)
  os.fsync(f.fileno())
  assert os.fstat(f.fileno()).st_size == 32
 assert path.read_bytes() == b'abc' + bytes(29)
 path.chmod(0o600); os.utime(path, (123, 456))
 assert int(path.stat().st_mtime) == 456
 os.rename(path, 'generated-moved')
 assert not path.exists()
 os.link('generated-moved', path)
 assert path.stat().st_nlink == 2
 Path('generated-moved').unlink()
 assert path.stat().st_nlink == 1
 os.mkdir('generated-dir')
 directory = os.open('generated-dir', os.O_RDONLY | os.O_DIRECTORY)
 try: os.fsync(directory)
 finally: os.close(directory)
 assert os.statvfs('.').f_bsize > 0
 os.rmdir('generated-dir')
 os.symlink('source', 'generated-link')
 assert os.readlink('generated-link') == 'source'
 Path('generated-link').unlink(); path.unlink()
 if policy == 'all':
  reader = os.open(source, os.O_RDONLY)
  writer = os.open(source, os.O_RDWR)
  try:
   assert os.pwrite(writer, b'changed', 0) == 7
   assert os.pread(reader, 100, 0) == b'changeded source\n'
   os.rename(source, 'moved'); os.link('moved', source)
   Path('moved').unlink(); source.unlink()
   assert os.fstat(reader).st_nlink == 0
   assert os.pread(reader, 100, 0) == b'changeded source\n'
   os.fsync(writer)
  finally: os.close(writer); os.close(reader)
print('fallback-real-mount', policy)
"#,
                ])
                .arg(name)
                .current_dir(&mountpoint)
                .output();
            let unmounted = Command::new("fusermount3")
                .arg("-u")
                .arg(&mountpoint)
                .output()
                .unwrap();
            assert!(
                unmounted.status.success(),
                "{}",
                String::from_utf8_lossy(&unmounted.stderr)
            );
            session.join().unwrap().unwrap();
            let result = result.unwrap();
            assert!(
                result.status.success(),
                "policy={name}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(repo.find_blob(large_oid).unwrap().content(), large);
            assert_eq!(
                snapshot.metrics().blob_loads,
                if policy == WritePolicy::Ignored { 3 } else { 2 },
                "only ignored policy needs the pinned ignore blob: {name}"
            );
        }
    }

    #[test]
    fn fallback_uses_loop_cache_and_progresses_while_file_worker_waits() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let oid = repo.blob(b"original contents").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", oid, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "fallback-cache",
        )
        .unwrap();
        let overlay = Overlay::with_write_policy(
            snapshot.clone(),
            temp.path().join("upper"),
            WritePolicy::All,
        )
        .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut fs = Fuse::with_fallback_source(overlay).unwrap();
                let objects = fs
                    .state()
                    .objects
                    .clone()
                    .expect("fallback must use the loop-owned cache");
                let ino = fs
                    .lookup_local(&objects, 1, OsStr::new("source"))
                    .await
                    .unwrap()
                    .ino;
                let reader = fs.open_local(ino, libc::O_RDONLY).await.unwrap();
                drop(
                    fs.read_file(ino, reader, 0, 100, libc::O_RDONLY)
                        .await
                        .unwrap(),
                );
                assert_eq!(
                    objects.read(oid, 17, 0, 100).await.unwrap().as_slice(),
                    b"original contents"
                );
                assert!(
                    !fs.io.io.worker_started(),
                    "fitting source reads must not start a file worker"
                );
                let demands = objects.tested_demands();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (resume, paused) = std::sync::mpsc::channel();
                let blocked = fs.io.io.blocking(move || {
                    entered_tx.send(()).unwrap();
                    paused
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(std::io::Error::other)
                });
                let progress = async {
                    entered_rx.await.unwrap();
                    let mut read = Box::pin(fs.read_file(ino, reader, 0, 100, libc::O_RDONLY));
                    poll_fn(|cx| {
                        assert!(
                            matches!(read.as_mut().poll(cx), Poll::Ready(Ok(_))),
                            "RAM read waited for blocked fallback I/O"
                        );
                        Poll::Ready(())
                    })
                    .await;
                    drop(read);
                    assert_eq!(objects.tested_demands(), demands);
                    resume.send(()).unwrap();
                };
                let (result, ()) = tokio::join!(blocked, progress);
                result.unwrap();
                let writer = fs.open_local(ino, libc::O_RDWR).await.unwrap();
                fs.write_backing(ino, writer, 0, b"modified", 0, libc::O_RDWR)
                    .await
                    .unwrap();
                assert_eq!(
                    fs.read_backing(ino, reader, 0, 100).await.unwrap(),
                    b"modified contents"
                );
                fs.rename_local(&objects, 1, OsStr::new("source"), 1, OsStr::new("moved"), 0)
                    .await
                    .unwrap();
                fs.remove_local(&objects, 1, OsStr::new("moved"), false)
                    .await
                    .unwrap();
                assert_eq!(
                    fs.attributes_local(ino, Some(reader)).await.unwrap().nlink,
                    0
                );
                assert_eq!(
                    fs.read_backing(ino, reader, 0, 100).await.unwrap(),
                    b"modified contents"
                );
                fs.sync_backing(ino, Some(writer), false).await.unwrap();
                for fh in [reader, writer] {
                    fs.state().core.release_prepared_handle(ino, fh).unwrap();
                    fs.io.released(ino, fh);
                }
                assert_eq!(fs.io.retained(), (0, 0, 0));
                let legacy = snapshot.metrics();
                assert_eq!(
                    (legacy.tree_reads, legacy.header_reads, legacy.blob_loads),
                    (0, 0, 0)
                );
                drop(objects);
                fs.shutdown().await.unwrap();
                assert!(!fs.io.io.worker_started());
                assert_eq!(snapshot.metrics().blob_loads, 1);
            });
    }

    #[test]
    fn all_lookup_reply_owns_name_binding_until_mutation_can_commit() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let oid = repo.blob(b"original contents").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", oid, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "entry-owner",
        )
        .unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::All)
                .unwrap();
        tokio_uring::start(async {
            let mut fs = Fuse::with_async_source(overlay).unwrap();
            let objects = fs.state().objects.clone().unwrap();
            let ino = fs
                .lookup_local(&objects, 1, OsStr::new("source"))
                .await
                .unwrap()
                .ino;
            objects.read(oid, 17, 0, 100).await.unwrap();
            let response = fs
                .lookup_all_response(&objects, 1, OsStr::new("source"))
                .await
                .unwrap();
            assert!(
                fs.namespace.try_write().is_err(),
                "name ownership ended before LOOKUP reply"
            );
            let mut rename = Box::pin(fs.rename_local(
                &objects,
                1,
                OsStr::new("source"),
                1,
                OsStr::new("moved"),
                0,
            ));
            poll_fn(|cx| {
                assert!(rename.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let fh = fs.open_local(ino, libc::O_RDONLY).await.unwrap();
            drop(fs.read_file(ino, fh, 0, 100, libc::O_RDONLY).await.unwrap());
            drop(response);
            rename.await.unwrap();
            assert_eq!(
                i32::from(
                    fs.lookup_local(&objects, 1, OsStr::new("source"))
                        .await
                        .unwrap_err()
                ),
                libc::ENOENT
            );
            assert_eq!(
                fs.lookup_local(&objects, 1, OsStr::new("moved"))
                    .await
                    .unwrap()
                    .ino,
                ino
            );
            fs.state().core.release_prepared_handle(ino, fh).unwrap();
            fs.io.released(ino, fh);
            assert!(fs.namespace.try_write().is_ok());
            drop(objects);
            fs.shutdown().await.unwrap();
        });
    }

    #[test]
    fn all_streamed_copy_up_preserves_ranges_and_failed_source_removes_pending_backing() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let data: Vec<u8> = (0..1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        let large = repo.blob(&data).unwrap();
        let missing = repo.blob(b"unique missing copy source").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("large", large, 0o100644).unwrap();
        tree.insert("missing", missing, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "all-copy-failure",
        )
        .unwrap();
        let overlay = Overlay::with_write_policy(
            snapshot.clone(),
            temp.path().join("upper"),
            WritePolicy::All,
        )
        .unwrap();
        tokio_uring::start(async {
            let mut fs = Fuse::with_async_source(overlay).unwrap();
            let objects = fs.state().objects.clone().unwrap();
            let large = fs
                .lookup_local(&objects, 1, OsStr::new("large"))
                .await
                .unwrap()
                .ino;
            let reader = fs.open_local(large, libc::O_RDONLY).await.unwrap();
            let writer = fs.open_local(large, libc::O_RDWR).await.unwrap();
            for offset in [0, 65530, data.len() - 100] {
                assert_eq!(
                    fs.read_backing(large, reader, offset as u64, 100)
                        .await
                        .unwrap(),
                    data[offset..offset + 100]
                );
            }
            fs.write_backing(
                large,
                writer,
                data.len() as u64 + 13,
                b"tail",
                0,
                libc::O_RDWR,
            )
            .await
            .unwrap();
            assert_eq!(
                fs.read_backing(large, reader, data.len() as u64, 32)
                    .await
                    .unwrap(),
                [vec![0; 13], b"tail".to_vec()].concat()
            );

            let failed = fs
                .lookup_local(&objects, 1, OsStr::new("missing"))
                .await
                .unwrap()
                .ino;
            let oid = missing.to_string();
            std::fs::remove_file(repo.path().join("objects").join(&oid[..2]).join(&oid[2..]))
                .unwrap();
            let path = fs.state().core.backing_path(failed).unwrap();
            assert_eq!(
                i32::from(fs.open_local(failed, libc::O_RDWR).await.unwrap_err()),
                libc::EIO
            );
            assert!(
                !path.exists(),
                "failed copy-up retained its pending backing"
            );
            assert!(fs.state().core.unchanged_source_data(failed));
            for fh in [reader, writer] {
                fs.state().core.release_prepared_handle(large, fh).unwrap();
                fs.io.released(large, fh);
            }
            assert_eq!(fs.io.retained(), (0, 0, 0));
            drop(objects);
            fs.shutdown().await.unwrap();
            let metrics = snapshot.metrics();
            assert!(metrics.blob_cache_bytes <= metrics.blob_cache_limit);
            assert_eq!(metrics.active_spool_bytes, 0);
        });
    }

    #[test]
    fn all_source_read_reply_orders_copy_up_and_cold_redirects_without_legacy_io() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let payload = repo.blob(b"original contents").unwrap();
        let mut child = repo.treebuilder(None).unwrap();
        child.insert("unvisited", payload, 0o100644).unwrap();
        let directory = child.write().unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", payload, 0o100644).unwrap();
        tree.insert("orphan", payload, 0o100644).unwrap();
        tree.insert("directory", directory, 0o040000).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "async-all",
        )
        .unwrap();
        let overlay = Overlay::with_write_policy(
            snapshot.clone(),
            temp.path().join("upper"),
            WritePolicy::All,
        )
        .unwrap();
        tokio_uring::start(async {
            let mut fs = Fuse::with_async_source(overlay).unwrap();
            let objects = fs
                .state()
                .objects
                .clone()
                .expect("all must use the async source");
            let source = fs
                .lookup_local(&objects, 1, OsStr::new("source"))
                .await
                .unwrap()
                .ino;
            let reader = fs.open_local(source, libc::O_RDONLY).await.unwrap();
            let response = fs
                .read_file(source, reader, 0, 100, libc::O_RDONLY)
                .await
                .unwrap();
            assert!(
                fs.io.gate(source).lock.try_write().is_err(),
                "read ownership ended before the wire response"
            );
            let mut writer = Box::pin(fs.open_local(source, libc::O_RDWR));
            poll_fn(|cx| {
                assert!(
                    writer.as_mut().poll(cx).is_pending(),
                    "copy-up passed an unsent old READ reply"
                );
                Poll::Ready(())
            })
            .await;
            assert_eq!(
                fs.lookup_local(&objects, 1, OsStr::new("orphan"))
                    .await
                    .unwrap()
                    .size,
                17
            );
            assert_eq!(
                objects.read(payload, 17, 0, 100).await.unwrap().as_slice(),
                b"original contents"
            );
            drop(response);
            let writer = writer.await.unwrap();
            fs.write_backing(source, writer, 0, b"modified", 0, libc::O_RDWR)
                .await
                .unwrap();
            assert_eq!(
                fs.read_backing(source, reader, 0, 100).await.unwrap(),
                b"modified contents"
            );
            fs.link_local(&objects, source, 1, OsStr::new("alias"))
                .await
                .unwrap();
            fs.rename_local(&objects, 1, OsStr::new("source"), 1, OsStr::new("moved"), 0)
                .await
                .unwrap();
            fs.remove_local(&objects, 1, OsStr::new("moved"), false)
                .await
                .unwrap();
            fs.remove_local(&objects, 1, OsStr::new("alias"), false)
                .await
                .unwrap();
            assert_eq!(
                fs.attributes_local(source, Some(reader))
                    .await
                    .unwrap()
                    .nlink,
                0
            );
            assert_eq!(
                fs.read_backing(source, reader, 0, 100).await.unwrap(),
                b"modified contents"
            );

            // Copy a directory's namespace before any of its children were
            // visited. Its source origin survives clearing its mutation OID.
            let directory = fs
                .lookup_local(&objects, 1, OsStr::new("directory"))
                .await
                .unwrap()
                .ino;
            fs.rename_local(
                &objects,
                1,
                OsStr::new("directory"),
                1,
                OsStr::new("renamed"),
                0,
            )
            .await
            .unwrap();
            let (path, oid) = fs
                .state()
                .core
                .readonly_directory_demand(directory)
                .unwrap()
                .unwrap();
            assert!(oid.is_none());
            fs.prepare_directory(&objects, &path).await.unwrap();
            let leaf = fs
                .lookup_local(&objects, directory, OsStr::new("unvisited"))
                .await
                .unwrap();
            assert_eq!(leaf.size, 17);
            assert_eq!(
                fs.state()
                    .core
                    .prepared_attributes(leaf.ino, None)
                    .unwrap()
                    .ino,
                leaf.ino
            );

            // Metadata mutation on a source file already unlinked through an
            // outstanding read handle still owns its private copied backing.
            let orphan = fs
                .lookup_local(&objects, 1, OsStr::new("orphan"))
                .await
                .unwrap()
                .ino;
            let orphan_fh = fs.open_local(orphan, libc::O_RDONLY).await.unwrap();
            fs.remove_local(&objects, 1, OsStr::new("orphan"), false)
                .await
                .unwrap();
            fs.set_attributes_local(
                orphan,
                unsafe { libc::geteuid() },
                SetAttributes {
                    mode: Some(0o600),
                    fh: Some(orphan_fh),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(
                fs.read_backing(orphan, orphan_fh, 0, 100).await.unwrap(),
                b"original contents"
            );
            for (ino, fh) in [(source, reader), (source, writer), (orphan, orphan_fh)] {
                fs.state().core.release_prepared_handle(ino, fh).unwrap();
                fs.io.released(ino, fh);
            }
            assert_eq!(fs.io.retained(), (0, 0, 0));
            let legacy = snapshot.metrics();
            assert_eq!(
                (legacy.tree_reads, legacy.header_reads, legacy.blob_loads),
                (0, 0, 0)
            );
            drop(objects);
            fs.shutdown().await.unwrap();
            assert_eq!(snapshot.metrics().blob_loads, 1);
        });
    }

    #[test]
    fn private_native_io_keeps_handles_and_other_inode_progress_without_a_core_borrow() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let oid = repo.blob(b"trusted source").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", oid, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let snapshot = crate::snapshot::Snapshot::load(
            repo.path(),
            OsStr::new(&root.to_string()),
            "native-private",
        )
        .unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::Untracked)
                .unwrap();
        tokio_uring::start(async {
            let mut fs = Fuse::with_async_source(overlay).unwrap();
            let objects = fs.state().objects.clone().unwrap();
            let attr = fs
                .lookup_local(&objects, 1, OsStr::new("source"))
                .await
                .unwrap();
            objects.read(oid, attr.size, 0, 100).await.unwrap();
            let ino = fs
                .create_node(
                    1,
                    OsStr::new("generated"),
                    FileType::RegularFile,
                    0o600,
                    unsafe { libc::geteuid() },
                    unsafe { libc::getegid() },
                    vec![],
                )
                .await
                .unwrap()
                .ino;
            let fh = fs.open_local(ino, libc::O_RDWR).await.unwrap();
            let gate = fs.io.gate(ino);
            let hold = gate.lock.write().await;
            let write = fs.write_backing(ino, fh, 2, b"abc", 0, libc::O_RDWR);
            let progress = async {
                assert_eq!(
                    fs.lookup_local(&objects, 1, OsStr::new("source"))
                        .await
                        .unwrap()
                        .ino,
                    attr.ino
                );
                assert_eq!(
                    objects
                        .read(oid, attr.size, 0, 100)
                        .await
                        .unwrap()
                        .as_slice(),
                    b"trusted source"
                );
                drop(hold);
            };
            let (written, ()) = tokio::join!(write, progress);
            assert_eq!(written.unwrap(), 3);
            drop(gate);
            assert_eq!(fs.read_backing(ino, fh, 0, 100).await.unwrap(), b"\0\0abc");
            assert_eq!(
                fs.write_backing(ino, fh, 0, b"d", 0, libc::O_RDWR | libc::O_APPEND)
                    .await
                    .unwrap(),
                1
            );
            fs.sync_backing(ino, Some(fh), true).await.unwrap();
            fs.sync_backing(ino, Some(fh), false).await.unwrap();
            assert_eq!(fs.read_backing(ino, fh, 0, 100).await.unwrap(), b"\0\0abcd");
            let (started, wait) = tokio::sync::oneshot::channel();
            let (resume, paused) = std::sync::mpsc::channel();
            let blocked = fs.io.io.blocking(move || {
                started.send(()).unwrap();
                paused.recv().unwrap();
                Ok(())
            });
            let progress = async {
                wait.await.unwrap();
                // A blocked backing actor cannot delay ready source data or
                // metadata on the same loop, and holds no core borrow.
                assert_eq!(
                    fs.lookup_local(&objects, 1, OsStr::new("source"))
                        .await
                        .unwrap()
                        .ino,
                    attr.ino
                );
                assert_eq!(
                    objects
                        .read(oid, attr.size, 0, 100)
                        .await
                        .unwrap()
                        .as_slice(),
                    b"trusted source"
                );
                resume.send(()).unwrap();
            };
            let (done, ()) = tokio::join!(blocked, progress);
            done.unwrap();
            let uid = unsafe { libc::geteuid() };
            let changed = fs
                .set_attributes_local(
                    ino,
                    uid,
                    SetAttributes {
                        size: Some(9),
                        mtime: Some(TimeOrNow::SpecificTime(
                            SystemTime::UNIX_EPOCH + Duration::from_secs(7),
                        )),
                        fh: Some(fh),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(changed.size, 9);
            assert_eq!(
                changed.mtime,
                SystemTime::UNIX_EPOCH + Duration::from_secs(7)
            );
            assert_eq!(
                fs.read_backing(ino, fh, 0, 100).await.unwrap(),
                b"\0\0abcd\0\0\0"
            );
            let reopened = fs
                .open_local(ino, libc::O_RDWR | libc::O_TRUNC)
                .await
                .unwrap();
            assert_eq!(fs.attributes_local(ino, Some(fh)).await.unwrap().size, 0);
            fs.state()
                .core
                .release_prepared_handle(ino, reopened)
                .unwrap();
            fs.io.released(ino, reopened);
            fs.state().core.release_prepared_handle(ino, fh).unwrap();
            fs.io.released(ino, fh);
            assert_eq!(fs.io.retained(), (0, 0, 0));
            drop(objects);
            f::AsyncFilesystem::shutdown(&mut fs).await.unwrap();
        });
    }

    #[test]
    fn handled_local_source_errors_do_not_poison_siblings_or_latch_remote_failure() {
        for policy in [
            WritePolicy::None,
            WritePolicy::Ignored,
            WritePolicy::Untracked,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
            let good = repo.blob(b"readable sibling").unwrap();
            let missing = repo.blob(b"missing payload").unwrap();
            let lfs = format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                "a".repeat(64)
            );
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert("good", good, 0o100644).unwrap();
            tree.insert("missing", missing, 0o100644).unwrap();
            tree.insert("lfs", repo.blob(lfs.as_bytes()).unwrap(), 0o100644)
                .unwrap();
            let root = tree.write().unwrap();
            let object = missing.to_string();
            std::fs::remove_file(
                repo.path()
                    .join("objects")
                    .join(&object[..2])
                    .join(&object[2..]),
            )
            .unwrap();
            let snapshot = crate::snapshot::Snapshot::load(
                repo.path(),
                OsStr::new(&root.to_string()),
                "handled-local-error",
            )
            .unwrap();
            let overlay =
                Overlay::with_write_policy(snapshot.clone(), temp.path().join("upper"), policy)
                    .unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut fs = Fuse::with_async_source(overlay).unwrap();
                    let objects = fs.state().objects.clone().unwrap();
                    assert_eq!(
                        i32::from(
                            fs.lookup_local(&objects, 1, OsStr::new("missing"))
                                .await
                                .unwrap_err()
                        ),
                        libc::EIO
                    );
                    let pointer = fs
                        .lookup_local(&objects, 1, OsStr::new("lfs"))
                        .await
                        .unwrap();
                    assert!(objects
                        .read(repo.blob(lfs.as_bytes()).unwrap(), pointer.size, 0, 4096)
                        .await
                        .is_err());
                    let sibling = fs
                        .lookup_local(&objects, 1, OsStr::new("good"))
                        .await
                        .unwrap();
                    assert_eq!(
                        objects
                            .read(good, sibling.size, 0, 4096)
                            .await
                            .unwrap()
                            .as_slice(),
                        b"readable sibling"
                    );
                    drop(objects);
                    fs.shutdown().await.unwrap();
                    assert!(
                        snapshot.remote_failure().is_none(),
                        "handled local errno was turned into an invocation-wide remote failure"
                    );
                });
        }
    }

    #[test]
    fn protected_policies_share_async_source_and_warm_namespace_work_survives_a_paused_decoder() {
        for policy in [WritePolicy::Ignored, WritePolicy::Untracked] {
            let temp = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
            let warm = repo.blob(b"verified source contents").unwrap();
            let cold = repo.blob(b"unrequested contents").unwrap();
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert(
                ".gitignore",
                repo.blob(b"*.tmp\n/out/\n!keep.tmp\n").unwrap(),
                0o100644,
            )
            .unwrap();
            tree.insert("warm", warm, 0o100644).unwrap();
            tree.insert("cold", cold, 0o100644).unwrap();
            let root = tree.write().unwrap();
            let snapshot = crate::snapshot::Snapshot::load(
                repo.path(),
                OsStr::new(&root.to_string()),
                "async-protected",
            )
            .unwrap();
            let overlay =
                Overlay::with_write_policy(snapshot.clone(), temp.path().join("upper"), policy)
                    .unwrap();
            tokio_uring::start(async {
                let mut fs = Fuse::with_async_source(overlay).unwrap();
                let objects = fs
                    .state()
                    .objects
                    .clone()
                    .expect("ordinary protected policies must use the async source");
                let a = fs
                    .lookup_local(&objects, 1, OsStr::new("warm"))
                    .await
                    .unwrap();
                assert_eq!(
                    objects.read(warm, a.size, 0, 100).await.unwrap().as_slice(),
                    b"verified source contents"
                );
                let uid = unsafe { libc::geteuid() };
                let gid = unsafe { libc::getegid() };
                let out = fs
                    .create_node(
                        1,
                        OsStr::new("out"),
                        FileType::Directory,
                        0o700,
                        uid,
                        gid,
                        vec![],
                    )
                    .await
                    .unwrap()
                    .ino;
                let generated = fs
                    .create_node(
                        out,
                        OsStr::new("generated.tmp"),
                        FileType::RegularFile,
                        0o600,
                        uid,
                        gid,
                        vec![],
                    )
                    .await
                    .unwrap()
                    .ino;
                let resume = objects.tested_pause().await;
                let request = fs.lookup_local(&objects, 1, OsStr::new("cold"));
                let hit = async {
                    assert_eq!(
                        fs.lookup_local(&objects, 1, OsStr::new("warm"))
                            .await
                            .unwrap()
                            .ino,
                        a.ino
                    );
                    assert_eq!(
                        objects.read(warm, a.size, 0, 100).await.unwrap().as_slice(),
                        b"verified source contents"
                    );
                    fs.create_node(
                        out,
                        OsStr::new("second.tmp"),
                        FileType::RegularFile,
                        0o600,
                        uid,
                        gid,
                        vec![],
                    )
                    .await
                    .unwrap();
                    assert_eq!(
                        fs.link_local(&objects, generated, out, OsStr::new("alias.tmp"))
                            .await
                            .unwrap()
                            .ino,
                        generated
                    );
                    fs.rename_local(
                        &objects,
                        out,
                        OsStr::new("generated.tmp"),
                        out,
                        OsStr::new("renamed.tmp"),
                        0,
                    )
                    .await
                    .unwrap();
                    fs.remove_local(&objects, out, OsStr::new("alias.tmp"), false)
                        .await
                        .unwrap();
                    let keep = fs
                        .create_node(
                            1,
                            OsStr::new("keep.tmp"),
                            FileType::RegularFile,
                            0o600,
                            uid,
                            gid,
                            vec![],
                        )
                        .await;
                    assert_eq!(keep.is_ok(), policy == WritePolicy::Untracked);
                    resume.send(()).unwrap();
                };
                let (result, ()) = tokio::join!(request, hit);
                result.unwrap();
                let legacy = snapshot.metrics();
                assert_eq!(
                    (legacy.tree_reads, legacy.header_reads, legacy.blob_loads),
                    (0, 0, 0),
                    "a protected operation entered the old synchronous loader"
                );
                drop(objects);
                fs.shutdown().await.unwrap();
                let observed = snapshot.metrics();
                assert_eq!(
                    observed.blob_loads,
                    if policy == WritePolicy::Ignored { 2 } else { 1 }
                );
                assert_eq!(
                    (
                        observed.retained_raw_blob_entries,
                        observed.retained_raw_blob_bytes
                    ),
                    (0, 0)
                );
            });
        }
    }

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
                let tree = objects.tree(oid.unwrap()).await.unwrap();
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
