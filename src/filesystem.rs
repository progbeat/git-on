//! An inode-based union of a pinned Git snapshot and a private, disposable
//! writable layer. User-controlled names never become host filesystem paths.
use crate::policy::Policy;
use crate::snapshot::{BlobReader, EntryKind, Snapshot, BLOB_CACHE_BYTES};
use anyhow::{Context, Result};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use git2::Oid;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

const TTL: Duration = Duration::from_secs(0);
type FsResult<T> = std::result::Result<T, i32>;
fn errno(e: std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

struct Node {
    attr: FileAttr,
    immutable: bool,
    oid: Option<Oid>,
    lookups: u64,
    snapshot_path: Option<PathBuf>,
    bytes: Vec<u8>,
    file: Option<File>,
}
#[derive(Clone)]
struct Handle {
    ino: u64,
    flags: i32,
    reader: Option<BlobReader>,
}

/// Snapshot ownership keeps the Git object pin alive for the entire mount.
pub struct Overlay {
    _snapshot: Snapshot,
    policy: Policy,
    backing: PathBuf,
    paths: BTreeMap<PathBuf, u64>,
    inode_paths: HashMap<u64, PathBuf>,
    upper_paths: BTreeMap<PathBuf, u64>,
    directory_refs: HashMap<u64, usize>,
    nodes: HashMap<u64, Node>,
    handles: HashMap<u64, Handle>,
    directories: HashMap<u64, Vec<(u64, FileType, OsString)>>,
    next_ino: u64,
    next_handle: u64,
    snapshot_time: SystemTime,
}

impl Overlay {
    pub fn new(snapshot: Snapshot, backing: PathBuf) -> Result<Self> {
        fs::create_dir_all(&backing).context("create writable backing directory")?;
        let meta = fs::symlink_metadata(&backing)?;
        anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "backing must be a real directory"
        );
        fs::set_permissions(&backing, fs::Permissions::from_mode(0o700))?;
        snapshot.set_spool_directory(&backing)?;
        let policy = Policy::new(&snapshot)?;
        let snapshot_time = SystemTime::now();
        let mut paths = BTreeMap::new();
        let mut nodes = HashMap::new();
        nodes.insert(
            1,
            Node {
                attr: Self::attr(
                    1,
                    FileType::Directory,
                    0,
                    0o755,
                    unsafe { libc::geteuid() },
                    unsafe { libc::getegid() },
                    snapshot_time,
                ),
                immutable: true,
                oid: Some(snapshot.tree_oid),
                lookups: 1,
                snapshot_path: Some(PathBuf::new()),
                bytes: Vec::new(),
                file: None,
            },
        );
        paths.insert(PathBuf::new(), 1);
        Ok(Self {
            _snapshot: snapshot,
            policy,
            backing,
            paths,
            inode_paths: HashMap::from([(1, PathBuf::new())]),
            upper_paths: BTreeMap::new(),
            directory_refs: HashMap::new(),
            nodes,
            handles: HashMap::new(),
            directories: HashMap::new(),
            next_ino: 2,
            next_handle: 1,
            snapshot_time,
        })
    }
    /// Materialize inode metadata only for the requested pathname. No sibling
    /// blobs or descendant trees are loaded as a side effect.
    fn ensure(&mut self, path: &Path) -> FsResult<u64> {
        if let Some(ino) = self.paths.get(path) {
            if self.nodes.contains_key(ino) {
                return Ok(*ino);
            }
        }
        let entry = self
            ._snapshot
            .entry(path)
            .map_err(|_| libc::EIO)?
            .ok_or(libc::ENOENT)?;
        let ino = if let Some(ino) = self.paths.get(path) {
            *ino
        } else {
            let ino = self.next_ino;
            self.next_ino = self.next_ino.checked_add(1).ok_or(libc::ENOSPC)?;
            ino
        };
        let kind = match entry.kind {
            EntryKind::File => FileType::RegularFile,
            EntryKind::Directory => FileType::Directory,
            EntryKind::Symlink => FileType::Symlink,
        };
        let perm = match kind {
            FileType::Directory => 0o755,
            FileType::Symlink => 0o777,
            _ => (entry.mode & 0o777) as u16,
        };
        self.nodes.insert(
            ino,
            Node {
                attr: Self::attr(
                    ino,
                    kind,
                    entry.size.ok_or(libc::EIO)?,
                    perm,
                    unsafe { libc::geteuid() },
                    unsafe { libc::getegid() },
                    self.snapshot_time,
                ),
                immutable: true,
                oid: Some(entry.oid),
                lookups: 0,
                snapshot_path: Some(path.to_path_buf()),
                bytes: Vec::new(),
                file: None,
            },
        );
        self.paths.insert(path.to_path_buf(), ino);
        self.inode_paths.insert(ino, path.to_path_buf());
        Ok(ino)
    }
    fn forget_inode(&mut self, ino: u64, count: u64) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups = node.lookups.saturating_sub(count);
        }
        self.reclaim(ino);
    }
    fn reclaim(&mut self, ino: u64) {
        if ino == 1
            || self.handles.values().any(|h| h.ino == ino)
            || self.directory_refs.get(&ino).copied().unwrap_or(0) > 0
        {
            return;
        }
        let removable = self
            .nodes
            .get(&ino)
            .is_none_or(|n| n.lookups == 0 && (n.immutable || n.attr.nlink == 0));
        if removable {
            self.nodes.remove(&ino);
            if let Some(path) = self.inode_paths.remove(&ino) {
                self.paths.remove(&path);
            }
            // Unlinked mutable nodes have no pathname left to remove.
        }
    }
    fn open_directory(&mut self, ino: u64, flags: i32) -> FsResult<u64> {
        if self.node(ino)?.attr.kind != FileType::Directory {
            return Err(libc::ENOTDIR);
        }
        let path = self.path(ino)?;
        let parent = match path.parent() {
            Some(parent) => self.ensure(parent)?,
            None => 1,
        };
        let mut entries = vec![
            (ino, FileType::Directory, OsString::from(".")),
            (parent, FileType::Directory, OsString::from("..")),
        ];
        if self.node(ino)?.immutable {
            for (name, entry) in self._snapshot.children(&path).map_err(|_| libc::EIO)? {
                let kind = match entry.kind {
                    EntryKind::Directory => FileType::Directory,
                    EntryKind::Symlink => FileType::Symlink,
                    EntryKind::File => FileType::RegularFile,
                };
                let child_path = path.join(&name);
                let child = if let Some(id) = self.paths.get(&child_path) {
                    *id
                } else {
                    let id = self.next_ino;
                    self.next_ino = self.next_ino.checked_add(1).ok_or(libc::ENOSPC)?;
                    self.inode_paths.insert(id, child_path.clone());
                    self.paths.insert(child_path, id);
                    id
                };
                entries.push((child, kind, name));
            }
        }
        for (p, child) in self
            .upper_paths
            .range(path.clone()..)
            .take_while(|(p, _)| p.starts_with(&path))
        {
            if p != &path
                && p.parent() == Some(path.as_path())
                && self.nodes.get(child).is_some_and(|n| !n.immutable)
            {
                entries.push((
                    *child,
                    self.node(*child)?.attr.kind,
                    p.file_name().ok_or(libc::EIO)?.to_os_string(),
                ));
            }
        }
        let fh = self.new_handle(ino, flags);
        for (child, _, _) in &entries {
            *self.directory_refs.entry(*child).or_default() += 1;
        }
        self.directories.insert(fh, entries);
        Ok(fh)
    }
    fn close_handle(&mut self, ino: u64, fh: u64) {
        self.handles.remove(&fh);
        let listed = self.directories.remove(&fh).unwrap_or_default();
        for (child, _, _) in &listed {
            if let Some(count) = self.directory_refs.get_mut(child) {
                *count -= 1;
                if *count == 0 {
                    self.directory_refs.remove(child);
                }
            }
        }
        for (child, _, _) in listed {
            self.reclaim(child);
        }
        if !self.handles.values().any(|h| h.ino == ino) {
            if let Some(n) = self.nodes.get_mut(&ino) {
                n.file = None;
            }
        }
        self.reclaim(ino);
    }
    fn attr(
        ino: u64,
        kind: FileType,
        size: u64,
        perm: u16,
        uid: u32,
        gid: u32,
        now: SystemTime,
    ) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
    fn node(&self, ino: u64) -> FsResult<&Node> {
        self.nodes.get(&ino).ok_or(libc::ENOENT)
    }
    fn node_mut(&mut self, ino: u64) -> FsResult<&mut Node> {
        self.nodes.get_mut(&ino).ok_or(libc::ENOENT)
    }
    fn path(&self, ino: u64) -> FsResult<PathBuf> {
        if let Some(path) = &self.node(ino)?.snapshot_path {
            return Ok(path.clone());
        }
        self.paths
            .iter()
            .find_map(|(p, n)| (*n == ino).then(|| p.clone()))
            .ok_or(libc::ENOENT)
    }
    fn child(&self, parent: u64, name: &OsStr) -> FsResult<PathBuf> {
        if self.node(parent)?.attr.kind != FileType::Directory {
            return Err(libc::ENOTDIR);
        }
        let mut c = Path::new(name).components();
        if !matches!(c.next(), Some(Component::Normal(_)))
            || c.next().is_some()
            || name.as_bytes().contains(&0)
        {
            return Err(libc::EINVAL);
        }
        if name.as_bytes().len() > 255 {
            return Err(libc::ENAMETOOLONG);
        }
        Ok(self.path(parent)?.join(name))
    }
    fn writable(&self, ino: u64) -> FsResult<()> {
        if self.node(ino)?.immutable {
            Err(libc::EROFS)
        } else {
            Ok(())
        }
    }
    fn allowed(&self, p: &Path, kind: FileType) -> FsResult<()> {
        if self.policy.allows(p, kind == FileType::Directory) {
            Ok(())
        } else {
            Err(libc::EROFS)
        }
    }
    fn fresh(&self, parent: u64, name: &OsStr, kind: FileType) -> FsResult<PathBuf> {
        let p = self.child(parent, name)?;
        if self.paths.contains_key(&p) || self._snapshot.entry(&p).map_err(|_| libc::EIO)?.is_some()
        {
            return Err(libc::EEXIST);
        }
        self.allowed(&p, kind)?;
        Ok(p)
    }
    fn touch_parent(&mut self, path: &Path) {
        if let Some(ino) = path.parent().and_then(|p| self.paths.get(p)).copied() {
            // A tracked directory's metadata belongs to the snapshot as well.
            if let Some(node) = self.nodes.get_mut(&ino) {
                if !node.immutable {
                    node.attr.mtime = SystemTime::now();
                    node.attr.ctime = node.attr.mtime;
                }
            }
        }
    }
    #[allow(clippy::too_many_arguments)] // Mirrors FUSE creation fields.
    fn make(
        &mut self,
        parent: u64,
        name: &OsStr,
        kind: FileType,
        mode: u32,
        uid: u32,
        gid: u32,
        bytes: Vec<u8>,
    ) -> FsResult<u64> {
        let p = self.fresh(parent, name, kind)?;
        let ino = self.next_ino;
        self.next_ino = self.next_ino.checked_add(1).ok_or(libc::ENOSPC)?;
        // Only generated numeric inode IDs are ever resolved on the host.
        // O_EXCL and O_NOFOLLOW prevent pre-existing links from being followed.
        let file = if kind == FileType::RegularFile {
            Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(self.backing.join(ino.to_string()))
                    .map_err(errno)?,
            )
        } else {
            None
        };
        let attr = Self::attr(
            ino,
            kind,
            bytes.len() as u64,
            (mode & 0o777) as u16,
            uid,
            gid,
            SystemTime::now(),
        );
        drop(file);
        self.nodes.insert(
            ino,
            Node {
                attr,
                immutable: false,
                oid: None,
                lookups: 0,
                snapshot_path: None,
                bytes,
                file: None,
            },
        );
        self.paths.insert(p.clone(), ino);
        self.upper_paths.insert(p.clone(), ino);
        self.touch_parent(&p);
        Ok(ino)
    }
    fn new_handle(&mut self, ino: u64, flags: i32) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            fh,
            Handle {
                ino,
                flags,
                reader: None,
            },
        );
        fh
    }
    fn handle(&self, ino: u64, fh: u64) -> FsResult<Handle> {
        self.handles
            .get(&fh)
            .filter(|h| h.ino == ino)
            .cloned()
            .ok_or(libc::EBADF)
    }
    fn open_file(&mut self, ino: u64, flags: i32) -> FsResult<u64> {
        let node = self.node(ino)?;
        if node.attr.kind != FileType::RegularFile {
            return Err(if node.attr.kind == FileType::Directory {
                libc::EISDIR
            } else {
                libc::ELOOP
            });
        }
        if flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0 {
            self.writable(ino)?;
        }
        if flags & libc::O_TRUNC != 0 {
            self.truncate(ino, 0)?;
        }
        if !self.node(ino)?.immutable && self.node(ino)?.file.is_none() {
            let file = self.disk_file(ino)?;
            self.node_mut(ino)?.file = Some(file);
        }
        Ok(self.new_handle(ino, flags))
    }
    fn disk_file(&self, ino: u64) -> FsResult<File> {
        let n = self.node(ino)?;
        if let Some(f) = &n.file {
            return f.try_clone().map_err(errno);
        }
        if n.attr.nlink == 0 {
            return Err(libc::ENOENT);
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.backing.join(ino.to_string()))
            .map_err(errno)
    }
    fn truncate(&mut self, ino: u64, size: u64) -> FsResult<()> {
        self.writable(ino)?;
        if self.node(ino)?.attr.kind != FileType::RegularFile {
            return Err(libc::EINVAL);
        }
        if size > i64::MAX as u64 {
            return Err(libc::EFBIG);
        }
        self.disk_file(ino)?.set_len(size).map_err(errno)?;
        let n = self.node_mut(ino)?;
        n.attr.size = size;
        n.attr.blocks = size.div_ceil(512);
        n.attr.mtime = SystemTime::now();
        n.attr.ctime = n.attr.mtime;
        Ok(())
    }
    fn read_data(&mut self, ino: u64, fh: u64, offset: i64, size: u32) -> FsResult<Vec<u8>> {
        let h = self.handle(ino, fh)?;
        if h.flags & libc::O_ACCMODE == libc::O_WRONLY {
            return Err(libc::EBADF);
        }
        if offset < 0 {
            return Err(libc::EINVAL);
        }
        let n = self.node(ino)?;
        if n.attr.kind != FileType::RegularFile {
            return Err(libc::EISDIR);
        }
        let count = (n.attr.size.saturating_sub(offset as u64)).min(size as u64) as usize;
        if let Some(file) = &n.file {
            let mut data = vec![0; count];
            let mut done = 0;
            while done < count {
                match file.read_at(&mut data[done..], offset as u64 + done as u64) {
                    Ok(0) => break,
                    Ok(n) => done += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(errno(e)),
                }
            }
            data.truncate(done);
            Ok(data)
        } else if count == 0 {
            Ok(Vec::new())
        } else {
            let oid = n.oid.ok_or(libc::EIO)?;
            // Small payload ownership stays exclusively in the bounded LRU;
            // an idle open descriptor must not pin an evicted whole payload.
            if n.attr.size <= BLOB_CACHE_BYTES as u64 {
                return self
                    ._snapshot
                    .read(oid, offset as u64, count as u32)
                    .map_err(|_| libc::EIO);
            }
            if self.handles[&fh].reader.is_none() {
                let reader = self._snapshot.open(oid).map_err(|_| libc::EIO)?;
                self.handles.get_mut(&fh).ok_or(libc::EBADF)?.reader = Some(reader);
            }
            self.handles[&fh]
                .reader
                .as_ref()
                .ok_or(libc::EIO)?
                .read(offset as u64, count as u32)
                .map_err(|_| libc::EIO)
        }
    }
    fn write_data(
        &mut self,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
    ) -> FsResult<u32> {
        let h = self.handle(ino, fh)?;
        if h.flags & libc::O_ACCMODE == libc::O_RDONLY {
            return Err(libc::EBADF);
        }
        self.writable(ino)?;
        if offset < 0 {
            return Err(libc::EINVAL);
        }
        let n = self.node_mut(ino)?;
        // FUSE_WRITE_CACHE identifies dirty mmap pages. Their kernel-selected
        // offsets must never acquire append semantics from the original fd.
        // For ordinary writes use current request flags (fcntl F_SETFL can
        // change them after open), not our handle's initial flags.
        let cached = write_flags & 1 != 0;
        let offset = if !cached && flags & libc::O_APPEND != 0 {
            n.attr.size
        } else {
            offset as u64
        };
        offset
            .checked_add(data.len() as u64)
            .filter(|end| *end <= i64::MAX as u64)
            .ok_or(libc::EFBIG)?;
        let file = n.file.as_ref().ok_or(libc::EINVAL)?;
        if data.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < data.len() {
            match file.write_at(&data[written..], offset + written as u64) {
                Ok(0) => return Err(libc::EIO),
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(errno(e)),
            }
        }
        n.attr.size = n.attr.size.max(offset + written as u64);
        n.attr.blocks = n.attr.size.div_ceil(512);
        n.attr.mtime = SystemTime::now();
        n.attr.ctime = n.attr.mtime;
        if !cached && flags & libc::O_SYNC == libc::O_SYNC {
            file.sync_all().map_err(errno)?;
        } else if !cached && flags & libc::O_DSYNC != 0 {
            file.sync_data().map_err(errno)?;
        }
        Ok(written as u32)
    }
    fn empty(&self, p: &Path) -> bool {
        !self.paths.keys().any(|q| q != p && q.starts_with(p))
    }
    fn remove_path(&mut self, p: &Path) {
        self.upper_paths.remove(p);
        if let Some(ino) = self.paths.remove(p) {
            if let Some(n) = self.nodes.get_mut(&ino) {
                n.attr.nlink = n
                    .attr
                    .nlink
                    .saturating_sub(if n.attr.kind == FileType::Directory {
                        2
                    } else {
                        1
                    });
                n.attr.ctime = SystemTime::now();
                if n.attr.nlink == 0 && n.attr.kind == FileType::RegularFile {
                    let _ = fs::remove_file(self.backing.join(ino.to_string()));
                }
            }
            self.touch_parent(p);
        }
        // Keep inode objects and open descriptors for outstanding kernel inode
        // references and open-unlinked handles; all are reclaimed at unmount.
    }
    fn remove(&mut self, parent: u64, name: &OsStr, directory: bool) -> FsResult<()> {
        let p = self.child(parent, name)?;
        let ino = self.ensure(&p)?;
        self.writable(ino)?;
        let kind = self.node(ino)?.attr.kind;
        if directory && kind != FileType::Directory {
            return Err(libc::ENOTDIR);
        }
        if !directory && kind == FileType::Directory {
            return Err(libc::EISDIR);
        }
        if directory && !self.empty(&p) {
            return Err(libc::ENOTEMPTY);
        }
        self.allowed(&p, kind)?;
        self.remove_path(&p);
        Ok(())
    }
    fn rename_path(
        &mut self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
    ) -> FsResult<()> {
        // Exchange/whiteout would need separate transaction semantics.
        if flags & !1 != 0 {
            return Err(libc::EINVAL);
        } // RENAME_NOREPLACE = 1
        let from = self.child(parent, name)?;
        let to = self.child(newparent, newname)?;
        let ino = self.ensure(&from)?;
        self.writable(ino)?;
        let kind = self.node(ino)?.attr.kind;
        self.allowed(&from, kind)?;
        self.allowed(&to, kind)?;
        if from == to {
            return if flags & 1 != 0 {
                Err(libc::EEXIST)
            } else {
                Ok(())
            };
        }
        if kind == FileType::Directory && to.starts_with(&from) {
            return Err(libc::EINVAL);
        }
        if let Some(dst) = self.paths.get(&to).copied() {
            self.writable(dst)?;
            if flags & 1 != 0 {
                return Err(libc::EEXIST);
            }
            if dst == ino {
                return Ok(());
            }
            let dk = self.node(dst)?.attr.kind;
            if kind == FileType::Directory && dk != FileType::Directory {
                return Err(libc::ENOTDIR);
            }
            if kind != FileType::Directory && dk == FileType::Directory {
                return Err(libc::EISDIR);
            }
            if dk == FileType::Directory && !self.empty(&to) {
                return Err(libc::ENOTEMPTY);
            }
        }
        let mut moves = Vec::new();
        for (old, id) in &self.paths {
            if old == &from || old.starts_with(&from) {
                self.writable(*id)?;
                let target = to.join(old.strip_prefix(&from).map_err(|_| libc::EIO)?);
                let k = self.node(*id)?.attr.kind;
                self.allowed(old, k)?;
                self.allowed(&target, k)?;
                if let Some(other) = self.paths.get(&target) {
                    if target != to && !target.starts_with(&from) && *other != *id {
                        return Err(libc::EEXIST);
                    }
                }
                moves.push((old.clone(), target, *id));
            }
        }
        // Every validation above precedes the first namespace mutation.
        if self.paths.contains_key(&to) {
            self.remove_path(&to);
        }
        for (old, _, _) in &moves {
            self.paths.remove(old);
            self.upper_paths.remove(old);
        }
        for (_, target, id) in moves {
            self.upper_paths.insert(target.clone(), id);
            self.paths.insert(target, id);
        }
        self.node_mut(ino)?.attr.ctime = SystemTime::now();
        self.touch_parent(&from);
        self.touch_parent(&to);
        Ok(())
    }
    fn hardlink(&mut self, ino: u64, parent: u64, name: &OsStr) -> FsResult<()> {
        self.writable(ino)?;
        let kind = self.node(ino)?.attr.kind;
        if kind == FileType::Directory {
            return Err(libc::EPERM);
        }
        let path = self.fresh(parent, name, kind)?;
        let n = self.node_mut(ino)?;
        if n.attr.nlink == 0 {
            return Err(libc::ENOENT);
        }
        n.attr.nlink = n.attr.nlink.checked_add(1).ok_or(libc::EMLINK)?;
        n.attr.ctime = SystemTime::now();
        self.paths.insert(path.clone(), ino);
        self.upper_paths.insert(path.clone(), ino);
        self.touch_parent(&path);
        Ok(())
    }
}

