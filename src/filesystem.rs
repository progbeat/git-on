//! An inode-based union of a pinned Git snapshot and a private, disposable
//! writable layer. During a mount, user-controlled names never become host
//! filesystem paths. All execution state is disposable.
use crate::backend::{FileAttr, FileType, SetAttributes, Statistics, TimeOrNow};
use crate::policy::{Policy, WritePolicy};
use crate::snapshot::{BlobReader, EntryKind, Snapshot, BLOB_CACHE_BYTES};
use anyhow::{Context, Result};
use git2::Oid;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) type FsResult<T> = std::result::Result<T, i32>;
fn errno(e: std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}
fn snapshot_errno(error: anyhow::Error) -> i32 {
    if crate::git::is_request_interruption(&error) {
        libc::EINTR
    } else {
        libc::EIO
    }
}

struct Node {
    attr: FileAttr,
    immutable: bool,
    oid: Option<Oid>,
    lookups: u64,
    snapshot_path: Option<PathBuf>,
    bytes: Vec<u8>,
    file: Option<File>,
    external_io: bool,
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
    write_policy: WritePolicy,
    backing: PathBuf,
    paths: BTreeMap<PathBuf, u64>,
    inode_paths: HashMap<u64, PathBuf>,
    upper_paths: BTreeMap<PathBuf, u64>,
    // Namespace changes stay separate from the pinned source. Directory
    // redirects preserve lazy access to unvisited descendants after rename.
    whiteouts: BTreeSet<PathBuf>,
    redirects: BTreeMap<PathBuf, PathBuf>,
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
        Self::with_write_policy(snapshot, backing, WritePolicy::Ignored)
    }
    pub fn with_write_policy(
        snapshot: Snapshot,
        backing: PathBuf,
        write_policy: WritePolicy,
    ) -> Result<Self> {
        fs::create_dir_all(&backing).context("create writable backing directory")?;
        let meta = fs::symlink_metadata(&backing)?;
        anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "backing must be a real directory"
        );
        fs::set_permissions(&backing, fs::Permissions::from_mode(0o700))?;
        snapshot.set_spool_directory(&backing)?;
        let policy = Policy::with_mode(&snapshot, write_policy)?;
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
                external_io: false,
            },
        );
        paths.insert(PathBuf::new(), 1);
        Ok(Self {
            _snapshot: snapshot,
            policy,
            write_policy,
            backing,
            paths,
            inode_paths: HashMap::from([(1, PathBuf::new())]),
            upper_paths: BTreeMap::new(),
            whiteouts: BTreeSet::new(),
            redirects: BTreeMap::new(),
            directory_refs: HashMap::new(),
            nodes,
            handles: HashMap::new(),
            directories: HashMap::new(),
            next_ino: 2,
            next_handle: 1,
            snapshot_time,
        })
    }
    fn lower_path(&self, path: &Path) -> Option<PathBuf> {
        for ancestor in path.ancestors() {
            if let Some(source) = self.redirects.get(ancestor) {
                return Some(source.join(path.strip_prefix(ancestor).ok()?));
            }
            if self.whiteouts.contains(ancestor) {
                return None;
            }
        }
        Some(path.to_path_buf())
    }
    /// Materialize inode metadata only for the requested pathname. No sibling
    /// blobs or descendant trees are loaded as a side effect.
    fn ensure(&mut self, path: &Path) -> FsResult<u64> {
        if let Some(ino) = self.paths.get(path) {
            if self.nodes.contains_key(ino) {
                return Ok(*ino);
            }
        }
        let source = self.lower_path(path).ok_or(libc::ENOENT)?;
        let entry = self
            ._snapshot
            .entry(&source)
            .map_err(snapshot_errno)?
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
                snapshot_path: Some(source),
                bytes: Vec::new(),
                file: None,
                external_io: false,
            },
        );
        self.paths.insert(path.to_path_buf(), ino);
        self.inode_paths.insert(ino, path.to_path_buf());
        Ok(ino)
    }
    pub(crate) fn forget_inode(&mut self, ino: u64, count: u64) {
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
    pub(crate) fn open_directory(&mut self, ino: u64, flags: i32) -> FsResult<u64> {
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
        if let Some(source) = self.node(ino)?.snapshot_path.clone() {
            for (name, entry) in self._snapshot.children(&source).map_err(snapshot_errno)? {
                let child_path = path.join(&name);
                if self.upper_paths.contains_key(&child_path)
                    || self.lower_path(&child_path).is_none()
                {
                    continue;
                }
                let kind = match entry.kind {
                    EntryKind::Directory => FileType::Directory,
                    EntryKind::Symlink => FileType::Symlink,
                    EntryKind::File => FileType::RegularFile,
                };
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
        let _ = self.refresh_backing_attr(ino);
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
        self.node(ino)?;
        if let Some(path) = self
            .inode_paths
            .get(&ino)
            .filter(|path| self.paths.get(*path) == Some(&ino))
        {
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
        let node = self.node(ino)?;
        if self.write_policy == WritePolicy::None
            || (node.immutable && self.write_policy != WritePolicy::All)
        {
            Err(libc::EROFS)
        } else {
            Ok(())
        }
    }
    /// Copy only this inode on its first mutation. A directory retains its
    /// source origin so its untouched descendants continue to load on demand.
    fn copy_up(&mut self, ino: u64, preserve_content: bool) -> FsResult<()> {
        self.writable(ino)?;
        let node = self.node(ino)?;
        if !node.immutable {
            return Ok(());
        }
        let kind = node.attr.kind;
        let oid = node.oid.ok_or(libc::EIO)?;
        let unlinked = node.attr.nlink == 0;
        let path = match self.path(ino) {
            Ok(path) => Some(path),
            Err(libc::ENOENT) if unlinked => None,
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        let file = if kind == FileType::RegularFile {
            let disk_path = self.backing.join(ino.to_string());
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&disk_path)
                .map_err(errno)?;
            let copied = (|| {
                if preserve_content && node.attr.size != 0 {
                    let reader = self._snapshot.open(oid).map_err(snapshot_errno)?;
                    let mut offset = 0;
                    while offset < node.attr.size {
                        let data = reader
                            .read(offset, (node.attr.size - offset).min(64 * 1024) as u32)
                            .map_err(snapshot_errno)?;
                        if data.is_empty() {
                            return Err(libc::EIO);
                        }
                        std::io::Write::write_all(&mut file, &data).map_err(errno)?;
                        offset += data.len() as u64;
                    }
                }
                Ok(())
            })();
            if let Err(error) = copied {
                drop(file);
                let _ = fs::remove_file(disk_path);
                return Err(error);
            }
            if unlinked {
                fs::remove_file(&disk_path).map_err(errno)?;
            }
            Some(file)
        } else {
            if kind == FileType::Symlink {
                bytes = self._snapshot.read_link(oid).map_err(snapshot_errno)?;
            }
            None
        };
        let keep_file = unlinked || self.handles.values().any(|handle| handle.ino == ino);
        let node = self.node_mut(ino)?;
        node.immutable = false;
        node.oid = None;
        node.bytes = bytes;
        if keep_file {
            node.file = file;
        }
        if let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) {
            self.upper_paths.insert(path, ino);
        }
        for handle in self.handles.values_mut().filter(|handle| handle.ino == ino) {
            handle.reader = None;
        }
        Ok(())
    }
    fn allowed(&self, p: &Path, kind: FileType) -> FsResult<()> {
        if self
            .policy
            .check(p, kind == FileType::Directory)
            .map_err(snapshot_errno)?
        {
            Ok(())
        } else {
            Err(libc::EROFS)
        }
    }
    fn fresh(&self, parent: u64, name: &OsStr, kind: FileType) -> FsResult<PathBuf> {
        let p = self.child(parent, name)?;
        let tracked = match self.lower_path(&p) {
            Some(source) => self
                ._snapshot
                .entry(&source)
                .map_err(snapshot_errno)?
                .is_some(),
            None => false,
        };
        if self.paths.contains_key(&p) || tracked {
            return Err(libc::EEXIST);
        }
        self.allowed(&p, kind)?;
        Ok(p)
    }
    fn adjust_parent_links(&mut self, path: &Path, delta: i32) {
        if let Some(ino) = path.parent().and_then(|p| self.paths.get(p)).copied() {
            if let Some(node) = self.nodes.get_mut(&ino) {
                if !node.immutable || self.write_policy == WritePolicy::All {
                    node.attr.nlink = node.attr.nlink.saturating_add_signed(delta);
                }
            }
        }
    }
    fn touch_parent(&mut self, path: &Path) {
        if let Some(ino) = path.parent().and_then(|p| self.paths.get(p)).copied() {
            // A tracked directory's metadata belongs to the snapshot as well.
            if let Some(node) = self.nodes.get_mut(&ino) {
                if self.write_policy == WritePolicy::All && node.immutable {
                    node.immutable = false;
                    node.oid = None;
                    let parent = path.parent().expect("namespace path has parent");
                    if !parent.as_os_str().is_empty() {
                        self.upper_paths.insert(parent.to_path_buf(), ino);
                    }
                }
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
                external_io: false,
            },
        );
        self.paths.insert(p.clone(), ino);
        self.upper_paths.insert(p.clone(), ino);
        if kind == FileType::Directory {
            self.adjust_parent_links(&p, 1);
        }
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
    fn refresh_backing_attr(&mut self, ino: u64) -> FsResult<()> {
        {
            if self.node(ino)?.external_io {
                let metadata = self.disk_file(ino)?.metadata().map_err(errno)?;
                let node = self.node_mut(ino)?;
                node.attr.size = metadata.len();
                node.attr.blocks = metadata.blocks();
                node.attr.atime = metadata.accessed().map_err(errno)?;
                node.attr.mtime = metadata.modified().map_err(errno)?;
                let backing_ctime = SystemTime::UNIX_EPOCH
                    .checked_add(Duration::new(
                        metadata.ctime().max(0) as u64,
                        metadata.ctime_nsec() as u32,
                    ))
                    .ok_or(libc::EOVERFLOW)?;
                // The numeric backing path does not change for overlay links
                // and renames. Preserve their later logical namespace changes.
                node.attr.ctime = node.attr.ctime.max(backing_ctime);
            }
        }
        Ok(())
    }
    pub(crate) fn open_file(&mut self, ino: u64, flags: i32) -> FsResult<u64> {
        self.refresh_backing_attr(ino)?;
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
        } else if flags & libc::O_ACCMODE != libc::O_RDONLY {
            self.copy_up(ino, true)?;
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
    fn record_data_change(attr: &mut FileAttr, metadata: &fs::Metadata) {
        attr.size = metadata.len();
        attr.blocks = metadata.blocks();
        attr.mtime = SystemTime::now();
        attr.ctime = attr.mtime;
    }
    fn truncate(&mut self, ino: u64, size: u64) -> FsResult<()> {
        self.writable(ino)?;
        if self.node(ino)?.attr.kind != FileType::RegularFile {
            return Err(libc::EINVAL);
        }
        if size > i64::MAX as u64 {
            return Err(libc::EFBIG);
        }
        self.copy_up(ino, size != 0)?;
        let file = self.disk_file(ino)?;
        file.set_len(size).map_err(errno)?;
        let metadata = file.metadata().map_err(errno)?;
        let n = self.node_mut(ino)?;
        Self::record_data_change(&mut n.attr, &metadata);
        Ok(())
    }
    pub(crate) fn read_data(
        &mut self,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
    ) -> FsResult<Vec<u8>> {
        let h = self.handle(ino, fh)?;
        if h.flags & libc::O_ACCMODE == libc::O_WRONLY {
            return Err(libc::EBADF);
        }
        if offset < 0 {
            return Err(libc::EINVAL);
        }
        self.refresh_backing_attr(ino)?;
        #[cfg(feature = "test-metrics")]
        if self.node(ino)?.snapshot_path.is_none() {
            self._snapshot.record_generated_io(false);
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
                    .map_err(snapshot_errno);
            }
            if self.handles[&fh].reader.is_none() {
                let reader = self._snapshot.open(oid).map_err(snapshot_errno)?;
                self.handles.get_mut(&fh).ok_or(libc::EBADF)?.reader = Some(reader);
            }
            self.handles[&fh]
                .reader
                .as_ref()
                .ok_or(libc::EIO)?
                .read(offset as u64, count as u32)
                .map_err(snapshot_errno)
        }
    }
    pub(crate) fn write_data(
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
        self.refresh_backing_attr(ino)?;
        #[cfg(feature = "test-metrics")]
        if self.node(ino)?.snapshot_path.is_none() {
            self._snapshot.record_generated_io(true);
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
                Ok(0) if written == 0 => return Err(libc::EIO),
                Ok(0) => break,
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if written == 0 => return Err(errno(e)),
                // A short backing write followed by ENOSPC/EFBIG still changed
                // the file. Report that progress and refresh visible metadata;
                // a subsequent write can report the underlying capacity error.
                Err(_) => break,
            }
        }
        let metadata = file.metadata().map_err(errno)?;
        Self::record_data_change(&mut n.attr, &metadata);
        if !cached && flags & libc::O_SYNC == libc::O_SYNC {
            file.sync_all().map_err(errno)?;
        } else if !cached && flags & libc::O_DSYNC != 0 {
            file.sync_data().map_err(errno)?;
        }
        Ok(written as u32)
    }
    fn empty(&self, p: &Path) -> FsResult<bool> {
        if self.paths.keys().any(|q| q != p && q.starts_with(p)) {
            return Ok(false);
        }
        if let Some(source) = self.lower_path(p) {
            if self
                ._snapshot
                .entry(&source)
                .map_err(snapshot_errno)?
                .is_some()
            {
                for (name, _) in self._snapshot.children(&source).map_err(snapshot_errno)? {
                    if self.lower_path(&p.join(name)).is_some() {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
    fn remove_path(&mut self, p: &Path) {
        self.upper_paths.remove(p);
        if self.write_policy == WritePolicy::All {
            self.whiteouts.insert(p.to_path_buf());
            self.redirects.remove(p);
        }
        if let Some(ino) = self.paths.remove(p) {
            if self
                .nodes
                .get(&ino)
                .is_some_and(|n| n.attr.kind == FileType::Directory)
            {
                self.adjust_parent_links(p, -1);
            }
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
            if self.inode_paths.get(&ino).is_some_and(|path| path == p) {
                self.inode_paths.remove(&ino);
            }
        }
        // Keep inode objects and open descriptors for outstanding kernel inode
        // references and open-unlinked handles; all are reclaimed at unmount.
    }
    pub(crate) fn remove(&mut self, parent: u64, name: &OsStr, directory: bool) -> FsResult<()> {
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
        if directory && !self.empty(&p)? {
            return Err(libc::ENOTEMPTY);
        }
        self.allowed(&p, kind)?;
        self.remove_path(&p);
        Ok(())
    }
    pub(crate) fn rename_path(
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
        let destination = match self.ensure(&to) {
            Ok(destination) => Some(destination),
            Err(libc::ENOENT) => None,
            Err(error) => return Err(error),
        };
        if let Some(dst) = destination {
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
            if dk == FileType::Directory && !self.empty(&to)? {
                return Err(libc::ENOTEMPTY);
            }
        }
        let cached: Vec<_> = self
            .paths
            .keys()
            .filter(|old| *old == &from || old.starts_with(&from))
            .cloned()
            .collect();
        let mut moves = Vec::new();
        for old in cached {
            let id = self.paths[&old];
            let target = to.join(old.strip_prefix(&from).map_err(|_| libc::EIO)?);
            if let Some(node) = self.nodes.get(&id) {
                let k = node.attr.kind;
                self.writable(id)?;
                self.allowed(&old, k)?;
                self.allowed(&target, k)?;
            } else if self.write_policy != WritePolicy::All {
                return Err(libc::EIO);
            }
            if let Some(other) = self.paths.get(&target) {
                if target != to && !target.starts_with(&from) && *other != id {
                    return Err(libc::EEXIST);
                }
            }
            moves.push((old, target, id));
        }
        let origin = if kind == FileType::Directory {
            self.node(ino)?.snapshot_path.clone()
        } else {
            None
        };
        // Every namespace validation precedes copy-up or a visible mutation.
        self.copy_up(ino, true)?;
        if destination.is_some() {
            self.remove_path(&to);
        }
        if self.write_policy == WritePolicy::All {
            let moved_whiteouts: Vec<_> = self
                .whiteouts
                .iter()
                .filter(|path| *path != &from && path.starts_with(&from))
                .map(|path| to.join(path.strip_prefix(&from).expect("whiteout descendant")))
                .collect();
            let moved_redirects: Vec<_> = self
                .redirects
                .iter()
                .filter(|(path, _)| *path != &from && path.starts_with(&from))
                .map(|(path, source)| {
                    (
                        to.join(path.strip_prefix(&from).expect("redirect descendant")),
                        source.clone(),
                    )
                })
                .collect();
            self.whiteouts
                .retain(|path| !path.starts_with(&from) && !path.starts_with(&to));
            self.redirects
                .retain(|path, _| !path.starts_with(&from) && !path.starts_with(&to));
            self.whiteouts.insert(from.clone());
            self.whiteouts.extend(moved_whiteouts);
            self.redirects.extend(moved_redirects);
            if let Some(origin) = origin {
                self.redirects.insert(to.clone(), origin);
            } else {
                // A generated directory must not inherit an overwritten lower
                // destination's hidden children.
                self.whiteouts.insert(to.clone());
            }
        }
        for (old, _, _) in &moves {
            self.paths.remove(old);
            self.upper_paths.remove(old);
        }
        for (_, target, id) in moves {
            if self.nodes.get(&id).is_some_and(|node| !node.immutable) {
                self.upper_paths.insert(target.clone(), id);
            }
            self.inode_paths.insert(id, target.clone());
            self.paths.insert(target, id);
        }
        self.node_mut(ino)?.attr.ctime = SystemTime::now();
        if kind == FileType::Directory && from.parent() != to.parent() {
            self.adjust_parent_links(&from, -1);
            self.adjust_parent_links(&to, 1);
        }
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
        if self.node(ino)?.attr.nlink == 0 {
            return Err(libc::ENOENT);
        }
        self.refresh_backing_attr(ino)?;
        self.copy_up(ino, true)?;
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

impl Overlay {
    pub(crate) fn lookup_entry(&mut self, parent: u64, name: &OsStr) -> FsResult<FileAttr> {
        let p = self.child(parent, name)?;
        let ino = self.ensure(&p)?;
        self.refresh_backing_attr(ino)?;
        let node = self.node_mut(ino)?;
        node.lookups = node.lookups.saturating_add(1);
        Ok(node.attr)
    }
    pub(crate) fn attributes(&mut self, ino: u64, fh: Option<u64>) -> FsResult<FileAttr> {
        if let Some(fh) = fh {
            self.handle(ino, fh)?;
        }
        self.refresh_backing_attr(ino)?;
        Ok(self.node(ino)?.attr)
    }
    pub(crate) fn set_attributes(
        &mut self,
        ino: u64,
        caller_uid: u32,
        update: SetAttributes,
    ) -> FsResult<FileAttr> {
        let SetAttributes {
            mode,
            owner: uid,
            group: gid,
            size,
            atime,
            mtime,
            ctime,
            fh,
            crtime,
            flags,
        } = update;

        self.writable(ino)?;
        if let Some(fh) = fh {
            let h = self.handle(ino, fh)?;
            if size.is_some() && h.flags & libc::O_ACCMODE == libc::O_RDONLY {
                return Err(libc::EBADF);
            }
        }
        let n = self.node(ino)?;
        if caller_uid != 0 && caller_uid != n.attr.uid {
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
        } else {
            self.copy_up(ino, true)?;
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
        if n.attr.kind == FileType::RegularFile && (atime.is_some() || mtime.is_some()) {
            // Unspecified timestamps stay untouched even when a bypass write
            // has made the cached attributes stale since the last callback.
            let mut times = std::fs::FileTimes::new();
            if atime.is_some() {
                times = times.set_accessed(n.attr.atime);
            }
            if mtime.is_some() {
                times = times.set_modified(n.attr.mtime);
            }
            self.disk_file(ino)?.set_times(times).map_err(errno)?;
        }
        self.refresh_backing_attr(ino)?;
        Ok(self.node(ino)?.attr)
    }
    pub(crate) fn read_link(&mut self, ino: u64) -> FsResult<Vec<u8>> {
        let n = self.node(ino)?;
        if n.attr.kind != FileType::Symlink {
            return Err(libc::EINVAL);
        }
        if let Some(oid) = n.oid {
            self._snapshot.read_link(oid).map_err(snapshot_errno)
        } else {
            Ok(n.bytes.clone())
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_node(
        &mut self,
        parent: u64,
        name: &OsStr,
        kind: FileType,
        mode: u32,
        uid: u32,
        gid: u32,
        data: Vec<u8>,
    ) -> FsResult<FileAttr> {
        if data.contains(&0) && kind == FileType::Symlink {
            return Err(libc::EINVAL);
        }
        let ino = self.make(parent, name, kind, mode, uid, gid, data)?;
        self.node_mut(ino)?.lookups += 1;
        Ok(self.node(ino)?.attr)
    }
    pub(crate) fn link_node(&mut self, ino: u64, parent: u64, name: &OsStr) -> FsResult<FileAttr> {
        self.hardlink(ino, parent, name)?;
        self.node_mut(ino)?.lookups += 1;
        Ok(self.node(ino)?.attr)
    }
    pub(crate) fn entries(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
    ) -> FsResult<&[(u64, FileType, OsString)]> {
        self.handle(ino, fh)?;
        if offset > i64::MAX as u64 {
            return Err(libc::EINVAL);
        }
        let entries = self.directories.get(&fh).ok_or(libc::EBADF)?;
        Ok(entries
            .get(usize::try_from(offset).map_err(|_| libc::EINVAL)?..)
            .unwrap_or(&[]))
    }
    pub(crate) fn sync_file(&mut self, ino: u64, fh: u64, datasync: bool) -> FsResult<()> {
        #[cfg(feature = "test-metrics")]
        self._snapshot.record_file_sync();

        self.handle(ino, fh)?;
        #[cfg(feature = "test-metrics")]
        if std::env::var_os("GIT_ON_TEST_FSYNC_ERROR").as_deref() == Some(OsStr::new("1")) {
            return Err(libc::EIO);
        }
        if let Some(file) = &self.node(ino)?.file {
            if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            }
            .map_err(errno)?;
        }
        Ok(())
    }
    pub(crate) fn sync_directory(&self, ino: u64, fh: u64) -> FsResult<()> {
        self.handle(ino, fh)?;
        File::open(&self.backing)
            .map_err(errno)?
            .sync_all()
            .map_err(errno)
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn validate_handle(&self, ino: u64, fh: u64) -> FsResult<()> {
        self.handle(ino, fh).map(|_| ())
    }
    pub(crate) fn release_handle(&mut self, ino: u64, fh: u64) -> FsResult<()> {
        self.handle(ino, fh)?;
        self.close_handle(ino, fh);
        Ok(())
    }
    pub(crate) fn check_access(&self, ino: u64, uid: u32, gid: u32, mask: i32) -> FsResult<()> {
        let n = self.node(ino)?;
        if mask & libc::W_OK != 0
            && (self.write_policy == WritePolicy::None
                || (matches!(
                    self.write_policy,
                    WritePolicy::Ignored | WritePolicy::Untracked
                ) && n.immutable
                    && n.attr.kind != FileType::Directory))
        {
            return Err(libc::EROFS);
        }
        let bits = if uid == n.attr.uid {
            n.attr.perm >> 6
        } else if gid == n.attr.gid {
            n.attr.perm >> 3
        } else {
            n.attr.perm
        };
        if uid == 0 {
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
    }
    pub(crate) fn statistics(&self) -> FsResult<Statistics> {
        let path = std::ffi::CString::new(self.backing.as_os_str().as_bytes())
            .map_err(|_| libc::EINVAL)?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(errno(std::io::Error::last_os_error()));
        }
        let s = unsafe { stats.assume_init() };
        #[allow(clippy::useless_conversion)]
        Ok(Statistics {
            blocks: u64::from(s.f_blocks),
            free: u64::from(s.f_bfree),
            available: u64::from(s.f_bavail),
            files: u64::from(s.f_files),
            free_files: u64::from(s.f_ffree),
            block_size: s.f_bsize as u32,
            fragment_size: s.f_frsize as u32,
        })
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn eligible_backing(&self, ino: u64) -> Option<&File> {
        let node = self.nodes.get(&ino)?;
        if node.snapshot_path.is_some() || node.attr.kind != FileType::RegularFile {
            return None;
        }
        node.file.as_ref()
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn enable_external_io(&mut self, ino: u64) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.external_io = true;
        }
    }
    #[cfg(all(target_os = "linux", feature = "test-metrics"))]
    pub(crate) fn record_passthrough(&self) {
        self._snapshot.record_passthrough();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::TempDir;

    #[test]
    fn missing_ignore_object_is_io_error_for_file_and_directory_creation() {
        let (temp, mut overlay) = fixture();
        let repo = Repository::open(temp.path().join("repo")).unwrap();
        let tree = repo.find_tree(overlay._snapshot.tree_oid).unwrap();
        let ignore = tree.get_name(".gitignore").unwrap().id().to_string();
        // Remove the actual loose object before policy demand, without replacing
        // the object loader or injecting an artificial policy error.
        fs::remove_file(
            repo.path()
                .join("objects")
                .join(&ignore[..2])
                .join(&ignore[2..]),
        )
        .unwrap();
        let outcomes = [
            ("new.tmp", FileType::RegularFile),
            ("cache", FileType::Directory),
        ]
        .map(|(name, kind)| {
            let outcome = overlay.make(1, OsStr::new(name), kind, 0o700, 0, 0, Vec::new());
            assert!(!overlay.paths.contains_key(Path::new(name)));
            outcome
        });
        assert_eq!(outcomes, [Err(libc::EIO), Err(libc::EIO)]);
        assert_eq!(overlay.nodes.len(), 1);
        assert_eq!(fs::read_dir(&overlay.backing).unwrap().count(), 0);
    }

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
    fn fixture_policy(mode: WritePolicy) -> (TempDir, Overlay) {
        let (temp, overlay) = fixture();
        let snapshot = overlay._snapshot.clone();
        drop(overlay);
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), mode).unwrap();
        (temp, overlay)
    }
    #[test]
    fn all_copies_up_tracked_bytes_and_preserves_existing_read_handles() {
        let (_temp, mut overlay) = fixture_policy(WritePolicy::All);
        let ino = overlay.ensure(Path::new("tracked.tmp")).unwrap();
        let original_oid = overlay.nodes[&ino].oid.unwrap();
        let read = overlay.open_file(ino, libc::O_RDONLY).unwrap();
        assert_eq!(
            overlay.read_data(ino, read, 0, 100).unwrap(),
            b"pinned content\n"
        );
        let write = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay
            .write_data(ino, write, 0, b"edited", 0, libc::O_RDWR)
            .unwrap();
        assert_eq!(
            overlay.read_data(ino, read, 0, 100).unwrap(),
            b"edited content\n"
        );
        overlay
            .rename_path(1, OsStr::new("tracked.tmp"), 1, OsStr::new("renamed"), 0)
            .unwrap();
        assert_eq!(overlay.ensure(Path::new("tracked.tmp")), Err(libc::ENOENT));
        assert_eq!(overlay.ensure(Path::new("renamed")).unwrap(), ino);
        assert_eq!(
            overlay.read_data(ino, read, 0, 100).unwrap(),
            b"edited content\n"
        );
        assert_eq!(
            overlay._snapshot.read(original_oid, 0, 100).unwrap(),
            b"pinned content\n"
        );
        overlay.remove(1, OsStr::new("renamed"), false).unwrap();
        assert_eq!(overlay.ensure(Path::new("renamed")), Err(libc::ENOENT));
        assert_eq!(
            overlay.read_data(ino, read, 0, 100).unwrap(),
            b"edited content\n"
        );
        let new = file(&mut overlay, 1, "tracked.tmp");
        assert_ne!(new, ino);
    }
    #[test]
    fn none_denies_ignored_creation_without_loading_rules() {
        let (_temp, mut overlay) = fixture_policy(WritePolicy::None);
        let before = overlay._snapshot.metrics();
        assert_eq!(
            overlay.make(
                1,
                OsStr::new("fresh.tmp"),
                FileType::RegularFile,
                0o600,
                0,
                0,
                Vec::new()
            ),
            Err(libc::EROFS)
        );
        assert_eq!(overlay._snapshot.metrics().blob_loads, before.blob_loads);
        let ino = overlay.ensure(Path::new("tracked.tmp")).unwrap();
        assert_eq!(overlay.open_file(ino, libc::O_RDWR), Err(libc::EROFS));
    }
    #[test]
    fn untracked_mutates_new_paths_inside_tracked_directories_and_protects_originals() {
        let (_temp, mut overlay) = directory_fixture();
        overlay.write_policy = WritePolicy::Untracked;
        overlay.policy = Policy::with_mode(&overlay._snapshot, WritePolicy::Untracked).unwrap();
        let source = overlay.ensure(Path::new("source")).unwrap();
        let original = overlay.ensure(Path::new("source/file")).unwrap();
        let new = file(&mut overlay, source, "not-ignored");
        let handle = overlay.open_file(new, libc::O_RDWR).unwrap();
        overlay
            .write_data(new, handle, 0, b"new bytes", 0, libc::O_RDWR)
            .unwrap();
        overlay
            .rename_path(
                source,
                OsStr::new("not-ignored"),
                source,
                OsStr::new("renamed"),
                0,
            )
            .unwrap();
        overlay.truncate(new, 3).unwrap();
        assert_eq!(overlay.read_data(new, handle, 0, 100).unwrap(), b"new");
        assert_eq!(
            overlay.rename_path(source, OsStr::new("renamed"), source, OsStr::new("file"), 0),
            Err(libc::EROFS)
        );
        assert_eq!(
            overlay.rename_path(1, OsStr::new("source"), 1, OsStr::new("moved"), 0),
            Err(libc::EROFS)
        );
        assert_eq!(
            overlay.remove(1, OsStr::new("source"), true),
            Err(libc::EROFS)
        );
        assert_eq!(
            overlay.remove(source, OsStr::new("file"), false),
            Err(libc::EROFS)
        );
        assert_eq!(overlay.open_file(original, libc::O_RDWR), Err(libc::EROFS));
        assert_eq!(
            overlay.hardlink(original, source, OsStr::new("original-alias")),
            Err(libc::EROFS)
        );
        overlay
            .hardlink(new, source, OsStr::new("new-alias"))
            .unwrap();
        overlay
            .remove(source, OsStr::new("renamed"), false)
            .unwrap();
        assert_eq!(overlay.ensure(Path::new("source/new-alias")).unwrap(), new);
        assert_eq!(overlay.ensure(Path::new("source/file")).unwrap(), original);
        assert!(overlay.whiteouts.is_empty());
        assert!(overlay.redirects.is_empty());
    }
    #[test]
    fn untracked_does_not_demand_missing_ignore_objects() {
        let (temp, mut overlay) = fixture_policy(WritePolicy::Untracked);
        let repo = Repository::open(temp.path().join("repo")).unwrap();
        let oid = repo
            .find_tree(overlay._snapshot.tree_oid)
            .unwrap()
            .get_name(".gitignore")
            .unwrap()
            .id()
            .to_string();
        fs::remove_file(repo.path().join("objects").join(&oid[..2]).join(&oid[2..])).unwrap();
        let before = overlay._snapshot.metrics();
        let generated = file(&mut overlay, 1, "ordinary-unignored-path");
        assert!(!overlay.nodes[&generated].immutable);
        assert_eq!(overlay._snapshot.metrics().blob_loads, before.blob_loads);
        assert_eq!(
            overlay.remove(1, OsStr::new("tracked.tmp"), false),
            Err(libc::EROFS)
        );
    }
    fn directory_fixture() -> (TempDir, Overlay) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path().join("repo")).unwrap();
        let content = repo.blob(b"original").unwrap();
        let mut nested = repo.treebuilder(None).unwrap();
        nested.insert("leaf", content, 0o100644).unwrap();
        let nested = nested.write().unwrap();
        let mut source = repo.treebuilder(None).unwrap();
        source.insert("file", content, 0o100644).unwrap();
        source.insert("nested", nested, 0o040000).unwrap();
        let source = source.write().unwrap();
        let empty = repo.treebuilder(None).unwrap().write().unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("source", source, 0o040000).unwrap();
        tree.insert("destination", empty, 0o040000).unwrap();
        tree.insert("untouched", source, 0o040000).unwrap();
        let oid = tree.write().unwrap();
        let snapshot =
            Snapshot::load(repo.path(), OsStr::new(&oid.to_string()), "directory-cow").unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), WritePolicy::All)
                .unwrap();
        (temp, overlay)
    }
    #[test]
    fn all_directory_rename_preserves_lazy_origins_and_moves_child_whiteouts() {
        let (_temp, mut overlay) = directory_fixture();
        let source = overlay.ensure(Path::new("source")).unwrap();
        let before = overlay._snapshot.metrics();
        overlay
            .rename_path(1, OsStr::new("source"), 1, OsStr::new("destination"), 0)
            .unwrap();
        assert_eq!(
            overlay._snapshot.metrics().tree_reads,
            before.tree_reads + 1,
            "only validate the empty destination, never traverse source descendants"
        );
        assert_eq!(overlay._snapshot.metrics().blob_loads, before.blob_loads);
        assert_eq!(overlay.ensure(Path::new("source")), Err(libc::ENOENT));
        assert_eq!(overlay.ensure(Path::new("destination")).unwrap(), source);
        assert!(!overlay.paths.contains_key(Path::new("destination/file")));
        let file = overlay.ensure(Path::new("destination/file")).unwrap();
        let read = overlay.open_file(file, libc::O_RDONLY).unwrap();
        assert_eq!(overlay.read_data(file, read, 0, 100).unwrap(), b"original");
        overlay.remove(source, OsStr::new("file"), false).unwrap();
        let directory = overlay.open_directory(source, libc::O_RDONLY).unwrap();
        assert!(!overlay.directories[&directory]
            .iter()
            .any(|(_, _, name)| name == "file"));
        overlay.close_handle(source, directory);
        overlay
            .rename_path(1, OsStr::new("destination"), 1, OsStr::new("again"), 0)
            .unwrap();
        assert_eq!(overlay.ensure(Path::new("again/file")), Err(libc::ENOENT));
        let leaf = overlay.ensure(Path::new("again/nested/leaf")).unwrap();
        let handle = overlay.open_file(leaf, libc::O_RDONLY).unwrap();
        assert_eq!(
            overlay.read_data(leaf, handle, 0, 100).unwrap(),
            b"original"
        );
    }
    #[test]
    fn all_directory_rename_never_demands_listed_child_headers() {
        let (temp, mut overlay) = directory_fixture();
        let source = overlay.ensure(Path::new("source")).unwrap();
        let handle = overlay.open_directory(source, libc::O_RDONLY).unwrap();
        let reserved = overlay.paths[Path::new("source/file")];
        assert!(!overlay.nodes.contains_key(&reserved));
        let repo = Repository::open(temp.path().join("repo")).unwrap();
        let oid = repo
            .find_tree(overlay._snapshot.tree_oid)
            .unwrap()
            .get_path(Path::new("source/file"))
            .unwrap()
            .id()
            .to_string();
        fs::remove_file(repo.path().join("objects").join(&oid[..2]).join(&oid[2..])).unwrap();
        let before = overlay._snapshot.metrics();
        overlay
            .rename_path(1, OsStr::new("source"), 1, OsStr::new("moved"), 0)
            .unwrap();
        assert_eq!(overlay.paths[Path::new("moved/file")], reserved);
        assert!(!overlay.nodes.contains_key(&reserved));
        assert_eq!(
            overlay._snapshot.metrics().header_reads,
            before.header_reads
        );
        assert_eq!(overlay.ensure(Path::new("moved/file")), Err(libc::EIO));
        overlay.close_handle(source, handle);
    }
    #[test]
    fn all_replaces_unseen_tracked_destinations_without_resurrecting_lower_entries() {
        let (_temp, mut overlay) = fixture_policy(WritePolicy::All);
        let fresh = file(&mut overlay, 1, "fresh");
        assert!(!overlay.paths.contains_key(Path::new("tracked.tmp")));
        overlay
            .rename_path(1, OsStr::new("fresh"), 1, OsStr::new("tracked.tmp"), 0)
            .unwrap();
        assert_eq!(overlay.ensure(Path::new("tracked.tmp")).unwrap(), fresh);
        let directory = overlay.open_directory(1, libc::O_RDONLY).unwrap();
        assert_eq!(
            overlay.directories[&directory]
                .iter()
                .filter(|(_, _, name)| name == "tracked.tmp")
                .count(),
            1
        );
        overlay.close_handle(1, directory);
        overlay.remove(1, OsStr::new("tracked.tmp"), false).unwrap();
        assert_eq!(overlay.ensure(Path::new("tracked.tmp")), Err(libc::ENOENT));
    }
    #[test]
    fn all_unlinked_original_read_inode_can_acquire_private_metadata() {
        let (_temp, mut overlay) = fixture_policy(WritePolicy::All);
        let file = overlay.ensure(Path::new("tracked.tmp")).unwrap();
        let handle = overlay.open_file(file, libc::O_RDONLY).unwrap();
        overlay.remove(1, OsStr::new("tracked.tmp"), false).unwrap();
        overlay.copy_up(file, true).unwrap();
        overlay.truncate(file, 6).unwrap();
        assert_eq!(overlay.read_data(file, handle, 0, 100).unwrap(), b"pinned");
        assert!(!overlay.backing.join(file.to_string()).exists());
        assert!(!overlay.upper_paths.contains_key(Path::new("tracked.tmp")));
        assert_eq!(overlay.nodes[&file].attr.nlink, 0);
    }
    #[test]
    fn all_truncate_to_zero_skips_source_content_and_directory_errors_are_atomic() {
        let (_temp, mut overlay) = fixture_policy(WritePolicy::All);
        let file = overlay.ensure(Path::new("tracked.tmp")).unwrap();
        let before = overlay._snapshot.metrics();
        let handle = overlay
            .open_file(file, libc::O_RDWR | libc::O_TRUNC)
            .unwrap();
        assert!(overlay.read_data(file, handle, 0, 100).unwrap().is_empty());
        assert_eq!(overlay._snapshot.metrics().blob_loads, before.blob_loads);
        let (_temp, mut overlay) = directory_fixture();
        assert_eq!(
            overlay.remove(1, OsStr::new("source"), true),
            Err(libc::ENOTEMPTY)
        );
        assert_eq!(
            overlay.rename_path(1, OsStr::new("destination"), 1, OsStr::new("source"), 0),
            Err(libc::ENOTEMPTY)
        );
        assert!(overlay.whiteouts.is_empty());
        assert!(overlay.upper_paths.is_empty());
        assert!(overlay.ensure(Path::new("source/file")).is_ok());
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
    #[cfg(target_os = "linux")]
    fn passthrough_data_metadata_survives_close_reopen_without_exposing_backing_mode() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "generated.tmp");
        let handle = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().attr.perm = 0o751;
        // Simulate the actual kernel's bypass of write_data, not passthrough
        // negotiation. The real-kernel test proves bypass separately.
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        let file = overlay.disk_file(ino).unwrap();
        file.write_at(b"extended", 4096).unwrap();
        overlay.refresh_backing_attr(ino).unwrap();
        assert_eq!(overlay.node(ino).unwrap().attr.size, 4104);
        assert_eq!(overlay.node(ino).unwrap().attr.perm, 0o751);
        overlay.close_handle(ino, handle);
        overlay.refresh_backing_attr(ino).unwrap();
        let handle = overlay.open_file(ino, libc::O_RDONLY).unwrap();
        assert_eq!(
            overlay.read_data(ino, handle, 4096, 100).unwrap(),
            b"extended"
        );
        overlay.truncate(ino, 3).unwrap();
        overlay.refresh_backing_attr(ino).unwrap();
        assert_eq!(overlay.node(ino).unwrap().attr.size, 3);
        assert_eq!(overlay.read_data(ino, handle, 0, 100).unwrap(), [0; 3]);
        assert_eq!(overlay.node(ino).unwrap().attr.perm, 0o751);
    }

    #[test]
    fn passthrough_first_open_keeps_times_set_before_registration() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "before-open.tmp");
        let accessed = SystemTime::UNIX_EPOCH + Duration::from_secs(1234);
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(5678);
        assert!(!overlay.node(ino).unwrap().external_io);
        overlay
            .set_attributes(
                ino,
                0,
                SetAttributes {
                    atime: Some(TimeOrNow::SpecificTime(accessed)),
                    mtime: Some(TimeOrNow::SpecificTime(modified)),
                    ..Default::default()
                },
            )
            .unwrap();
        let handle = overlay.open_file(ino, libc::O_RDONLY).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        let attr = overlay.attributes(ino, Some(handle)).unwrap();
        assert_eq!((attr.atime, attr.mtime), (accessed, modified));
    }

    #[test]
    fn passthrough_link_reply_refreshes_backing_metadata_without_stat() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "generated.tmp");
        let _handle = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        let backing = overlay.disk_file(ino).unwrap();
        backing.write_at(b"extended", 4096).unwrap();
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(5678);
        backing
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let expected = backing.metadata().unwrap();

        // No getattr, close or other overlay refresh between bypass I/O and LINK.
        let linked = overlay.link_node(ino, 1, OsStr::new("alias.tmp")).unwrap();
        assert_eq!(linked.size, expected.len());
        assert_eq!(linked.blocks, expected.blocks());
        assert_eq!(linked.mtime, expected.modified().unwrap());
        assert_eq!(linked.nlink, 2);
        assert_eq!(linked.perm, 0o600);
    }

    #[test]
    fn passthrough_link_preserves_namespace_ctime_after_stat() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "generated.tmp");
        let _handle = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        std::thread::sleep(Duration::from_millis(5));
        let linked = overlay.link_node(ino, 1, OsStr::new("alias.tmp")).unwrap();
        assert_eq!(overlay.attributes(ino, None).unwrap().ctime, linked.ctime);
    }

    #[test]
    fn passthrough_rename_preserves_namespace_ctime_after_stat() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "generated.tmp");
        let _handle = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        std::thread::sleep(Duration::from_millis(5));
        overlay
            .rename_path(
                1,
                OsStr::new("generated.tmp"),
                1,
                OsStr::new("renamed.tmp"),
                0,
            )
            .unwrap();
        let changed = overlay.node(ino).unwrap().attr.ctime;
        assert_eq!(overlay.attributes(ino, None).unwrap().ctime, changed);
    }

    #[test]
    fn passthrough_mode_and_partial_utime_preserve_unspecified_disk_times() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "generated.tmp");
        let handle = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        // The kernel writes without going through Overlay or refreshing its
        // cache. chmod must not copy cached pre-write timestamps back to disk.
        let backing = overlay.disk_file(ino).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        backing.write_at(b"kernel write", 0).unwrap();
        let modified = backing.metadata().unwrap().modified().unwrap();
        let attr = overlay
            .set_attributes(
                ino,
                0,
                SetAttributes {
                    mode: Some(0o751),
                    fh: Some(handle),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(attr.mtime, modified);
        assert_eq!(attr.perm, 0o751);
        // A second bypass write makes the cache stale again before an atime-only update.
        std::thread::sleep(Duration::from_millis(5));
        backing.write_at(b"newer", 0).unwrap();
        let modified = backing.metadata().unwrap().modified().unwrap();
        let accessed = SystemTime::UNIX_EPOCH + Duration::from_secs(1234);
        let attr = overlay
            .set_attributes(
                ino,
                0,
                SetAttributes {
                    atime: Some(TimeOrNow::SpecificTime(accessed)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(attr.atime, accessed);
        assert_eq!(attr.mtime, modified);
        assert_eq!(backing.metadata().unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn passthrough_path_utime_after_last_close_updates_backing_and_reopen() {
        let (_temp, mut overlay) = fixture();
        let ino = file(&mut overlay, 1, "timestamps.tmp");
        let handle = overlay.open_file(ino, libc::O_RDWR).unwrap();
        overlay.nodes.get_mut(&ino).unwrap().external_io = true;
        overlay.close_handle(ino, handle);
        assert!(overlay.node(ino).unwrap().file.is_none());
        let atime = SystemTime::UNIX_EPOCH + Duration::from_secs(1234);
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(5678);
        let attr = overlay
            .set_attributes(
                ino,
                0,
                SetAttributes {
                    atime: Some(TimeOrNow::SpecificTime(atime)),
                    mtime: Some(TimeOrNow::SpecificTime(mtime)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(attr.atime, atime);
        assert_eq!(attr.mtime, mtime);
        let metadata = overlay.disk_file(ino).unwrap().metadata().unwrap();
        assert_eq!(metadata.accessed().unwrap(), atime);
        assert_eq!(metadata.modified().unwrap(), mtime);
        let handle = overlay.open_file(ino, libc::O_RDONLY).unwrap();
        let attr = overlay.attributes(ino, Some(handle)).unwrap();
        assert_eq!((attr.atime, attr.mtime), (atime, mtime));
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
