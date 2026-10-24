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
pub struct Fuse {
    state: Mutex<State>,
}
struct State {
    core: Overlay,
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
        let _request = crate::git::RequestGuard::enter(req.pid());
        entry(reply, self.state().core.lookup_entry(parent.0, name));
    }
    fn forget(&self, _req: &Request, ino: INodeNo, count: u64) {
        self.state().core.forget_inode(ino.0, count);
    }
    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        attributes(reply, self.state().core.attributes(ino.0, fh.map(|h| h.0)));
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
        let _request = crate::git::RequestGuard::enter(req.pid());
        let mut state = self.state();
        match state.core.open_file(ino.0, flags.0) {
            Ok(fh) => {
                #[cfg(target_os = "linux")]
                if let Some(id) = state.backing(ino.0, |file| reply.open_backing(file)) {
                    state.handles.insert(fh, (ino.0, id.clone()));
                    #[cfg(feature = "test-metrics")]
                    state.core.record_passthrough();
                    reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &id);
                    return;
                }
                reply.opened(FileHandle(fh), FopenFlags::empty());
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
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        data(
            reply,
            i64::try_from(offset)
                .map_err(|_| libc::EINVAL)
                .and_then(|offset| self.state().core.read_data(ino.0, fh.0, offset, size)),
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
        empty(reply, self.state().core.validate_handle(ino.0, fh.0));
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
        empty(reply, self.state().core.sync_file(ino.0, fh.0, datasync));
    }
    fn opendir(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let _request = crate::git::RequestGuard::enter(req.pid());
        match self.state().core.open_directory(ino.0, flags.0) {
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
        let state = self.state();
        match state.core.entries(ino.0, fh.0, offset) {
            Ok(entries) => {
                for (i, (ino, k, name)) in entries.iter().enumerate() {
                    if reply.add(INodeNo(*ino), offset + i as u64 + 1, kind(*k), name) {
                        break;
                    }
                }
                reply.ok();
            }
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