impl Filesystem for Overlay {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let result = (|| {
            let p = self.child(parent, name)?;
            let ino = self.ensure(&p)?;
            let node = self.node_mut(ino)?;
            node.lookups = node.lookups.saturating_add(1);
            Ok(node.attr)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }
    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        self.forget_inode(ino, nlookup);
    }
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        if let Some(fh) = fh {
            if let Err(e) = self.handle(ino, fh) {
                reply.error(e);
                return;
            }
        }
        match self.node(ino) {
            Ok(n) => reply.attr(&TTL, &n.attr),
            Err(e) => reply.error(e),
        }
    }
    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<u64>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
            self.writable(ino)?;
            if let Some(fh) = fh {
                let h = self.handle(ino, fh)?;
                if size.is_some() && h.flags & libc::O_ACCMODE == libc::O_RDONLY {
                    return Err(libc::EBADF);
                }
            }
            let n = self.node(ino)?;
            if req.uid() != 0 && req.uid() != n.attr.uid {
                return Err(libc::EPERM);
            }
            if uid.is_some_and(|u| u != n.attr.uid) || gid.is_some_and(|g| g != n.attr.gid) {
                return Err(libc::EPERM);
            }
            if flags.is_some_and(|f| f != 0) {
                return Err(libc::EOPNOTSUPP);
            }
            if let Some(size) = size {
                self.truncate(ino, size)?;
            }
            let n = self.node_mut(ino)?;
            if let Some(mode) = mode {
                n.attr.perm = (mode & 0o777) as u16;
            }
            let time = |v: TimeOrNow| match v {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => SystemTime::now(),
            };
            if let Some(t) = atime {
                n.attr.atime = time(t);
            }
            if let Some(t) = mtime {
                n.attr.mtime = time(t);
            }
            if let Some(t) = crtime {
                n.attr.crtime = t;
            }
            n.attr.ctime = ctime.unwrap_or_else(SystemTime::now);
            Ok(n.attr)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let result = (|| {
            let n = self.node(ino)?;
            if n.attr.kind != FileType::Symlink {
                return Err(libc::EINVAL);
            }
            if let Some(oid) = n.oid {
                self._snapshot.read_link(oid).map_err(|_| libc::EIO)
            } else {
                Ok(n.bytes.clone())
            }
        })();
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }
    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & libc::S_IFMT != 0 && mode & libc::S_IFMT != libc::S_IFREG {
            reply.error(libc::EPERM);
            return;
        }
        match self.make(
            parent,
            name,
            FileType::RegularFile,
            mode & !umask,
            req.uid(),
            req.gid(),
            Vec::new(),
        ) {
            Ok(ino) => {
                self.nodes.get_mut(&ino).unwrap().lookups += 1;
                reply.entry(&TTL, &self.nodes[&ino].attr, 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self.make(
            parent,
            name,
            FileType::Directory,
            mode & !umask,
            req.uid(),
            req.gid(),
            Vec::new(),
        ) {
            Ok(ino) => {
                self.nodes.get_mut(&ino).unwrap().lookups += 1;
                reply.entry(&TTL, &self.nodes[&ino].attr, 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        if target.as_os_str().as_bytes().contains(&0) {
            reply.error(libc::EINVAL);
            return;
        }
        match self.make(
            parent,
            link_name,
            FileType::Symlink,
            0o777,
            req.uid(),
            req.gid(),
            target.as_os_str().as_bytes().to_vec(),
        ) {
            Ok(ino) => {
                self.nodes.get_mut(&ino).unwrap().lookups += 1;
                reply.entry(&TTL, &self.nodes[&ino].attr, 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        match self.rename_path(parent, name, newparent, newname, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match self.hardlink(ino, newparent, newname) {
            Ok(()) => {
                self.nodes.get_mut(&ino).unwrap().lookups += 1;
                reply.entry(&TTL, &self.nodes[&ino].attr, 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        // No keep-cache or writeback-cache flags: normal page-cache semantics
        // preserve executable mappings and shared mmap write-through handles.
        match self.open_file(ino, flags) {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(e),
        }
    }
    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| {
            let ino = self.make(
                parent,
                name,
                FileType::RegularFile,
                mode & !umask,
                req.uid(),
                req.gid(),
                Vec::new(),
            )?;
            let fh = self.open_file(ino, flags)?;
            Ok((ino, fh))
        })();
        match result {
            Ok((ino, fh)) => {
                self.nodes.get_mut(&ino).unwrap().lookups += 1;
                reply.created(&TTL, &self.nodes[&ino].attr, 0, fh, 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.read_data(ino, fh, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.write_data(ino, fh, offset, data, write_flags, flags) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }
    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.handle(ino, fh) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.handle(ino, fh) {
            Ok(_) => {
                self.close_handle(ino, fh);
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }
    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let result = (|| {
            self.handle(ino, fh)?;
            if let Some(file) = &self.node(ino)?.file {
                if datasync {
                    file.sync_data()
                } else {
                    file.sync_all()
                }
                .map_err(errno)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn opendir(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let result = self.open_directory(ino, flags);
        match result {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(e),
        }
    }
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        if let Err(e) = self.handle(ino, fh) {
            reply.error(e);
            return;
        }
        match self.directories.get(&fh) {
            Some(entries) => {
                for (i, (child, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                    if reply.add(*child, (i + 1) as i64, *kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            None => reply.error(libc::EBADF),
        }
    }
    fn releasedir(&mut self, req: &Request<'_>, ino: u64, fh: u64, flags: i32, reply: ReplyEmpty) {
        self.release(req, ino, fh, flags, None, false, reply)
    }
    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = self
            .handle(ino, fh)
            .and_then(|_| File::open(&self.backing).map_err(errno))
            .and_then(|f| f.sync_all().map_err(errno));
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let path = match std::ffi::CString::new(self.backing.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            reply.error(errno(std::io::Error::last_os_error()));
            return;
        }
        let s = unsafe { stats.assume_init() };
        reply.statfs(
            s.f_blocks,
            s.f_bfree,
            s.f_bavail,
            s.f_files,
            s.f_ffree,
            s.f_bsize as u32,
            255,
            s.f_frsize as u32,
        );
    }
    fn access(&mut self, req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        let result = (|| {
            let n = self.node(ino)?;
            if mask & libc::W_OK != 0 && n.immutable && n.attr.kind != FileType::Directory {
                return Err(libc::EROFS);
            }
            let bits = if req.uid() == n.attr.uid {
                n.attr.perm >> 6
            } else if req.gid() == n.attr.gid {
                n.attr.perm >> 3
            } else {
                n.attr.perm
            };
            if req.uid() == 0 {
                if mask & libc::X_OK != 0
                    && n.attr.kind != FileType::Directory
                    && n.attr.perm & 0o111 == 0
                {
                    return Err(libc::EACCES);
                }
            } else if bits as i32 & mask != mask {
                return Err(libc::EACCES);
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Overlay) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path().join("repo")).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        let ignore = repo
            .blob(b"*.tmp\n/cache/\n/dest/\n!/dest/\n/dest/*\n!/dest/blocked\n")
            .unwrap();
        builder.insert(".gitignore", ignore, 0o100644).unwrap();
        let content = repo.blob(b"pinned content\n").unwrap();
        builder.insert("tracked.tmp", content, 0o100755).unwrap();
        let link = repo.blob(b"tracked.tmp").unwrap();
        builder.insert("link", link, 0o120000).unwrap();
        let tree = builder.write().unwrap();
        let snapshot =
            Snapshot::load(repo.path(), OsStr::new(&tree.to_string()), "fs-tests").unwrap();
        let overlay = Overlay::new(snapshot, temp.path().join("upper")).unwrap();
        (temp, overlay)
    }
    fn file(fs: &mut Overlay, parent: u64, name: &str) -> u64 {
        fs.make(
            parent,
            OsStr::new(name),
            FileType::RegularFile,
            0o600,
            0,
            0,
            Vec::new(),
        )
        .unwrap()
    }
    fn dir(fs: &mut Overlay, name: &str) -> u64 {
        fs.make(
            1,
            OsStr::new(name),
            FileType::Directory,
            0o700,
            0,
            0,
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn snapshot_reads_offsets_tails_and_modes_and_denies_every_mutation() {
        let (_tmp, mut fs) = fixture();
        let ino = fs.ensure(Path::new("tracked.tmp")).unwrap();
        assert_eq!(fs.nodes[&ino].attr.perm, 0o755);
        let fh = fs.open_file(ino, libc::O_RDONLY).unwrap();
        assert_eq!(fs.read_data(ino, fh, 7, 100).unwrap(), b"content\n");
        assert!(fs.read_data(ino, fh, 999, 100).unwrap().is_empty());
        assert_eq!(fs.read_data(ino, fh, -1, 1), Err(libc::EINVAL));
        assert_eq!(fs.open_file(ino, libc::O_RDWR), Err(libc::EROFS));
        assert_eq!(
            fs.open_file(ino, libc::O_RDONLY | libc::O_TRUNC),
            Err(libc::EROFS)
        );
        assert_eq!(fs.truncate(ino, 0), Err(libc::EROFS));
        assert_eq!(
            fs.remove(1, OsStr::new("tracked.tmp"), false),
            Err(libc::EROFS)
        );
        assert_eq!(
            fs.rename_path(1, OsStr::new("tracked.tmp"), 1, OsStr::new("other.tmp"), 0),
            Err(libc::EROFS)
        );
        assert_eq!(
            fs.hardlink(ino, 1, OsStr::new("hard.tmp")),
            Err(libc::EROFS)
        );
        assert_eq!(
            fs.fresh(1, OsStr::new("tracked.tmp"), FileType::RegularFile),
            Err(libc::EEXIST)
        );
        let link = fs.ensure(Path::new("link")).unwrap();
        assert_eq!(
            fs._snapshot
                .read_link(fs.nodes[&link].oid.unwrap())
                .unwrap(),
            b"tracked.tmp"
        );
    }
    #[test]
    fn lazy_overlay_starts_with_only_root_and_reclaims_forgotten_metadata() {
        let (_tmp, mut fs) = fixture();
        assert_eq!(fs.nodes.len(), 1);
        assert_eq!(fs.paths.len(), 1);
        let ino = fs.ensure(Path::new("tracked.tmp")).unwrap();
        assert_eq!(fs.nodes.len(), 2);
        let original_time = fs.nodes[&ino].attr.mtime;
        fs.nodes.get_mut(&ino).unwrap().lookups = 1;
        let handle = fs.open_file(ino, libc::O_RDONLY).unwrap();
        fs.forget_inode(ino, 1);
        assert!(
            fs.nodes.contains_key(&ino),
            "open handle retains forgotten inode"
        );
        assert_eq!(
            fs.read_data(ino, handle, 0, 100).unwrap(),
            b"pinned content\n"
        );
        fs.handles.remove(&handle);
        fs.reclaim(ino);
        assert_eq!(fs.nodes.len(), 1);
        assert!(!fs.paths.contains_key(Path::new("tracked.tmp")));
        let replacement = fs.ensure(Path::new("tracked.tmp")).unwrap();
        assert_ne!(replacement, ino, "never recycle inode IDs");
        assert_eq!(fs.nodes[&replacement].attr.mtime, original_time);
    }
    #[test]
    fn directory_reserved_inodes_are_stable_and_reclaimed() {
        let (_tmp, mut fs) = fixture();
        let first = fs.open_directory(1, libc::O_RDONLY).unwrap();
        let second = fs.open_directory(1, libc::O_RDONLY).unwrap();
        let reserved = fs.paths[Path::new("tracked.tmp")];
        assert_ne!(reserved, 0);
        assert!(!fs.nodes.contains_key(&reserved));
        assert_eq!(fs.ensure(Path::new("tracked.tmp")).unwrap(), reserved);
        fs.nodes.get_mut(&reserved).unwrap().lookups = 1;
        fs.close_handle(1, first);
        assert_eq!(fs.paths[Path::new("tracked.tmp")], reserved);
        fs.close_handle(1, second);
        assert_eq!(fs.paths.len(), 2);
        fs.forget_inode(reserved, 1);
        assert_eq!(fs.paths.len(), 1);
        assert_eq!(fs.nodes.len(), 1);
        assert_eq!(fs.inode_paths.len(), 1);
        assert!(fs.directory_refs.is_empty());
    }
    #[test]
    fn idle_open_descriptors_do_not_pin_evicted_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path().join("repo")).unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        for i in 0..5 {
            let oid = repo.blob(&vec![b'a' + i; 4 * 1024 * 1024]).unwrap();
            tree.insert(format!("file{i}"), oid, 0o100644).unwrap();
        }
        let oid = tree.write().unwrap();
        let snapshot =
            Snapshot::load(repo.path(), OsStr::new(&oid.to_string()), "retention").unwrap();
        let mut fs = Overlay::new(snapshot, temp.path().join("upper")).unwrap();
        for i in 0..5 {
            let ino = fs.ensure(Path::new(&format!("file{i}"))).unwrap();
            let fh = fs.open_file(ino, libc::O_RDONLY).unwrap();
            assert_eq!(fs.read_data(ino, fh, 0, 1).unwrap(), [b'a' + i]);
        }
        assert_eq!(fs.handles.len(), 5);
        assert!(fs.handles.values().all(|h| h.reader.is_none()));
        let metrics = fs._snapshot.metrics();
        assert!(metrics.blob_cache_bytes <= BLOB_CACHE_BYTES);
        assert_eq!(metrics.active_blob_readers, 0);
    }
    #[test]
    fn never_looked_up_tracked_paths_cannot_be_shadowed_or_removed() {
        let (_tmp, mut fs) = fixture();
        assert_eq!(fs.paths.len(), 1);
        assert_eq!(
            fs.fresh(1, OsStr::new("tracked.tmp"), FileType::RegularFile),
            Err(libc::EEXIST)
        );
        assert_eq!(
            fs.remove(1, OsStr::new("tracked.tmp"), false),
            Err(libc::EROFS)
        );
        let source = file(&mut fs, 1, "source.tmp");
        assert_eq!(
            fs.rename_path(1, OsStr::new("source.tmp"), 1, OsStr::new("tracked.tmp"), 0),
            Err(libc::EROFS)
        );
        assert!(fs.nodes.contains_key(&source));
    }
    #[test]
    fn writable_files_support_offsets_holes_truncate_append_and_unlink() {
        let (_tmp, mut fs) = fixture();
        let ino = file(&mut fs, 1, "new.tmp");
        let fh = fs.open_file(ino, libc::O_RDWR).unwrap();
        assert_eq!(fs.write_data(ino, fh, 3, b"abc", 0, libc::O_RDWR), Ok(3));
        assert_eq!(fs.read_data(ino, fh, 0, 20).unwrap(), b"\0\0\0abc");
        fs.truncate(ino, 4).unwrap();
        assert_eq!(fs.read_data(ino, fh, 0, 20).unwrap(), b"\0\0\0a");
        let app = fs.open_file(ino, libc::O_WRONLY | libc::O_APPEND).unwrap();
        fs.write_data(ino, app, 0, b"tail", 0, libc::O_WRONLY | libc::O_APPEND)
            .unwrap();
        assert_eq!(fs.read_data(ino, fh, 3, 20).unwrap(), b"atail");
        fs.remove(1, OsStr::new("new.tmp"), false).unwrap();
        assert!(!fs.backing.join(ino.to_string()).exists());
        fs.write_data(ino, fh, 0, b"alive", 0, libc::O_RDWR)
            .unwrap();
        assert_eq!(fs.read_data(ino, fh, 0, 5).unwrap(), b"alive");
    }
    #[test]
    fn mmap_writeback_preserves_offsets_on_append_handles() {
        let (_tmp, mut fs) = fixture();
        let ino = file(&mut fs, 1, "mapped.tmp");
        let fh = fs.open_file(ino, libc::O_RDWR | libc::O_APPEND).unwrap();
        fs.write_data(ino, fh, 0, b"original", 0, libc::O_RDWR | libc::O_APPEND)
            .unwrap();
        fs.write_data(ino, fh, 0, b"M", 1, libc::O_RDWR | libc::O_APPEND)
            .unwrap();
        assert_eq!(fs.read_data(ino, fh, 0, 100).unwrap(), b"Mriginal");
        assert_eq!(fs.nodes[&ino].attr.size, 8);
    }
    #[test]
    fn current_request_flags_override_initial_append_status() {
        let (_tmp, mut fs) = fixture();
        let ino = file(&mut fs, 1, "flags.tmp");
        let fh = fs.open_file(ino, libc::O_RDWR | libc::O_APPEND).unwrap();
        fs.write_data(ino, fh, 0, b"original", 0, libc::O_RDWR | libc::O_APPEND)
            .unwrap();
        // fcntl(F_SETFL): append removed after opening.
        fs.write_data(ino, fh, 0, b"N", 0, libc::O_RDWR).unwrap();
        assert_eq!(fs.read_data(ino, fh, 0, 100).unwrap(), b"Nriginal");
        let nonappend = fs.open_file(ino, libc::O_RDWR).unwrap();
        // fcntl(F_SETFL): append enabled after opening.
        fs.write_data(ino, nonappend, 0, b"+", 0, libc::O_RDWR | libc::O_APPEND)
            .unwrap();
        assert_eq!(fs.read_data(ino, fh, 0, 100).unwrap(), b"Nriginal+");
    }
    #[test]
    fn ignored_atomic_rename_and_hardlinks_preserve_inode_identity() {
        let (_tmp, mut fs) = fixture();
        let cache = dir(&mut fs, "cache");
        let old = file(&mut fs, cache, "old");
        let fh = fs.open_file(old, libc::O_RDWR).unwrap();
        fs.write_data(old, fh, 0, b"old", 0, libc::O_RDWR).unwrap();
        let new = file(&mut fs, cache, "atomic");
        fs.rename_path(cache, OsStr::new("atomic"), cache, OsStr::new("old"), 0)
            .unwrap();
        assert_eq!(fs.paths[Path::new("cache/old")], new);
        assert_eq!(fs.read_data(old, fh, 0, 20).unwrap(), b"old");
        fs.hardlink(new, cache, OsStr::new("alias")).unwrap();
        assert_eq!(fs.nodes[&new].attr.nlink, 2);
        fs.remove(cache, OsStr::new("old"), false).unwrap();
        assert_eq!(fs.nodes[&new].attr.nlink, 1);
        assert!(fs.backing.join(new.to_string()).exists());
    }
    #[test]
    fn recursive_rename_checks_every_destination_before_mutation() {
        let (_tmp, mut fs) = fixture();
        let cache = dir(&mut fs, "cache");
        file(&mut fs, cache, "blocked");
        assert_eq!(
            fs.rename_path(1, OsStr::new("cache"), 1, OsStr::new("dest"), 0),
            Err(libc::EROFS)
        );
        assert!(fs.paths.contains_key(Path::new("cache/blocked")));
        assert!(!fs.paths.contains_key(Path::new("dest")));
        assert_eq!(
            fs.remove(1, OsStr::new("cache"), true),
            Err(libc::ENOTEMPTY)
        );
        assert_eq!(
            fs.rename_path(1, OsStr::new("cache"), cache, OsStr::new("nested"), 0),
            Err(libc::EINVAL)
        );
    }
    #[test]
    fn no_unignored_paths_or_snapshot_shadowing_or_symlink_traversal() {
        let (_tmp, mut fs) = fixture();
        assert_eq!(
            fs.make(
                1,
                OsStr::new("source.rs"),
                FileType::RegularFile,
                0o600,
                0,
                0,
                Vec::new()
            ),
            Err(libc::EROFS)
        );
        let tmp = file(&mut fs, 1, "a.tmp");
        assert_eq!(
            fs.rename_path(1, OsStr::new("a.tmp"), 1, OsStr::new("tracked.tmp"), 0),
            Err(libc::EROFS)
        );
        assert_eq!(
            fs.hardlink(tmp, 1, OsStr::new("source.rs")),
            Err(libc::EROFS)
        );
        let link = fs
            .make(
                1,
                OsStr::new("evil.tmp"),
                FileType::Symlink,
                0o777,
                0,
                0,
                b"/tmp".to_vec(),
            )
            .unwrap();
        assert_eq!(fs.child(link, OsStr::new("outside")), Err(libc::ENOTDIR));
        assert_eq!(fs.child(1, OsStr::new("../outside")), Err(libc::EINVAL));
        assert_eq!(fs.child(1, OsStr::new("/absolute")), Err(libc::EINVAL));
        assert_eq!(fs.child(1, OsStr::new("..")), Err(libc::EINVAL));
    }
}
