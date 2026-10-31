//! Thin Linux FUSE adapter. The registry dependency owns the kernel protocol;
//! this module only converts typed callbacks and holds Linux backing handles.
use super::{FileAttr, FileType, SetAttributes, TimeOrNow};
use crate::filesystem::{FsResult, Overlay};
use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileHandle, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, WriteFlags,
};
#[cfg(target_os = "linux")]
use std::{
    collections::HashMap,
    fs::File,
    sync::{Arc, Weak},
};
use std::{
    ffi::OsStr,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime},
};
const TTL: Duration = Duration::ZERO;
// Only pinned, policy-protected names/data may outlive a kernel request.
// Protected directory attributes are immutable too; this does not cache their
// generated children or directory streams. All-policy metadata stays uncached.
const PINNED_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub struct Fuse {
    state: Mutex<State>,
}
struct State {
    core: Overlay,
    // The completely readonly namespace needs no per-open userspace state.
    // Enabled only after the peer advertises the kernel's no-open contract.
    no_open: bool,
    #[cfg(feature = "test-metrics")]
    profile_uncached_source: bool,
    #[cfg(target_os = "linux")]
    enabled: bool,
    #[cfg(target_os = "linux")]
    backings: HashMap<u64, Weak<fuser::BackingId>>,
    #[cfg(target_os = "linux")]
    handles: HashMap<u64, (u64, Arc<fuser::BackingId>)>,
}
impl Fuse {
    pub fn new(core: Overlay) -> Self {
        Self {
            state: Mutex::new(State {
                core,
                no_open: false,
                #[cfg(feature = "test-metrics")]
                profile_uncached_source: std::env::var_os("GIT_ON_TEST_UNCACHED_SOURCE").is_some(),
                #[cfg(target_os = "linux")]
                enabled: false,
                #[cfg(target_os = "linux")]
                backings: HashMap::new(),
                #[cfg(target_os = "linux")]
                handles: HashMap::new(),
            }),
        }
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("filesystem operation panicked")
    }
}
impl State {
    fn retain_source_data(&self, ino: u64) -> bool {
        #[cfg(feature = "test-metrics")]
        if self.profile_uncached_source {
            return false;
        }
        self.core.unchanged_source_data(ino)
    }
    fn source_cacheable(&self, ino: u64) -> bool {
        #[cfg(feature = "test-metrics")]
        if self.profile_uncached_source {
            return false;
        }
        self.core.protected_source(ino)
    }
    fn cache_ttls(&self, a: &FileAttr) -> (Duration, Duration) {
        if self.source_cacheable(a.ino) {
            (PINNED_TTL, PINNED_TTL)
        } else {
            (TTL, TTL)
        }
    }
    #[cfg(target_os = "linux")]
    fn backing(
        &mut self,
        ino: u64,
        register: impl FnOnce(&File) -> std::io::Result<fuser::BackingId>,
    ) -> Option<Arc<fuser::BackingId>> {
        if !self.enabled {
            return None;
        }
        let file = self.core.eligible_backing(ino)?;
        let id = if let Some(id) = self.backings.get(&ino).and_then(Weak::upgrade) {
            id
        } else {
            let id = Arc::new(register(file).ok()?);
            self.backings.insert(ino, Arc::downgrade(&id));
            id
        };
        self.core.enable_external_io(ino);
        Some(id)
    }
    fn release(&mut self, ino: u64, handle: u64) -> FsResult<()> {
        self.core.release_handle(ino, handle)?;
        #[cfg(target_os = "linux")]
        {
            self.handles.remove(&handle);
            if self
                .backings
                .get(&ino)
                .is_some_and(|b| b.strong_count() == 0)
            {
                self.backings.remove(&ino);
            }
        }
        Ok(())
    }
}
fn kind(k: FileType) -> fuser::FileType {
    match k {
        FileType::Directory => fuser::FileType::Directory,
        FileType::RegularFile => fuser::FileType::RegularFile,
        FileType::Symlink => fuser::FileType::Symlink,
        FileType::NamedPipe => fuser::FileType::NamedPipe,
        FileType::CharDevice => fuser::FileType::CharDevice,
        FileType::BlockDevice => fuser::FileType::BlockDevice,
        FileType::Socket => fuser::FileType::Socket,
    }
}
fn attr(a: FileAttr) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: INodeNo(a.ino),
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
fn entry(reply: ReplyEntry, result: FsResult<FileAttr>) {
    match result {
        Ok(a) => reply.entry(&TTL, &attr(a), Generation(0)),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}
fn attributes(reply: ReplyAttr, result: FsResult<FileAttr>) {
    match result {
        Ok(a) => reply.attr(&TTL, &attr(a)),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}
fn empty(reply: ReplyEmpty, result: FsResult<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}
fn data(reply: ReplyData, result: FsResult<Vec<u8>>) {
    match result {
        Ok(data) => reply.data(&data),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}
fn time(t: fuser::TimeOrNow) -> TimeOrNow {
    match t {
        fuser::TimeOrNow::Now => TimeOrNow::Now,
        fuser::TimeOrNow::SpecificTime(t) => TimeOrNow::SpecificTime(t),
    }
}
impl Filesystem for Fuse {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let state = self.state.get_mut().expect("filesystem state poisoned");
            let no_open = state.core.read_only();
            #[cfg(feature = "test-metrics")]
            let no_open = no_open && !state.profile_uncached_source;
            state.no_open = no_open
                && config
                    .add_capabilities(fuser::InitFlags::FUSE_NO_OPEN_SUPPORT)
                    .is_ok();
            state.enabled = crate::platform::has_sys_admin()
                && config
                    .add_capabilities(fuser::InitFlags::FUSE_PASSTHROUGH)
                    .is_ok();
            if state.enabled {
                state.enabled = config.set_max_stack_depth(2).is_ok();
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = config;
        Ok(())
    }
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_lookup", 0);
        let _request = crate::git::RequestGuard::enter(req.pid());
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("lookup");
        match state.core.lookup_entry(parent.0, name) {
            Ok(a) => {
                let (attr_ttl, entry_ttl) = state.cache_ttls(&a);
                reply.entry_with_ttls(&attr_ttl, &entry_ttl, &attr(a), Generation(0));
            }
            Err(error) => reply.error(Errno::from_i32(error)),
        }
    }
    fn forget(&self, _req: &Request, ino: INodeNo, count: u64) {
        self.state().core.forget_inode(ino.0, count);
    }
    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_getattr", 0);
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("getattr");
        let result = if state.no_open && fh == Some(FileHandle(0)) {
            state.core.readonly_file_attributes(ino.0)
        } else {
            state.core.attributes(ino.0, fh.map(|h| h.0))
        };
        match result {
            Ok(a) => reply.attr(&state.cache_ttls(&a).0, &attr(a)),
            Err(error) => reply.error(Errno::from_i32(error)),
        }
    }
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        attributes(
            reply,
            self.state().core.set_attributes(
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
            ),
        );
    }
    fn readlink(&self, req: &Request, ino: INodeNo, reply: ReplyData) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        data(reply, self.state().core.read_link(ino.0));
    }
    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        #[allow(clippy::useless_conversion)]
        let regular = mode & u32::from(libc::S_IFMT);
        #[allow(clippy::useless_conversion)]
        if regular != 0 && regular != u32::from(libc::S_IFREG) {
            reply.error(Errno::EPERM);
            return;
        }
        entry(
            reply,
            self.state().core.create_node(
                parent.0,
                name,
                FileType::RegularFile,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
            ),
        );
    }
    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        entry(
            reply,
            self.state().core.create_node(
                parent.0,
                name,
                FileType::Directory,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
            ),
        );
    }
    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        empty(reply, self.state().core.remove(parent.0, name, false));
    }
    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        empty(reply, self.state().core.remove(parent.0, name, true));
    }
    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        use std::os::unix::ffi::OsStrExt;
        let _request = crate::git::RequestGuard::enter(req.pid());
        entry(
            reply,
            self.state().core.create_node(
                parent.0,
                name,
                FileType::Symlink,
                0o777,
                req.uid(),
                req.gid(),
                target.as_os_str().as_bytes().to_vec(),
            ),
        );
    }
    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        empty(
            reply,
            self.state()
                .core
                .rename_path(parent.0, name, newparent.0, newname, flags.bits()),
        );
    }
    fn link(&self, req: &Request, ino: INodeNo, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        entry(reply, self.state().core.link_node(ino.0, parent.0, name));
    }
    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_open", 0);
        let _request = crate::git::RequestGuard::enter(req.pid());
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("open");
        if state.no_open {
            let error =
                if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
                    libc::EROFS
                } else {
                    state
                        .core
                        .readonly_file_attributes(ino.0)
                        .err()
                        .unwrap_or(libc::ENOSYS)
                };
            reply.error(Errno::from_i32(error));
            return;
        }
        match state.core.open_file(ino.0, flags.0) {
            Ok(fh) => {
                let cache = if state.retain_source_data(ino.0)
                    && flags.0 & libc::O_ACCMODE == libc::O_RDONLY
                {
                    // Copy-up/mutation still uses this FUSE inode's coherent
                    // page cache. All-policy names and metadata keep zero TTL.
                    // NOFLUSH remains limited to source that cannot be mutated.
                    FopenFlags::FOPEN_KEEP_CACHE
                        | if state.source_cacheable(ino.0) {
                            FopenFlags::FOPEN_NOFLUSH
                        } else {
                            FopenFlags::empty()
                        }
                } else {
                    FopenFlags::empty()
                };
                #[cfg(target_os = "linux")]
                if let Some(id) = state.backing(ino.0, |file| reply.open_backing(file)) {
                    state.handles.insert(fh, (ino.0, id.clone()));
                    #[cfg(feature = "test-metrics")]
                    state.core.record_passthrough();
                    reply.opened_passthrough(FileHandle(fh), cache, &id);
                    return;
                }
                reply.opened(FileHandle(fh), cache);
            }
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        let mut state = self.state();
        let result = (|| {
            let a = state.core.create_node(
                parent.0,
                name,
                FileType::RegularFile,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
            )?;
            let fh = match state.core.open_file(a.ino, flags) {
                Ok(fh) => fh,
                Err(error) => {
                    state.core.forget_inode(a.ino, 1);
                    return Err(error);
                }
            };
            Ok((a, fh))
        })();
        match result {
            Ok((a, fh)) => {
                #[cfg(target_os = "linux")]
                if let Some(id) = state.backing(a.ino, |file| reply.open_backing(file)) {
                    state.handles.insert(fh, (a.ino, id.clone()));
                    #[cfg(feature = "test-metrics")]
                    state.core.record_passthrough();
                    reply.created_passthrough(
                        &TTL,
                        &attr(a),
                        Generation(0),
                        FileHandle(fh),
                        FopenFlags::empty(),
                        &id,
                    );
                    return;
                }
                reply.created(
                    &TTL,
                    &attr(a),
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_read", 0);
        let _request = crate::git::RequestGuard::enter(req.pid());
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("read");
        data(
            reply,
            i64::try_from(offset)
                .map_err(|_| libc::EINVAL)
                .and_then(|offset| {
                    if state.no_open && fh.0 == 0 {
                        if flags.0 & libc::O_ACCMODE == libc::O_WRONLY {
                            return Err(libc::EBADF);
                        }
                        state.core.read_without_handle(ino.0, offset, size)
                    } else {
                        state.core.read_data(ino.0, fh.0, offset, size)
                    }
                }),
        );
    }
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = i64::try_from(offset)
            .map_err(|_| libc::EINVAL)
            .and_then(|offset| {
                self.state()
                    .core
                    .write_data(ino.0, fh.0, offset, data, write_flags.bits(), flags.0)
            });
        match result {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_flush", 0);
        let state = self.state();
        if state.no_open && fh.0 == 0 {
            // Linux remembers ENOSYS, avoiding future flush requests for this
            // connection. No source in a readonly mount has dirty data to flush.
            let error = state
                .core
                .readonly_file_attributes(ino.0)
                .err()
                .unwrap_or(libc::ENOSYS);
            reply.error(Errno::from_i32(error));
        } else {
            empty(reply, state.core.validate_handle(ino.0, fh.0));
        }
    }
    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_release", 0);
        empty(reply, self.state().release(ino.0, fh.0));
    }
    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state();
        let result = if state.no_open && fh.0 == 0 {
            state.core.sync_readonly_file(ino.0, datasync)
        } else {
            state.core.sync_file(ino.0, fh.0, datasync)
        };
        empty(reply, result);
    }
    fn opendir(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_opendir", 0);
        let _request = crate::git::RequestGuard::enter(req.pid());
        let mut state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("opendir");
        match state.core.open_directory(ino.0, flags.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_readdir", 0);
        let state = self.state();
        #[cfg(feature = "test-metrics")]
        state.core.record_fuse_request("readdir");
        match state
            .core
            .read_directory(ino.0, fh.0, offset, |ino, k, name, cookie| {
                reply.add(INodeNo(ino), cookie, kind(k), name)
            }) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
    fn releasedir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("fuse_releasedir", 0);
        #[cfg(feature = "test-metrics")]
        self.state().core.record_fuse_request("releasedir");
        empty(reply, self.state().release(ino.0, fh.0));
    }
    fn fsyncdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        empty(reply, self.state().core.sync_directory(ino.0, fh.0));
    }
    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.state().core.statistics() {
            Ok(s) => reply.statfs(
                s.blocks,
                s.free,
                s.available,
                s.files,
                s.free_files,
                s.block_size,
                255,
                s.fragment_size,
            ),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        empty(
            reply,
            self.state()
                .core
                .check_access(ino.0, req.uid(), req.gid(), mask.bits()),
        );
    }
}
