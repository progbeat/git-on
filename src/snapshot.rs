//! Immutable, demand-loaded objects from the pinned Git tree.
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid, Repository, RepositoryOpenFlags};
use serde::Serialize;
#[cfg(feature = "test-metrics")]
use std::collections::BTreeMap;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::symlink,
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
};

pub const BLOB_CACHE_BYTES: usize = 8 * 1024 * 1024;
const BLOB_CACHE_ENTRIES: usize = 128;
const TREE_CACHE_BYTES: usize = 4 * 1024 * 1024;
const TREE_CACHE_ENTRIES: usize = 256;
const HEADER_CACHE_ENTRIES: usize = 4096;

/// Recognize complete pointer files, not ordinary source/documentation which
/// happens to start with the LFS version line. Keep this bounded even for large
/// blobs. Format: https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md
fn is_lfs_pointer(bytes: &[u8]) -> bool {
    if bytes.len() >= 1024 || bytes.contains(&0) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    // Like Git LFS's decoder, also recognize CRLF and a missing final newline.
    let mut lines = text.lines();
    if !matches!(
        lines.next(),
        Some(
            "version https://git-lfs.github.com/spec/v1"
                | "version https://hawser.github.com/spec/v1"
                | "version http://git-media.io/v/2"
        )
    ) {
        return false;
    }
    let valid_oid = |value: &str| {
        value.strip_prefix("sha256:").is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    };
    let mut previous = None;
    let mut oid = false;
    let mut size = false;
    let mut extension_priorities = [false; 10];
    for line in lines {
        let Some((key, value)) = line.split_once(' ') else {
            return false;
        };
        if key.is_empty()
            || !key.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte)
            })
            || value.is_empty()
            || value.starts_with(' ')
            || value.contains('\r')
            || key == "version"
            || previous.is_some_and(|last| last >= key)
        {
            return false;
        }
        previous = Some(key);
        match key {
            "oid" => {
                if !valid_oid(value) {
                    return false;
                }
                oid = true;
            }
            "size" => {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) || value.parse::<i64>().is_err()
                {
                    return false;
                }
                size = true;
            }
            key if key.starts_with("ext-") => {
                let Some((priority, name)) = key[4..].split_once('-') else {
                    return false;
                };
                if priority.len() != 1
                    || !priority.as_bytes()[0].is_ascii_digit()
                    || name.is_empty()
                    || !valid_oid(value)
                {
                    return false;
                }
                let priority = usize::from(priority.as_bytes()[0] - b'0');
                if std::mem::replace(&mut extension_priorities[priority], true) {
                    return false;
                }
            }
            // The specification permits unknown keys; their syntax and order
            // still have to be valid, and they cannot replace required fields.
            _ => {}
        }
    }
    oid && size
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: EntryKind,
    pub mode: u32,
    pub oid: Oid,
    /// Directory listings deliberately omit blob sizes. `entry` obtains the
    /// correct size from an ODB header, without requesting the file payload.
    pub size: Option<u64>,
}
#[derive(Debug)]
struct Pin {
    git_dir: PathBuf,
    name: String,
    oid: Oid,
}

/// libgit2 handles relative object environments before Git's repository setup
/// and splits alternate paths on every colon. Adapt native Git's selected
/// locations to private ASCII aliases without copying any objects.
fn local_object_bridge(
    git: &crate::git::Git,
    repo: &Repository,
    run_directory: &Path,
) -> Result<Option<tempfile::TempDir>> {
    let alternates = std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    if alternates.is_none() && std::env::var_os("GIT_OBJECT_DIRECTORY").is_none() {
        return Ok(None);
    }
    let primary = git.output(
        None,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-path"),
            OsStr::new("objects"),
        ],
    )?;
    let mut locations = vec![PathBuf::from(OsString::from_vec(
        primary.strip_suffix(b"\n").unwrap_or(&primary).to_vec(),
    ))];
    let mut cwd = git.cwd.canonicalize()?;
    if alternates.is_some() {
        let prefix = git.output(
            None,
            &[OsStr::new("rev-parse"), OsStr::new("--show-prefix")],
        )?;
        // Git moves from a worktree subdirectory to its top level. Bare
        // invocations and explicit GIT_DIR calls outside a worktree retain the
        // invocation cwd, even when --show-cdup prints a worktree path.
        if !prefix.strip_suffix(b"\n").unwrap_or(&prefix).is_empty() {
            let cdup = git.output(None, &[OsStr::new("rev-parse"), OsStr::new("--show-cdup")])?;
            cwd = cwd
                .join(PathBuf::from(OsString::from_vec(
                    cdup.strip_suffix(b"\n").unwrap_or(&cdup).to_vec(),
                )))
                .canonicalize()?;
        }
    }
    let mut remaining = alternates
        .as_ref()
        .map_or(&[][..], |value| value.as_bytes());
    while !remaining.is_empty() {
        let end = remaining
            .iter()
            .position(|byte| *byte == b':')
            .unwrap_or(remaining.len());
        let decoded = remaining
            .starts_with(b"\"")
            .then(|| gix_quote::ansi_c::undo(remaining.into()))
            .and_then(Result::ok);
        let (value, consumed) = if remaining.starts_with(b"#") {
            (Vec::new(), end)
        } else if let Some((value, consumed)) = decoded {
            (value.as_ref().to_vec(), consumed)
        } else {
            // Native Git likewise falls back to the ordinary component when
            // quoting is broken; missing paths are ignored by its ODB loader.
            (remaining[..end].to_vec(), end)
        };
        remaining = &remaining[consumed..];
        if !remaining.is_empty() {
            remaining = &remaining[1..];
        }
        if !value.is_empty() {
            let path = PathBuf::from(OsString::from_vec(value));
            if let Ok(path) = cwd.join(path).canonicalize() {
                locations.push(path);
            }
        }
    }
    let bridge = tempfile::Builder::new()
        .prefix("local-object-bridge-")
        .tempdir_in(run_directory)?;
    let private = Repository::init_bare(bridge.path())?;
    let mut aliases = String::new();
    for (index, location) in locations.into_iter().enumerate() {
        let name = format!("alternate-{index}");
        symlink(location, bridge.path().join(&name))?;
        aliases.push_str(&format!("../{name}\n"));
    }
    fs::write(bridge.path().join("objects/info/alternates"), aliases)?;
    repo.set_odb(&private.odb()?)?;
    Ok(Some(bridge))
}
impl Drop for Pin {
    fn drop(&mut self) {
        if let Ok(repo) = Repository::open(&self.git_dir) {
            if let Ok(mut reference) = repo.find_reference(&self.name) {
                if reference.target() == Some(self.oid) {
                    let _ = reference.delete();
                }
            };
        }
    }
}

