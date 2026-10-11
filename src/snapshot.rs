//! Immutable, demand-loaded objects from the pinned Git tree.
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid, Repository};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::Write,
    os::unix::{ffi::OsStringExt, fs::FileExt},
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
    pub spool_writes: u64,
    pub spool_bytes: u64,
}

enum BlobData {
    Memory(Arc<[u8]>),
    // tempfile() unlinks immediately; only active handles keep this file alive.
    Spool { file: File, size: u64 },
}
/// A live source-file reader. Large objects decode once into an unlinked spool;
/// their subsequent partial reads do not decode or copy the full object again.
#[derive(Clone)]
pub struct BlobReader(Arc<BlobData>);
impl BlobReader {
    pub fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        match &*self.0 {
            BlobData::Memory(bytes) => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                let end = start.saturating_add(size as usize).min(bytes.len());
                Ok(bytes[start..end].to_vec())
            }
            BlobData::Spool { file, size: length } => {
                let length = length.saturating_sub(offset).min(u64::from(size));
                let mut bytes = vec![0; length as usize];
                if !bytes.is_empty() {
                    file.read_exact_at(&mut bytes, offset)
                        .context("read source blob spool")?;
                }
                Ok(bytes)
            }
        }
    }
}

/// A byte- and entry-bounded LRU. Values exceeding the budget are never retained.
struct Cache<V> {
    values: BTreeMap<Oid, (V, usize, u64)>,
    recency: BTreeMap<u64, Oid>,
    bytes: usize,
    clock: u64,
    byte_limit: usize,
    entry_limit: usize,
}
impl<V: Clone> Cache<V> {
    fn new(byte_limit: usize, entry_limit: usize) -> Self {
        Self {
            values: BTreeMap::new(),
            recency: BTreeMap::new(),
            bytes: 0,
            clock: 0,
            byte_limit,
            entry_limit,
        }
    }
    fn get(&mut self, oid: Oid) -> Option<V> {
        let (value, _, used) = self.values.get_mut(&oid)?;
        self.recency.remove(used);
        self.clock = self.clock.saturating_add(1);
        *used = self.clock;
        self.recency.insert(*used, oid);
        Some(value.clone())
    }
    fn insert(&mut self, oid: Oid, value: V, bytes: usize) {
        if bytes > self.byte_limit || self.entry_limit == 0 {
            return;
        }
        if let Some((_, old_bytes, used)) = self.values.remove(&oid) {
            self.bytes -= old_bytes;
            self.recency.remove(&used);
        }
        while self.values.len() >= self.entry_limit
            || self.bytes > self.byte_limit.saturating_sub(bytes)
        {
            let (_, oldest) = self.recency.pop_first().expect("cache has a victim");
            let (_, removed_bytes, _) = self.values.remove(&oldest).unwrap();
            self.bytes -= removed_bytes;
        }
        self.clock = self.clock.saturating_add(1);
        self.values.insert(oid, (value, bytes, self.clock));
        self.recency.insert(self.clock, oid);
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
    readers: BTreeMap<Oid, Weak<BlobData>>,
    metrics: SnapshotMetrics,
    spool_directory: PathBuf,
}
impl State {
    fn ensure(&mut self, oid: Oid) -> Result<()> {
        if let Some(remote) = &mut self.remote {
            remote.ensure(oid)?;
            self.repo.odb()?.refresh()?;
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
        self.readers.retain(|_, value| value.strong_count() != 0);
        if let Some(reader) = self.readers.get(&oid).and_then(Weak::upgrade) {
            self.metrics.cache_hits += 1;
            return Ok(BlobReader(reader));
        }
        let data = if let Some(bytes) = self.blobs.get(oid) {
            self.metrics.cache_hits += 1;
            BlobData::Memory(bytes)
        } else {
            self.metrics.blob_loads += 1;
            self.ensure(oid)?;
            let blob = self.repo.find_blob(oid).context("read snapshot blob")?;
            let bytes = blob.content();
            self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(bytes.len() as u64);
            Self::validate_blob(bytes)?;
            if bytes.len() <= self.blobs.byte_limit {
                let bytes = Arc::<[u8]>::from(bytes);
                drop(blob);
                self.blobs.insert(oid, bytes.clone(), bytes.len());
                BlobData::Memory(bytes)
            } else {
                let mut file = tempfile::tempfile_in(&self.spool_directory)
                    .context("create private source blob spool")?;
                file.write_all(bytes).context("write source blob spool")?;
                let size = bytes.len() as u64;
                self.metrics.spool_writes += 1;
                self.metrics.spool_bytes = self.metrics.spool_bytes.saturating_add(size);
                BlobData::Spool { file, size }
            }
        };
        let reader = Arc::new(data);
        self.readers.insert(oid, Arc::downgrade(&reader));
        Ok(BlobReader(reader))
    }
    fn validate_blob(bytes: &[u8]) -> Result<()> {
        if is_lfs_pointer(bytes) {
            bail!("Git LFS pointer blobs are unsupported");
        }
        Ok(())
    }
    /// No retained duplicate of a cached payload. For oversized blobs the
    /// operation consumes libgit2's borrowed bytes and only its result is copied.
    fn with_blob<T>(&mut self, oid: Oid, consume: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        if let Some(bytes) = self.blobs.get(oid) {
            self.metrics.cache_hits += 1;
            return consume(&bytes);
        }
        self.metrics.blob_loads += 1;
        self.ensure(oid)?;
        let blob = self.repo.find_blob(oid).context("read snapshot blob")?;
        let bytes = blob.content();
        self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(bytes.len() as u64);
        Self::validate_blob(bytes)?;
        if bytes.len() <= self.blobs.byte_limit {
            let bytes = Arc::<[u8]>::from(bytes);
            drop(blob);
            self.blobs.insert(oid, bytes.clone(), bytes.len());
            consume(&bytes)
        } else {
            consume(bytes)
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
            repo_path, None, treeish, false, repo_path, run_id, record,
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
        let spec = treeish.to_str().context("tree-ish must be valid UTF-8")?;
        let (revision, subtree) = if verify || source.is_some() {
            match spec.split_once(':') {
                Some((revision, path)) => (revision, Some(path)),
                None => (spec, None),
            }
        } else {
            (spec, None)
        };
        let (repo, mut remote, oid, git) = if let Some(source) = source {
            let (mut remote, advertisement) =
                crate::remote::Remote::open(repo_path, source, run_directory)?;
            let oid = crate::remote::Remote::resolve(&advertisement, revision)?;
            remote.ensure(oid)?;
            (Repository::open_bare(&remote.dir)?, Some(remote), oid, None)
        } else {
            let git = crate::git::Git::capture(repo_path)?;
            let oid = git.resolve(OsStr::new(revision))?;
            (
                Repository::discover(repo_path).context("discover Git repository")?,
                None,
                oid,
                Some(git),
            )
        };
        let ensure = |remote: &mut Option<crate::remote::Remote>, oid| -> Result<()> {
            if let Some(remote) = remote {
                remote.ensure(oid)?;
                repo.odb()?.refresh()?;
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
        let mut tag_depth = 0;
        let mut tree_oid = loop {
            ensure(&mut remote, selected)?;
            let object = repo.find_object(selected, None)?;
            match object.kind() {
                Some(ObjectType::Tree) => break selected,
                Some(ObjectType::Commit) => break object.as_commit().unwrap().tree_id(),
                Some(ObjectType::Tag) => {
                    tag_depth += 1;
                    if tag_depth > 64 {
                        bail!("annotated tag chain exceeds 64 objects");
                    }
                    selected = object.as_tag().unwrap().target_id();
                }
                _ => bail!("revision does not resolve to a tree"),
            }
        };
        if let Some(subtree) = subtree {
            if !subtree.is_empty() {
                // Git accepts one trailing slash for a tree, but interior empty
                // or dot components are literal invalid tree paths. Path's
                // components() would silently normalize those spellings.
                for name in subtree.strip_suffix('/').unwrap_or(subtree).split('/') {
                    if name.is_empty() || name == "." || name == ".." {
                        bail!("REV:path must select a relative directory without empty, . or .. components");
                    }
                    ensure(&mut remote, tree_oid)?;
                    let tree = repo.find_tree(tree_oid)?;
                    let entry = tree
                        .get_name(name)
                        .context("REV:path directory does not exist")?;
                    if entry.kind() != Some(ObjectType::Tree) {
                        bail!("REV:path must select a directory");
                    }
                    tree_oid = entry.id();
                }
            }
        }
        // libgit2 requires a reference target to exist. Fetch only the selected
        // root tree, not its descendants, before installing the GC pin.
        ensure(&mut remote, tree_oid)?;
        let pin_ref = format!("refs/git-on/{run_id}");
        if run_id.is_empty() || !git2::Reference::is_valid_name(&pin_ref) {
            bail!("invalid snapshot run ID");
        }
        record(repo.path(), &pin_ref, tree_oid)?;
        repo.reference(&pin_ref, tree_oid, false, "git-on snapshot pin")
            .context("pin snapshot tree against garbage collection")?;
        let git_dir = repo.path().to_path_buf();
        let pin = Pin {
            git_dir: git_dir.clone(),
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
                    readers: BTreeMap::new(),
                    metrics: SnapshotMetrics::default(),
                    spool_directory: std::env::temp_dir(),
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
    pub fn stop_remote_transport(&self) {
        if let Ok(state) = self.state() {
            if let Some(remote) = &state.remote {
                remote.stop_transport();
            }
        }
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
    /// Call on the first actual read, then keep the reader for the open handle.
    pub fn set_spool_directory(&self, path: &Path) -> Result<()> {
        self.state()?.spool_directory = path.to_path_buf();
        Ok(())
    }
    pub fn open(&self, oid: Oid) -> Result<BlobReader> {
        self.state()?.open(oid)
    }
    pub fn read(&self, oid: Oid, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.state()?.with_blob(oid, |bytes| {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let end = start.saturating_add(size as usize).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        })
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
    pub fn metrics(&self) -> SnapshotMetrics {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut metrics = state.metrics.clone();
        metrics.blob_cache_bytes = state.blobs.bytes;
        metrics.blob_cache_entries = state.blobs.values.len();
        metrics.blob_cache_limit = state.blobs.byte_limit;
        metrics.tree_cache_bytes = state.trees.bytes;
        metrics.tree_cache_entries = state.trees.values.len();
        metrics.tree_cache_limit = state.trees.byte_limit;
        metrics.header_cache_entries = state.headers.values.len();
        for reader in state.readers.values().filter_map(Weak::upgrade) {
            metrics.active_blob_readers += 1;
            if let BlobData::Spool { size, .. } = &*reader {
                metrics.active_spool_bytes = metrics.active_spool_bytes.saturating_add(*size);
            }
        }
        metrics
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
        assert_eq!(snap.metrics().blob_loads, loads + 1);
        let large = repo.blob(&vec![b'x'; BLOB_CACHE_BYTES + 1]).unwrap();
        let retained = snap.metrics().blob_cache_bytes;
        assert_eq!(snap.read(large, 1000, 5).unwrap(), b"xxxxx");
        assert_eq!(snap.metrics().blob_cache_bytes, retained);
        assert!(!snap.state().unwrap().blobs.values.contains_key(&large));
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
    fn active_large_blob_reader_decodes_once_and_releases_spool() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "spool").unwrap();
        let size = BLOB_CACHE_BYTES + 123;
        let oid = repo.blob(&vec![b'x'; size]).unwrap();
        let reader = snap.open(oid).unwrap();
        let second = snap.open(oid).unwrap();
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
    fn malformed_symlink_targets_are_rejected_on_readlink() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "links").unwrap();
        for bytes in [b"".as_slice(), b"bad\0target"] {
            assert!(snap.read_link(repo.blob(bytes).unwrap()).is_err());
        }
    }
}