/// Application-level demand requests, excluding initial tree-ish resolution.
/// libgit2's necessary pack/delta decoding is not separately observable here.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SnapshotMetrics {
    /// Successful explicit native fetches, including initial ref resolution.
    pub remote_fetches: u64,
    pub remote_fetch_operations: u64,
    pub remote_decode_operations: u64,
    pub tree_reads: u64,
    pub header_reads: u64,
    pub blob_loads: u64,
    pub blob_bytes: u64,
    pub cache_hits: u64,
    pub blob_cache_bytes: usize,
    pub blob_cache_entries: usize,
    pub blob_cache_limit: usize,
    pub tree_cache_bytes: usize,
    pub tree_cache_entries: usize,
    pub tree_cache_limit: usize,
    pub header_cache_entries: usize,
    pub active_spool_bytes: u64,
    pub active_blob_readers: usize,
    pub streamed_blob_loads: u64,
    pub spool_writes: u64,
    pub spool_bytes: u64,
    pub retained_raw_blob_bytes: u64,
    pub retained_raw_blob_entries: u64,
    pub remote_raw_blob_imports: u64,
    pub remote_raw_blob_bytes: u64,
    #[cfg(feature = "test-metrics")]
    pub passthrough_handles: u64,
    #[cfg(feature = "test-metrics")]
    pub generated_read_callbacks: u64,
    #[cfg(feature = "test-metrics")]
    pub generated_write_callbacks: u64,
    #[cfg(feature = "test-metrics")]
    pub file_sync_callbacks: u64,
    #[cfg(feature = "test-metrics")]
    pub fuse_requests: BTreeMap<&'static str, u64>,
}

enum BlobData {
    Memory {
        bytes: Weak<[u8]>,
        raw: crate::raw_blob::RawBlob,
    },
    Stored(crate::raw_blob::RawBlob),
}
type ReaderRegistry = Mutex<HashMap<Oid, Weak<ActiveBlob>>>;
struct ActiveBlob {
    oid: Oid,
    data: BlobData,
    registry: Weak<ReaderRegistry>,
}
impl Drop for ActiveBlob {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
            if registry
                .get(&self.oid)
                .is_some_and(|reader| std::ptr::eq(reader.as_ptr(), self))
            {
                registry.remove(&self.oid);
            }
        }
    }
}
/// A live source-file reader. Large objects decode once into unlinked run-local
/// storage, reused even after every reader has closed.
#[derive(Clone)]
pub struct BlobReader(Arc<ActiveBlob>);
impl BlobReader {
    pub fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        match &self.0.data {
            BlobData::Memory { bytes, raw } => {
                let Some(bytes) = bytes.upgrade() else {
                    return raw.read(offset, size);
                };
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                let end = start.saturating_add(size as usize).min(bytes.len());
                Ok(bytes[start..end].to_vec())
            }
            BlobData::Stored(blob) => blob.read(offset, size),
        }
    }
}

/// A byte- and entry-bounded LRU. Values exceeding the budget are never retained.
struct Cache<V> {
    values: lru::LruCache<Oid, (V, usize)>,
    bytes: usize,
    byte_limit: usize,
    entry_limit: usize,
}
impl<V: Clone> Cache<V> {
    fn new(byte_limit: usize, entry_limit: usize) -> Self {
        Self {
            values: lru::LruCache::unbounded(),
            bytes: 0,
            byte_limit,
            entry_limit,
        }
    }
    fn get(&mut self, oid: Oid) -> Option<V> {
        self.values.get(&oid).map(|(value, _)| value.clone())
    }
    fn insert(&mut self, oid: Oid, value: V, bytes: usize) {
        if bytes > self.byte_limit || self.entry_limit == 0 {
            return;
        }
        if let Some((_, old_bytes)) = self.values.pop(&oid) {
            self.bytes -= old_bytes;
        }
        while self.values.len() >= self.entry_limit
            || self.bytes > self.byte_limit.saturating_sub(bytes)
        {
            let (_, (_, removed_bytes)) = self.values.pop_lru().expect("cache has a victim");
            self.bytes -= removed_bytes;
        }
        self.values.put(oid, (value, bytes));
        self.bytes += bytes;
    }
}

#[derive(Debug)]
struct TreeItem {
    name: OsString,
    mode: u32,
    oid: Oid,
}
struct State {
    repo: Repository,
    remote: Option<crate::remote::Remote>,
    trees: Cache<Arc<[TreeItem]>>,
    headers: Cache<u64>,
    blobs: Cache<Arc<[u8]>>,
    readers: Arc<ReaderRegistry>,
    raw_blobs: Option<crate::raw_blob::RawBlobs>,
    metrics: SnapshotMetrics,
    spool_directory: PathBuf,
    _local_object_bridge: Option<tempfile::TempDir>,
}
impl State {
    fn ensure(&mut self, oid: Oid) -> Result<()> {
        if let Some(remote) = &mut self.remote {
            remote.ensure_in(&self.repo, oid)?;
        }
        Ok(())
    }
    fn tree(&mut self, oid: Oid) -> Result<Arc<[TreeItem]>> {
        if let Some(tree) = self.trees.get(oid) {
            self.metrics.cache_hits += 1;
            return Ok(tree);
        }
        self.metrics.tree_reads += 1;
        self.ensure(oid)?;
        let (entries, weight) = {
            let tree = self.repo.find_tree(oid).context("read snapshot tree")?;
            let mut entries = Vec::with_capacity(tree.len());
            let mut weight = std::mem::size_of::<usize>() * 2;
            for item in tree.iter() {
                let bytes = item.name_bytes();
                if bytes.is_empty()
                    || bytes.contains(&b'/')
                    || bytes.contains(&0)
                    || bytes == b"."
                    || bytes == b".."
                {
                    bail!("invalid name in Git tree");
                }
                // This copies only this directory's metadata, never descendants.
                weight = weight.saturating_add(std::mem::size_of::<TreeItem>() + bytes.len());
                entries.push(TreeItem {
                    name: OsString::from_vec(bytes.to_vec()),
                    mode: item.filemode() as u32,
                    oid: item.id(),
                });
            }
            entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            if entries.windows(2).any(|pair| pair[0].name == pair[1].name) {
                bail!("duplicate path in Git tree");
            }
            (Arc::<[TreeItem]>::from(entries), weight)
        };
        self.trees.insert(oid, entries.clone(), weight);
        Ok(entries)
    }
    fn size(&mut self, oid: Oid) -> Result<u64> {
        if let Some(size) = self.headers.get(oid) {
            self.metrics.cache_hits += 1;
            return Ok(size);
        }
        self.metrics.header_reads += 1;
        self.ensure(oid)?;
        if let Some(raw) = self
            .remote
            .as_ref()
            .map(|remote| remote.raw_blobs.get(oid))
            .transpose()?
            .flatten()
            .or(self
                .raw_blobs
                .as_ref()
                .map(|raw| raw.get(oid))
                .transpose()?
                .flatten())
        {
            let size = raw.len();
            self.headers.insert(oid, size, std::mem::size_of::<u64>());
            return Ok(size);
        }
        let (size, kind) = self
            .repo
            .odb()?
            .read_header(oid)
            .context("read blob header")?;
        if kind != ObjectType::Blob {
            bail!("snapshot file does not reference a blob");
        }
        let size = size as u64;
        self.headers.insert(oid, size, std::mem::size_of::<u64>());
        Ok(size)
    }
    fn open(&mut self, oid: Oid) -> Result<BlobReader> {
        let existing = self
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&oid)
            .and_then(Weak::upgrade);
        if let Some(reader) = existing {
            self.metrics.cache_hits += 1;
            return Ok(BlobReader(reader));
        }
        let data = if let Some(bytes) = self.blobs.get(oid) {
            self.metrics.cache_hits += 1;
            let raw = if let Some(remote) = &self.remote {
                remote.raw_blobs.get(oid)?
            } else {
                self.raw_blobs
                    .as_ref()
                    .map(|raw| raw.get(oid))
                    .transpose()?
                    .flatten()
            }
            .context("cached blob has no prepared raw storage")?;
            BlobData::Memory {
                bytes: Arc::downgrade(&bytes),
                raw,
            }
        } else {
            let raw = if let Some(remote) = &mut self.remote {
                remote.ensure_in(&self.repo, oid)?;
                let (raw, first) = remote
                    .raw_blobs
                    .for_read(oid)?
                    .context("snapshot file does not reference a blob")?;
                if first {
                    self.metrics.blob_loads += 1;
                    self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(raw.len());
                    if raw.len() > self.blobs.byte_limit as u64 {
                        self.metrics.spool_writes += 1;
                        self.metrics.spool_bytes =
                            self.metrics.spool_bytes.saturating_add(raw.len());
                    }
                } else {
                    self.metrics.cache_hits += 1;
                }
                raw
            } else if let Some(raw) = self
                .raw_blobs
                .as_ref()
                .map(|raw| raw.get(oid))
                .transpose()?
                .flatten()
            {
                self.metrics.cache_hits += 1;
                raw
            } else {
                let (size, kind) = self.repo.odb()?.read_header(oid)?;
                anyhow::ensure!(
                    kind == ObjectType::Blob,
                    "snapshot file does not reference a blob"
                );
                if self.raw_blobs.is_none() {
                    self.raw_blobs = Some(crate::raw_blob::RawBlobs::new(&self.spool_directory)?);
                }
                let raw = self.raw_blobs.as_mut().unwrap();
                let odb = self.repo.odb()?;
                let (blob, streamed) = if let Ok((reader, length, kind)) = odb.reader(oid) {
                    anyhow::ensure!(
                        kind == ObjectType::Blob && length == size,
                        "Git blob changed during transfer"
                    );
                    (raw.insert(oid, size, reader, &self.spool_directory)?, true)
                } else {
                    let object = odb.read(oid).context("decode Git blob")?;
                    anyhow::ensure!(
                        object.kind() == ObjectType::Blob && object.len() == size,
                        "Git blob changed during transfer"
                    );
                    (
                        raw.insert(oid, size, object.data(), &self.spool_directory)?,
                        false,
                    )
                };
                self.metrics.blob_loads += 1;
                self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(size as u64);
                if size > self.blobs.byte_limit {
                    self.metrics.spool_writes += 1;
                    self.metrics.spool_bytes = self.metrics.spool_bytes.saturating_add(size as u64);
                    self.metrics.streamed_blob_loads += u64::from(streamed);
                }
                blob
            };
            if raw.len() <= self.blobs.byte_limit as u64 {
                let bytes = raw.read_all()?;
                Self::validate_blob(&bytes)?;
                let bytes = Arc::<[u8]>::from(bytes);
                self.blobs.insert(oid, bytes.clone(), bytes.len());
                BlobData::Memory {
                    bytes: Arc::downgrade(&bytes),
                    raw,
                }
            } else {
                BlobData::Stored(raw)
            }
        };
        let reader = Arc::new(ActiveBlob {
            oid,
            data,
            registry: Arc::downgrade(&self.readers),
        });
        self.readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(oid, Arc::downgrade(&reader));
        Ok(BlobReader(reader))
    }
    fn validate_blob(bytes: &[u8]) -> Result<()> {
        if is_lfs_pointer(bytes) {
            bail!("Git LFS pointer blobs are unsupported");
        }
        Ok(())
    }
    /// Full-object consumers explicitly need the returned bytes (ignore rules
    /// and symlink targets). Ordinary ranged reads use BlobReader directly.
    fn with_blob<T>(&mut self, oid: Oid, consume: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        let reader = self.open(oid)?;
        match &reader.0.data {
            BlobData::Memory { bytes, raw } => match bytes.upgrade() {
                Some(bytes) => consume(&bytes),
                None => consume(&raw.read_all()?),
            },
            BlobData::Stored(raw) => consume(&raw.read_all()?),
        }
    }
}
struct Inner {
    state: Mutex<State>,
    _pin: Pin,
}
/// Clones share the object caches and pin; there is no all-path index.
#[derive(Clone)]
pub struct Snapshot {
    pub tree_oid: Oid,
    pub git_dir: PathBuf,
    pub pin_ref: String,
    inner: Arc<Inner>,
}
impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("tree_oid", &self.tree_oid)
            .field("git_dir", &self.git_dir)
            .field("pin_ref", &self.pin_ref)
            .finish_non_exhaustive()
    }
}
impl Snapshot {
    pub fn load(repo_path: &Path, treeish: &OsStr, run_id: &str) -> Result<Self> {
        Self::load_with_pin_record(repo_path, treeish, run_id, |_, _, _| Ok(()))
    }
    /// Persist the planned pin before creating it, so interrupted loads are recoverable.
    /// Resolving/pinning the root is the only startup object work; no tree walk.
    pub fn load_with_pin_record<F>(
        repo_path: &Path,
        treeish: &OsStr,
        run_id: &str,
        record: F,
    ) -> Result<Self>
    where
        F: FnMut(&Path, &str, Oid) -> Result<()>,
    {
        Self::load_source_with_pin_record(
            repo_path,
            None,
            treeish,
            false,
            &std::env::temp_dir(),
            run_id,
            record,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn load_source_with_pin_record<F>(
        repo_path: &Path,
        source: Option<&OsStr>,
        treeish: &OsStr,
        verify: bool,
        run_directory: &Path,
        run_id: &str,
        mut record: F,
    ) -> Result<Self>
    where
        F: FnMut(&Path, &str, Oid) -> Result<()>,
    {
        let (repo, mut remote, oid, git, local_bridge) = if let Some(source) = source {
            let (remote, oid) =
                crate::remote::Remote::open(repo_path, source, treeish, run_directory)?;
            (
                Repository::open_bare(&remote.dir)?,
                Some(remote),
                oid,
                None,
                None,
            )
        } else {
            let git = crate::git::Git::capture(repo_path)?;
            let oid = git.resolve(treeish)?;
            // Native Git resolved the operand in the caller's object namespace.
            // Keep that environment for subsequent lazy libgit2 object reads.
            let repo = Repository::open_ext(
                git.repository_dir()?,
                RepositoryOpenFlags::FROM_ENV,
                &[] as &[&OsStr],
            )
            .context("open selected Git repository and object environment")?;
            // GIT_NAMESPACE scopes upload-pack/receive-pack refs in native Git,
            // while libgit2 also applies it to ordinary reference operations.
            // Keep the GC pin in the same ref namespace as native update-ref,
            // so normal release and recorded stale cleanup find that pin.
            repo.remove_namespace()?;
            let bridge = local_object_bridge(&git, &repo, run_directory)?;
            (repo, None, oid, Some(git), bridge)
        };
        let ensure = |remote: &mut Option<crate::remote::Remote>, oid| -> Result<()> {
            if let Some(remote) = remote {
                remote.ensure_in(&repo, oid)?;
            }
            Ok(())
        };
        let object = repo.find_object(oid, None)?;
        if verify {
            let git = remote.as_ref().map(|r| &r.git).or(git.as_ref()).unwrap();
            git.verify(
                repo.path(),
                oid,
                object.kind().context("unknown Git object type")?,
            )?;
        }
        drop(object);
        let mut selected = oid;
        let tree_oid = loop {
            ensure(&mut remote, selected)?;
            let object = repo.find_object(selected, None)?;
            match object.kind() {
                Some(ObjectType::Tree) => break selected,
                Some(ObjectType::Commit) => break object.as_commit().unwrap().tree_id(),
                Some(ObjectType::Tag) => {
                    selected = object.as_tag().unwrap().target_id();
                }
                _ => bail!("revision does not resolve to a tree"),
            }
        };
        // libgit2 requires a reference target to exist. Fetch only the selected
        // root tree, not its descendants, before installing the GC pin.
        ensure(&mut remote, tree_oid)?;
        let pin_ref = format!("refs/git-on/{run_id}");
        if run_id.is_empty() || !git2::Reference::is_valid_name(&pin_ref) {
            bail!("invalid snapshot run ID");
        }
        // Ordinary refs live in the common directory, including an explicit
        // GIT_COMMON_DIR override. Record that actual owner so release/recovery
        // do not require the original process environment to find the pin.
        let pin_git_dir = repo
            .commondir()
            .canonicalize()
            .context("resolve pin reference storage")?;
        record(&pin_git_dir, &pin_ref, tree_oid)?;
        repo.reference(&pin_ref, tree_oid, false, "git-on snapshot pin")
            .context("pin snapshot tree against garbage collection")?;
        let git_dir = repo.path().to_path_buf();
        let pin = Pin {
            git_dir: pin_git_dir,
            name: pin_ref.clone(),
            oid: tree_oid,
        };
        Ok(Self {
            tree_oid,
            git_dir,
            pin_ref,
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    repo,
                    remote,
                    trees: Cache::new(TREE_CACHE_BYTES, TREE_CACHE_ENTRIES),
                    headers: Cache::new(HEADER_CACHE_ENTRIES * 8, HEADER_CACHE_ENTRIES),
                    blobs: Cache::new(BLOB_CACHE_BYTES, BLOB_CACHE_ENTRIES),
                    readers: Arc::new(Mutex::new(HashMap::new())),
                    raw_blobs: None,
                    metrics: SnapshotMetrics::default(),
                    spool_directory: run_directory.to_path_buf(),
                    _local_object_bridge: local_bridge,
                }),
                _pin: pin,
            }),
        })
    }
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.inner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot object cache poisoned"))
    }
    pub fn stop_remote_transport(&self) -> Result<()> {
        crate::git::stop_operations();
        let state = self.state()?;
        if let Some(remote) = &state.remote {
            remote.stop_transport()?;
        }
        Ok(())
    }
    pub fn remote_failure(&self) -> Option<String> {
        self.state()
            .ok()
            .and_then(|s| s.remote.as_ref().and_then(|r| r.failure()))
    }
    fn metadata(item: &TreeItem, path: &Path) -> Result<Entry> {
        let kind = match item.mode {
            0o040000 => EntryKind::Directory,
            0o100644 | 0o100755 => EntryKind::File,
            0o120000 => EntryKind::Symlink,
            0o160000 => bail!("submodules are unsupported: {}", path.display()),
            mode => bail!("unsupported Git mode {mode:o}: {}", path.display()),
        };
        Ok(Entry {
            kind,
            mode: item.mode,
            oid: item.oid,
            size: (kind == EntryKind::Directory).then_some(0),
        })
    }
    fn lookup(&self, state: &mut State, path: &Path) -> Result<Option<Entry>> {
        let mut entry = Entry {
            kind: EntryKind::Directory,
            mode: 0o040000,
            oid: self.tree_oid,
            size: Some(0),
        };
        for component in path.components() {
            let Component::Normal(name) = component else {
                bail!("snapshot path must be relative and contain no parent components");
            };
            if entry.kind != EntryKind::Directory {
                bail!("snapshot ancestor is not a directory: {}", path.display());
            }
            let tree = state.tree(entry.oid)?;
            let Ok(index) = tree.binary_search_by(|item| item.name.as_os_str().cmp(name)) else {
                return Ok(None);
            };
            entry = Self::metadata(&tree[index], path)?;
        }
        Ok(Some(entry))
    }
    /// Resolve only a path's ancestor trees and requested object's header.
    pub fn entry(&self, path: &Path) -> Result<Option<Entry>> {
        let mut state = self.state()?;
        let Some(mut entry) = self.lookup(&mut state, path)? else {
            return Ok(None);
        };
        if entry.kind != EntryKind::Directory {
            entry.size = Some(state.size(entry.oid)?);
        }
        Ok(Some(entry))
    }
    /// Enumerate one directory without reading child tree objects or blob headers.
    pub fn children(&self, path: &Path) -> Result<Vec<(OsString, Entry)>> {
        let mut state = self.state()?;
        let entry = self
            .lookup(&mut state, path)?
            .context("snapshot directory not found")?;
        if entry.kind != EntryKind::Directory {
            bail!("snapshot path is not a directory: {}", path.display());
        }
        state
            .tree(entry.oid)?
            .iter()
            .map(|item| {
                Ok((
                    item.name.clone(),
                    if item.mode == 0o160000 {
                        // A gitlink name may be listed, but lookup/stat/open of
                        // that name must fail instead of presenting a checkout.
                        Entry {
                            kind: EntryKind::Directory,
                            mode: item.mode,
                            oid: item.oid,
                            size: Some(0),
                        }
                    } else {
                        Self::metadata(item, &path.join(&item.name))?
                    },
                ))
            })
            .collect()
    }
    pub(crate) fn prepare_materialization(&self, children: &[(OsString, Entry)]) -> Result<()> {
        let mut state = self.state()?;
        let State { repo, remote, .. } = &mut *state;
        let Some(remote) = remote else { return Ok(()) };
        // Bounded requests avoid argv limits and O(repository-size) request RAM.
        // Repeated OIDs in a directory are deduplicated within each batch and
        // later batches reuse already prepared objects.
        for children in children.chunks(256) {
            let oids: Vec<_> = children
                .iter()
                .filter(|(_, entry)| entry.mode != 0o160000)
                .map(|(_, entry)| entry.oid)
                .collect();
            remote.ensure_batch_in(repo, &oids)?;
        }
        Ok(())
    }
    /// Call on the first actual read, then keep the reader for the open handle.
    pub fn set_spool_directory(&self, path: &Path) -> Result<()> {
        let mut state = self.state()?;
        // Own storage while the validated private directory exists.
        // Later upper-layer failures must not turn policy reads into unrelated
        // object-storage failures merely because that directory was removed.
        if state.remote.is_none() && state.raw_blobs.is_none() {
            // Named parts stay outside the execution view.
            // It remains inside the invocation's owned directory for recovery.
            let storage = path.parent().context("missing private storage parent")?;
            state.raw_blobs = Some(crate::raw_blob::RawBlobs::new(storage)?);
        }
        state.spool_directory = path.to_path_buf();
        Ok(())
    }
    pub fn open(&self, oid: Oid) -> Result<BlobReader> {
        self.state()?.open(oid)
    }
    pub fn read(&self, oid: Oid, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.open(oid)?.read(offset, size)
    }
    pub fn read_link(&self, oid: Oid) -> Result<Vec<u8>> {
        self.state()?.with_blob(oid, |bytes| {
            if bytes.is_empty() || bytes.contains(&0) {
                bail!("unusable symlink target");
            }
            Ok(bytes.to_vec())
        })
    }
    /// Parse a demanded ignore blob directly, without retaining a second copy.
    pub(crate) fn with_blob<T>(
        &self,
        oid: Oid,
        consume: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        self.state()?.with_blob(oid, consume)
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn record_generated_io(&self, write: bool) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if write {
            state.metrics.generated_write_callbacks += 1;
        } else {
            state.metrics.generated_read_callbacks += 1;
        }
    }
    #[cfg(all(feature = "test-metrics", target_os = "linux"))]
    pub(crate) fn record_fuse_request(&self, operation: &'static str) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        *state.metrics.fuse_requests.entry(operation).or_default() += 1;
    }
    #[cfg(all(feature = "test-metrics", target_os = "linux"))]
    pub(crate) fn record_passthrough(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metrics
            .passthrough_handles += 1;
    }
    pub fn metrics(&self) -> SnapshotMetrics {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut metrics = state.metrics.clone();
        if let Some(remote) = &state.remote {
            metrics.remote_fetches = remote.completed_fetches();
            metrics.remote_fetch_operations = remote.fetch_operations();
            metrics.remote_decode_operations = remote.decode_operations();
            metrics.remote_raw_blob_imports = remote.raw_blobs.entries();
            metrics.remote_raw_blob_bytes = remote.raw_blobs.bytes();
            metrics.retained_raw_blob_bytes = remote.raw_blobs.bytes();
            metrics.retained_raw_blob_entries = remote.raw_blobs.entries();
        }
        metrics.blob_cache_bytes = state.blobs.bytes;
        metrics.blob_cache_entries = state.blobs.values.len();
        metrics.blob_cache_limit = state.blobs.byte_limit;
        metrics.tree_cache_bytes = state.trees.bytes;
        metrics.tree_cache_entries = state.trees.values.len();
        metrics.tree_cache_limit = state.trees.byte_limit;
        metrics.header_cache_entries = state.headers.values.len();
        if let Some(raw) = &state.raw_blobs {
            metrics.retained_raw_blob_bytes += raw.bytes();
            metrics.retained_raw_blob_entries += raw.entries();
        }
        // Drop the registry lock before dropping any upgraded reader: its last
        // owner removes itself from that registry, even during metrics sampling.
        let readers: Vec<_> = state
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for reader in readers {
            metrics.active_blob_readers += 1;
            if let BlobData::Stored(blob) = &reader.data {
                metrics.active_spool_bytes = metrics.active_spool_bytes.saturating_add(blob.len());
            }
        }
        metrics
    }
    #[cfg(feature = "test-metrics")]
    pub fn available_objects(&self) -> Result<Vec<Oid>> {
        let state = self.state()?;
        let mut objects = Vec::new();
        state.repo.odb()?.foreach(|oid| {
            objects.push(*oid);
            true
        })?;
        if let Some(raw) = &state.raw_blobs {
            objects.extend(raw.oids()?);
        }
        if let Some(remote) = &state.remote {
            objects.extend(remote.raw_blobs.oids()?);
        }
        objects.sort_unstable();
        objects.dedup();
        Ok(objects)
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn record_file_sync(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metrics
            .file_sync_callbacks += 1;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use tempfile::TempDir;
    fn fixture() -> (TempDir, Oid) {
        let tmp = TempDir::new().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let blob = repo.blob(b"original\n").unwrap();
        let target = repo.blob(b"script").unwrap();
        let mut nested = repo.treebuilder(None).unwrap();
        nested.insert("child", blob, 0o100644).unwrap();
        let nested_oid = nested.write().unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("script", blob, 0o100755).unwrap();
        builder.insert("naïve space", blob, 0o100644).unwrap();
        builder.insert("link", target, 0o120000).unwrap();
        builder.insert("dir", nested_oid, 0o040000).unwrap();
        let oid = builder.write().unwrap();
        let sig = Signature::now("Test", "test@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "initial",
            &repo.find_tree(oid).unwrap(),
            &[],
        )
        .unwrap();
        (tmp, oid)
    }
    #[test]
    fn reads_objects_modes_links_and_cleans_pin() {
        let (tmp, oid) = fixture();
        std::fs::write(tmp.path().join("script"), b"dirty checkout").unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "test-pin").unwrap();
        assert_eq!(snap.tree_oid, oid);
        for path in ["naïve space", "script"] {
            let entry = snap.entry(Path::new(path)).unwrap().unwrap();
            assert_eq!(entry.size, Some(9));
            assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"original\n");
        }
        assert_eq!(
            snap.entry(Path::new("script")).unwrap().unwrap().mode,
            0o100755
        );
        let link = snap.entry(Path::new("link")).unwrap().unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(snap.read_link(link.oid).unwrap(), b"script");
        let repo = Repository::open(tmp.path()).unwrap();
        assert_eq!(
            repo.find_reference("refs/git-on/test-pin")
                .unwrap()
                .target(),
            Some(oid)
        );
        assert!(Snapshot::load(tmp.path(), OsStr::new("HEAD"), "test-pin").is_err());
        let shared = snap.clone();
        drop(snap);
        assert!(repo.find_reference("refs/git-on/test-pin").is_ok());
        drop(shared);
        assert!(repo.find_reference("refs/git-on/test-pin").is_err());
        assert_eq!(
            std::fs::read(tmp.path().join("script")).unwrap(),
            b"dirty checkout"
        );
    }
    #[test]
    fn accepts_raw_tree_and_subtree() {
        let (tmp, oid) = fixture();
        let snap = Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "raw").unwrap();
        assert_eq!(snap.tree_oid, oid);
        let sub = Snapshot::load(tmp.path(), OsStr::new("HEAD:dir"), "sub").unwrap();
        assert!(sub.entry(Path::new("child")).unwrap().is_some());
        assert!(sub.entry(Path::new("dir")).unwrap().is_none());
        assert!(sub.entry(Path::new("")).unwrap().is_some());
    }
    #[test]
    fn rejects_lfs_submodules_on_demand_not_at_startup() {
        for submodule in [false, true] {
            let (tmp, _) = fixture();
            let repo = Repository::open(tmp.path()).unwrap();
            let oid = if submodule {
                repo.head().unwrap().target().unwrap()
            } else {
                repo.blob(
                    format!(
                        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                        "a".repeat(64)
                    )
                    .as_bytes(),
                )
                .unwrap()
            };
            let ordinary = repo.blob(b"works").unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder
                .insert(
                    "unsupported",
                    oid,
                    if submodule { 0o160000 } else { 0o100644 },
                )
                .unwrap();
            builder.insert("ordinary", ordinary, 0o100644).unwrap();
            let tree = builder.write().unwrap();
            let snap = Snapshot::load(tmp.path(), OsStr::new(&tree.to_string()), "demand").unwrap();
            assert_eq!(snap.metrics().blob_loads, 0);
            assert!(snap.entry(Path::new("ordinary")).unwrap().is_some());
            if submodule {
                assert!(snap.entry(Path::new("unsupported")).is_err());
            } else {
                let entry = snap.entry(Path::new("unsupported")).unwrap().unwrap();
                assert!(snap.read(entry.oid, 20, 1).is_err());
            }
            assert!(repo.find_reference("refs/git-on/demand").is_ok());
            drop(snap);
            assert!(repo.find_reference("refs/git-on/demand").is_err());
        }
    }
    #[test]
    fn lfs_detection_requires_complete_valid_pointer_structure() {
        let hash = "0123456789abcdef".repeat(4);
        let version = "version https://git-lfs.github.com/spec/v1\n";
        let pointer = format!("{version}oid sha256:{hash}\nsize 123\n");
        for valid in [
            pointer.clone(),
            pointer.replace('\n', "\r\n"),
            pointer.trim_end_matches('\n').to_owned(),
            pointer.replace("git-lfs", "hawser"),
            pointer.replace("https://git-lfs.github.com/spec/v1", "http://git-media.io/v/2"),
            pointer.replace("size 123", "size 0"),
            format!("{version}ext-0-compress sha256:{hash}\next-1-encrypt sha256:{hash}\noid sha256:{hash}\nsize 123\n"),
            format!("{pointer}x-note preserved metadata\n"),
        ] {
            assert!(is_lfs_pointer(valid.as_bytes()), "{valid:?}");
            assert!(State::validate_blob(valid.as_bytes()).is_err());
        }
        for ordinary in [
            String::new(),
            version.to_owned(),
            format!("{version}This is documentation, not a pointer.\n"),
            format!("{version}oid sha256:{hash}\n"),
            format!("{version}size 123\n"),
            pointer.replace(&hash, "abc"),
            pointer.replace(&hash, &"a".repeat(65)),
            pointer.replace(&hash, &hash.to_uppercase()),
            pointer.replace("sha256:", "sha1:"),
            pointer.replace("size 123", "size -1"),
            pointer.replace("size 123", "size 1.5"),
            pointer.replace("size 123", "size 9223372036854775808"),
            pointer.replace("size 123", "size 123 bytes"),
            pointer.replace("size 123", "size  123"),
            pointer.replace("size 123", "size\t123"),
            pointer.replace("/spec/v1", "/spec/v2"),
            format!("{version}size 123\noid sha256:{hash}\n"),
            format!("{version}oid sha256:{hash}\noid sha256:{hash}\nsize 123\n"),
            format!("{pointer}size 123\n"),
            format!("{pointer}This is documentation, not a pointer.\n"),
            format!("{version}ext-0-compress sha256:abc\noid sha256:{hash}\nsize 123\n"),
            format!("{version}ext-0-compress sha256:{hash}\next-0-encrypt sha256:{hash}\noid sha256:{hash}\nsize 123\n"),
            format!("{pointer}x-note contains\rcarriage return\n"),
            format!("{pointer}x-note nul\0byte\n"),
            format!("{pointer}x-note {}\n", "x".repeat(1024)),
        ] {
            assert!(!is_lfs_pointer(ordinary.as_bytes()), "{ordinary:?}");
            assert!(State::validate_blob(ordinary.as_bytes()).is_ok());
        }
        for length in [1023, 1024] {
            let prefix = format!("{pointer}x-note ");
            let boundary = format!("{prefix}{}\n", "x".repeat(length - prefix.len() - 1));
            assert_eq!(boundary.len(), length);
            assert_eq!(is_lfs_pointer(boundary.as_bytes()), length < 1024);
        }
        let mut binary = pointer.into_bytes();
        binary.extend_from_slice(b"x-note invalid UTF-8: \xff\n");
        assert!(!is_lfs_pointer(&binary));
        assert!(State::validate_blob(&binary).is_ok());
    }
    #[test]
    fn lfs_prefix_in_ordinary_blobs_is_readable_through_both_read_paths() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let text =
            b"version https://git-lfs.github.com/spec/v1\nThis is documentation, not a pointer.\n";
        let ordinary = repo.blob(text).unwrap();
        let pointer = repo
            .blob(
                format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 123\n",
                    "a".repeat(64)
                )
                .as_bytes(),
            )
            .unwrap();
        for first_open in [false, true] {
            let snap = Snapshot::load(
                tmp.path(),
                OsStr::new("HEAD"),
                if first_open { "lfs-open" } else { "lfs-read" },
            )
            .unwrap();
            if first_open {
                assert_eq!(snap.open(ordinary).unwrap().read(0, 4096).unwrap(), text);
            } else {
                assert_eq!(snap.read(ordinary, 0, 4096).unwrap(), text);
            }
            // Both the uncached and cached paths preserve the original bytes.
            assert_eq!(snap.read(ordinary, 0, 4096).unwrap(), text);
            assert_eq!(snap.open(ordinary).unwrap().read(0, 4096).unwrap(), text);
            assert!(snap.read(pointer, 0, 4096).is_err());
            assert!(snap.open(pointer).is_err());
            assert_eq!(snap.read(ordinary, 0, 4096).unwrap(), text);
        }
    }
    #[test]
    fn invocation_from_worktree_subdirectory() {
        let (tmp, oid) = fixture();
        let directory = tmp.path().join("some/deep/path");
        std::fs::create_dir_all(&directory).unwrap();
        let snap = Snapshot::load(&directory, OsStr::new("HEAD"), "discovery").unwrap();
        assert_eq!(snap.tree_oid, oid);
    }
    #[test]
    fn durable_pin_record_precedes_pin_and_failed_record_leaves_no_reference() {
        let (tmp, oid) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let result = Snapshot::load_with_pin_record(
            tmp.path(),
            OsStr::new(&oid.to_string()),
            "record",
            |dir, pin, tree| {
                assert_eq!(dir, repo.path());
                assert_eq!(tree, oid);
                assert!(repo.find_reference(pin).is_err());
                anyhow::bail!("simulated manifest failure")
            },
        );
        assert!(result.unwrap_err().to_string().contains("manifest failure"));
        assert!(repo.find_reference("refs/git-on/record").is_err());
    }
    #[test]
    fn startup_listing_lookup_and_read_are_separately_lazy() {
        let (tmp, _) = fixture();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "lazy").unwrap();
        assert_eq!(snap.metrics().tree_reads, 0);
        assert_eq!(snap.metrics().header_reads, 0);
        assert_eq!(snap.metrics().blob_loads, 0);
        let listing = snap.children(Path::new("")).unwrap();
        assert_eq!(listing.len(), 4);
        assert!(listing
            .iter()
            .filter(|(_, e)| e.kind != EntryKind::Directory)
            .all(|(_, e)| e.size.is_none()));
        assert_eq!(snap.metrics().tree_reads, 1);
        assert_eq!(snap.metrics().header_reads, 0);
        assert_eq!(snap.metrics().blob_loads, 0);
        let entry = snap.entry(Path::new("dir/child")).unwrap().unwrap();
        assert_eq!(snap.metrics().tree_reads, 2);
        assert_eq!(snap.metrics().header_reads, 1);
        assert_eq!(snap.metrics().blob_loads, 0);
        assert_eq!(snap.read(entry.oid, 2, 3).unwrap(), b"igi");
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().blob_bytes, 9);
        assert_eq!(snap.read(entry.oid, 8, 100).unwrap(), b"\n");
        assert!(snap.read(entry.oid, u64::MAX, 100).unwrap().is_empty());
        assert_eq!(snap.metrics().blob_loads, 1);
        snap.entry(Path::new("dir/child")).unwrap();
        assert_eq!(snap.metrics().header_reads, 1);
        assert_eq!(snap.metrics().tree_reads, 2);
    }
    #[test]
    fn unvisited_missing_objects_do_not_block_startup_or_other_reads() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let data = repo.blob(b"this object will disappear").unwrap();
        let mut child = repo.treebuilder(None).unwrap();
        child.insert("missing", data, 0o100644).unwrap();
        let missing_tree = child.write().unwrap();
        let good = repo.blob(b"good").unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("unvisited", missing_tree, 0o040000).unwrap();
        root.insert("good", good, 0o100644).unwrap();
        let oid = root.write().unwrap();
        for remove in [data, missing_tree] {
            let hex = remove.to_string();
            std::fs::remove_file(repo.path().join("objects").join(&hex[..2]).join(&hex[2..]))
                .unwrap();
        }
        let snap = Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "missing").unwrap();
        assert_eq!(snap.children(Path::new("")).unwrap().len(), 2);
        let entry = snap.entry(Path::new("good")).unwrap().unwrap();
        assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"good");
        assert!(snap.children(Path::new("unvisited")).is_err());
    }
    #[test]
    fn reader_registry_releases_closed_entries_without_another_open() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "reader-slots").unwrap();
        let oid = repo.blob(b"reader").unwrap();
        let reader = snap.open(oid).unwrap();
        assert_eq!(snap.metrics().active_blob_readers, 1);
        drop(reader);
        assert_eq!(snap.metrics().active_blob_readers, 0);
        assert_eq!(snap.state().unwrap().readers.lock().unwrap().len(), 0);
    }
    #[test]
    fn active_readers_do_not_pin_evicted_payloads_in_ram() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "active-memory").unwrap();
        let mut readers = Vec::new();
        for byte in 0..12u8 {
            let oid = repo.blob(&vec![byte; 1024 * 1024]).unwrap();
            readers.push(snap.open(oid).unwrap());
        }
        let state = snap.state().unwrap();
        assert!(
            state
                .blobs
                .values
                .iter()
                .all(|(_, (bytes, _))| Arc::strong_count(bytes) == 1),
            "active readers retained full payloads outside the bounded RAM cache"
        );
        drop(state);
        assert_eq!(readers[0].read(0, 1).unwrap(), [0]);
        assert_eq!(snap.metrics().blob_loads, 12);
    }
    #[test]
    fn blob_cache_is_bounded_and_large_partial_reads_bypass_retention() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "bounded").unwrap();
        let mut ids = Vec::new();
        for n in 0..12u8 {
            let data = vec![n; 1024 * 1024];
            let oid = repo.blob(&data).unwrap();
            ids.push(oid);
            assert_eq!(snap.read(oid, 8192, 1).unwrap(), vec![n]);
            assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        }
        assert_eq!(snap.metrics().blob_cache_bytes, BLOB_CACHE_BYTES);
        let loads = snap.metrics().blob_loads;
        snap.read(*ids.last().unwrap(), 0, 1).unwrap();
        assert_eq!(snap.metrics().blob_loads, loads);
        snap.read(ids[0], 0, 1).unwrap();
        assert_eq!(
            snap.metrics().blob_loads,
            loads,
            "eviction must not unpack prepared data again"
        );
        let large = repo.blob(&vec![b'x'; BLOB_CACHE_BYTES + 1]).unwrap();
        let retained = snap.metrics().blob_cache_bytes;
        assert_eq!(snap.read(large, 1000, 5).unwrap(), b"xxxxx");
        assert_eq!(snap.metrics().blob_cache_bytes, retained);
        assert!(!snap.state().unwrap().blobs.values.contains(&large));
    }
    #[test]
    fn cache_entry_limits_bound_empty_blobs_and_directory_metadata() {
        let mut cache = Cache::new(4, 2);
        let ids = [
            Oid::from_bytes(&[1; 20]).unwrap(),
            Oid::from_bytes(&[2; 20]).unwrap(),
            Oid::from_bytes(&[3; 20]).unwrap(),
        ];
        cache.insert(ids[0], (), 0);
        cache.insert(ids[1], (), 0);
        cache.get(ids[0]);
        cache.insert(ids[2], (), 0);
        assert!(cache.get(ids[1]).is_none());
        assert_eq!(cache.values.len(), 2);
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "tree-bounds").unwrap();
        let blob = repo.blob(b"x").unwrap();
        for i in 0..TREE_CACHE_ENTRIES + 10 {
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert(format!("file-{i}"), blob, 0o100644).unwrap();
            snap.state().unwrap().tree(tree.write().unwrap()).unwrap();
        }
        let stats = snap.metrics();
        assert_eq!(stats.tree_cache_entries, TREE_CACHE_ENTRIES);
        assert!(stats.tree_cache_bytes <= TREE_CACHE_BYTES);
    }
    #[test]
    fn active_large_blob_reader_decodes_once_and_releases_active_accounting() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "spool").unwrap();
        let size = BLOB_CACHE_BYTES + 123;
        let oid = repo.blob(&vec![b'x'; size]).unwrap();
        let reader = snap.open(oid).unwrap();
        let second = snap.open(oid).unwrap();
        assert_eq!(snap.metrics().streamed_blob_loads, 1);
        for offset in (0..size as u64).step_by(4096) {
            assert_eq!(
                reader.read(offset, 4096).unwrap(),
                vec![b'x'; (size as u64 - offset).min(4096) as usize]
            );
        }
        assert_eq!(second.read(size as u64 - 2, 100).unwrap(), b"xx");
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().spool_writes, 1);
        assert_eq!(snap.metrics().active_spool_bytes, size as u64);
        assert_eq!(snap.metrics().blob_cache_bytes, 0);
        drop(reader);
        assert_eq!(snap.metrics().active_spool_bytes, size as u64);
        drop(second);
        assert_eq!(snap.metrics().active_spool_bytes, 0);
        assert_eq!(snap.metrics().active_blob_readers, 0);
    }
    #[test]
    fn large_blob_preparation_survives_close_and_reopen_with_bounded_memory() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let size = BLOB_CACHE_BYTES + 123;
        let oid = repo.blob(&vec![b'x'; size]).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "reopen").unwrap();
        for offset in [0, size as u64 - 1, 123] {
            let reader = snap.open(oid).unwrap();
            assert_eq!(reader.read(offset, 1).unwrap(), b"x");
            drop(reader);
            assert_eq!(snap.metrics().active_blob_readers, 0);
            assert_eq!(snap.metrics().blob_cache_bytes, 0);
            assert_eq!(snap.metrics().spool_writes, 1);
            assert_eq!(snap.metrics().blob_loads, 1);
        }
    }
    #[test]
    fn malformed_symlink_targets_are_rejected_on_readlink() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "links").unwrap();
        for bytes in [b"".as_slice(), b"bad\0target"] {
            assert!(snap.read_link(repo.blob(bytes).unwrap()).is_err());
        }
    }
}
