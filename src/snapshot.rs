//! Immutable, demand-loaded objects from the pinned Git tree.
use crate::tree::{Directory, TreeItem};
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

pub const BLOB_CACHE_BYTES: usize = crate::raw_blob::MEMORY_BUDGET;

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
    #[cfg(feature = "test-metrics")]
    pub profile_spans: BTreeMap<&'static str, crate::profile::Summary>,
    /// Successful explicit native fetches, including initial ref resolution.
    pub remote_fetches: u64,
    pub remote_fetch_operations: u64,
    pub remote_decode_operations: u64,
    pub remote_session_fetch_operations: u64,
    pub remote_session_decode_operations: u64,
    pub remote_session_starts: u64,
    /// Native Git's retained received packs, possibly normalized by index-pack.
    pub remote_native_pack_bytes: u64,
    /// Exact received Git pack payloads on invocation-owned demand sessions.
    /// Neither counter includes protocol framing, TLS or other wire overhead.
    pub remote_session_pack_bytes: u64,
    pub tree_reads: u64,
    pub prepared_tree_entries: u64,
    pub retained_tree_bytes: u64,
    pub verification_metadata_bytes: usize,
    pub tree_index_probes: u64,
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

type ReaderRegistry = Mutex<HashMap<Oid, Weak<ActiveBlob>>>;
struct ActiveBlob {
    oid: Oid,
    data: crate::raw_blob::RawBlob,
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
        self.0.data.read(offset, size)
    }
    pub(crate) fn len(&self) -> u64 {
        self.0.data.len()
    }
}

struct State {
    repo: Repository,
    remote: Option<crate::remote::Remote>,
    headers: HashMap<Oid, u64>,
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
    fn raw_store(&self) -> Option<&crate::raw_blob::RawBlobs> {
        self.remote
            .as_ref()
            .map(|remote| &remote.raw_blobs)
            .or(self.raw_blobs.as_ref())
    }
    fn tree(&mut self, oid: Oid) -> Result<Arc<Directory>> {
        if let Some((tree, first)) = self.raw_store().and_then(|raw| raw.directory(oid)) {
            if first {
                self.metrics.tree_reads += 1;
            } else {
                self.metrics.cache_hits += 1;
            }
            return Ok(tree);
        }
        self.ensure(oid)?;
        if let Some((tree, first)) = self.raw_store().and_then(|raw| raw.directory(oid)) {
            self.metrics.tree_reads += u64::from(first);
            return Ok(tree);
        }
        if self.remote.is_none() && self.raw_blobs.is_none() {
            self.raw_blobs = Some(crate::raw_blob::RawBlobs::new(&self.spool_directory)?);
        }
        let odb = self.repo.odb()?;
        let storage = if let Some(remote) = &mut self.remote {
            &mut remote.raw_blobs
        } else {
            self.raw_blobs.as_mut().unwrap()
        };
        if let Ok((reader, size, kind)) = odb.reader(oid) {
            anyhow::ensure!(
                kind == ObjectType::Tree,
                "snapshot directory does not reference a tree"
            );
            storage.insert_tree(oid, size, reader, &self.spool_directory)?;
        } else {
            let object = odb.read(oid).context("decode snapshot tree")?;
            anyhow::ensure!(
                object.kind() == ObjectType::Tree,
                "snapshot directory does not reference a tree"
            );
            storage.insert_tree(oid, object.len(), object.data(), &self.spool_directory)?;
        }
        let (tree, first) = storage
            .directory(oid)
            .context("verified directory missing")?;
        self.metrics.tree_reads += u64::from(first);
        Ok(tree)
    }
    fn size(&mut self, oid: Oid) -> Result<u64> {
        if let Some(size) = self.headers.get(&oid).copied() {
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
            anyhow::ensure!(
                raw.kind() == ObjectType::Blob,
                "snapshot file does not reference a blob"
            );
            let size = raw.len();
            self.headers.insert(oid, size);
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
        self.headers.insert(oid, size);
        Ok(size)
    }
    fn open(&mut self, oid: Oid) -> Result<BlobReader> {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("snapshot_open", 0);
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
        let raw = if let Some(remote) = &mut self.remote {
            remote.ensure_in(&self.repo, oid)?;
            let (raw, first) = remote
                .raw_blobs
                .for_read(oid)?
                .context("snapshot file does not reference a blob")?;
            if first {
                self.metrics.blob_loads += 1;
                self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(raw.len());
                if raw.len() > crate::raw_blob::BLOCK as u64 {
                    self.metrics.spool_writes += 1;
                    self.metrics.spool_bytes = self.metrics.spool_bytes.saturating_add(raw.len());
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
            if size > crate::raw_blob::BLOCK {
                self.metrics.spool_writes += 1;
                self.metrics.spool_bytes = self.metrics.spool_bytes.saturating_add(size as u64);
                self.metrics.streamed_blob_loads += u64::from(streamed);
            }
            blob
        };
        anyhow::ensure!(
            raw.kind() == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        // Only a complete LFS pointer can need this bounded validation. A cold
        // partial read must not hydrate or authenticate the whole object.
        if raw.len() < 1024 {
            Self::validate_blob(&raw.read_all()?)?;
        }
        let data = raw;
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
        consume(&reader.0.data.read_all()?)
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
        let kind = |remote: &Option<crate::remote::Remote>, oid| -> Result<ObjectType> {
            if let Some(raw) = remote
                .as_ref()
                .map(|remote| remote.raw_blobs.get(oid))
                .transpose()?
                .flatten()
            {
                return Ok(raw.kind());
            }
            Ok(repo.odb()?.read_header(oid)?.1)
        };
        if verify {
            let git = remote.as_ref().map(|r| &r.git).or(git.as_ref()).unwrap();
            git.verify(repo.path(), oid, kind(&remote, oid)?)?;
        }
        let mut selected = oid;
        let tree_oid = loop {
            ensure(&mut remote, selected)?;
            match kind(&remote, selected)? {
                ObjectType::Tree => break selected,
                ObjectType::Commit => break repo.find_commit(selected)?.tree_id(),
                ObjectType::Tag => selected = repo.find_tag(selected)?.target_id(),
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
                    headers: HashMap::new(),
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
            let Some(item) = tree.lookup(name) else {
                return Ok(None);
            };
            entry = Self::metadata(&item, path)?;
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
            .children()?
            .into_iter()
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
                        Self::metadata(&item, &path.join(&item.name))?
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
        state.spool_directory = path
            .parent()
            .context("missing private storage parent")?
            .to_path_buf();
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
        #[cfg(feature = "test-metrics")]
        {
            metrics.profile_spans = crate::profile::summary();
        }
        if let Some(remote) = &state.remote {
            metrics.remote_fetches = remote.completed_fetches();
            metrics.remote_fetch_operations = remote.fetch_operations();
            metrics.remote_decode_operations = remote.decode_operations();
            (
                metrics.remote_session_fetch_operations,
                metrics.remote_session_decode_operations,
                metrics.remote_session_starts,
            ) = remote.session_operations();
            (
                metrics.remote_native_pack_bytes,
                metrics.remote_session_pack_bytes,
            ) = remote.received_pack_bytes();
            metrics.remote_raw_blob_imports = remote.raw_blobs.entries();
            metrics.remote_raw_blob_bytes = remote.raw_blobs.bytes();
            let (bytes, entries) = remote.raw_blobs.disk_metrics();
            metrics.retained_raw_blob_bytes = bytes;
            metrics.retained_raw_blob_entries = entries;
        }
        let (memory_bytes, memory_entries) = state
            .raw_store()
            .map_or((0, 0), |store| store.memory_metrics());
        metrics.blob_cache_bytes = memory_bytes;
        metrics.blob_cache_entries = memory_entries;
        metrics.blob_cache_limit = BLOB_CACHE_BYTES;
        if let Some(raw) = state.raw_store() {
            let (entries, bytes, count) = raw.tree_metrics();
            metrics.prepared_tree_entries = entries;
            metrics.tree_cache_bytes = bytes;
            metrics.tree_cache_entries = count;
            metrics.verification_metadata_bytes = raw.verification_bytes();
        }
        // Parsed metadata is invocation-local and retained for touched OIDs;
        // it is not part of the byte-budgeted payload LRU.
        metrics.tree_cache_limit = 0;
        metrics.header_cache_entries = state.headers.len();
        if let Some(raw) = &state.raw_blobs {
            let (bytes, entries) = raw.disk_metrics();
            metrics.retained_raw_blob_bytes += bytes;
            metrics.retained_raw_blob_entries += entries;
        }
        metrics.spool_bytes = metrics.retained_raw_blob_bytes;
        metrics.spool_writes = metrics.retained_raw_blob_entries;
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
            if reader.data.on_disk() {
                metrics.active_spool_bytes =
                    metrics.active_spool_bytes.saturating_add(reader.data.len());
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
    fn oversized_directory_metadata_is_prepared_once_for_repeated_lookups() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        for index in 0..40 {
            tree.insert(format!("file-{index:03}"), blob, 0o100644)
                .unwrap();
        }
        let tree = tree.write().unwrap();
        let snapshot = Snapshot::load(
            tmp.path(),
            OsStr::new(&tree.to_string()),
            "oversized-metadata",
        )
        .unwrap();
        for index in 0..20 {
            assert_eq!(
                snapshot
                    .entry(Path::new(&format!("file-{index:03}")))
                    .unwrap()
                    .unwrap()
                    .size,
                Some(1)
            );
        }
        assert!(snapshot.entry(Path::new("absent")).unwrap().is_none());
        assert_eq!(
            snapshot.metrics().tree_reads,
            1,
            "oversized metadata was decoded/scanned again"
        );
        assert_eq!(snapshot.metrics().blob_loads, 0);
    }
    #[test]
    fn metadata_eviction_keeps_prepared_directory_entries_available() {
        let (tmp, _) = fixture();
        let snapshot = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "metadata-eviction").unwrap();
        assert_eq!(
            snapshot
                .entry(Path::new("dir/child"))
                .unwrap()
                .unwrap()
                .size,
            Some(9)
        );
        let reads = snapshot.metrics().tree_reads;
        assert_eq!(
            snapshot.entry(Path::new("script")).unwrap().unwrap().size,
            Some(9)
        );
        assert_eq!(
            snapshot
                .entry(Path::new("dir/child"))
                .unwrap()
                .unwrap()
                .size,
            Some(9)
        );
        assert_eq!(
            snapshot.metrics().tree_reads,
            reads,
            "eviction reread a prepared Git tree"
        );
    }
    #[test]
    fn fitting_small_working_set_uses_byte_budget_without_early_disk_spill() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "resident-working-set").unwrap();
        for index in 0..160u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&index.to_le_bytes());
            let oid = repo.blob(&bytes).unwrap();
            assert_eq!(snap.read(oid, 0, 8192).unwrap(), bytes);
        }
        assert_eq!(
            snap.metrics().blob_cache_entries,
            160,
            "an unrelated entry cap evicted fitting blobs"
        );
        assert_eq!(
            snap.metrics().retained_raw_blob_bytes,
            0,
            "resident blobs were written eagerly"
        );
        assert!(snap.metrics().blob_cache_bytes <= snap.metrics().blob_cache_limit);
    }
    #[test]
    fn tiny_reopened_reads_of_spilled_blobs_do_not_refill_entire_objects() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "range-refill").unwrap();
        let oid = repo.blob(&vec![b'z'; 1024 * 1024]).unwrap();
        assert_eq!(snap.read(oid, 0, 1).unwrap(), b"z");
        for index in 0..20u8 {
            let other = repo.blob(&vec![index; 1024 * 1024]).unwrap();
            assert_eq!(snap.read(other, 0, 1).unwrap(), [index]);
        }
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        let before = snap
            .state()
            .unwrap()
            .raw_blobs
            .as_ref()
            .unwrap()
            .tested_authenticated_bytes();
        for offset in [100, 8192, 500_000] {
            assert_eq!(snap.read(oid, offset, 1).unwrap(), b"z");
        }
        let after = snap
            .state()
            .unwrap()
            .raw_blobs
            .as_ref()
            .unwrap()
            .tested_authenticated_bytes();
        assert_eq!(
            after - before,
            2 * crate::raw_blob::BLOCK as u64,
            "tiny reads authenticated whole objects again"
        );
        assert_eq!(
            snap.metrics().blob_loads,
            21,
            "a prepared source was decoded again"
        );
    }
    #[test]
    fn memory_hits_do_not_probe_the_disk_locator_and_evicted_readers_still_verify() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "cached-locator").unwrap();
        let bytes = b"retained memory bytes";
        let oid = repo.blob(bytes).unwrap();
        assert_eq!(snap.read(oid, 0, 1024).unwrap(), bytes);
        for _ in 0..64 {
            assert_eq!(snap.read(oid, 0, 1024).unwrap(), bytes);
        }
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().retained_raw_blob_bytes, 0);
        let reader = snap.open(oid).unwrap();
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        assert_eq!(reader.read(0, 1024).unwrap(), bytes);
        // The retained locator is trusted memory; underlying bytes still pass
        // through block authentication after memory-cache eviction.
        let raw = snap
            .state()
            .unwrap()
            .raw_blobs
            .as_ref()
            .unwrap()
            .get(oid)
            .unwrap()
            .unwrap();
        raw.corrupt_for_test().unwrap();
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        assert!(reader.read(0, 1024).is_err());
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
        for byte in 0..20u8 {
            let oid = repo.blob(&vec![byte; 1024 * 1024]).unwrap();
            readers.push(snap.open(oid).unwrap());
        }
        assert!(
            snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES,
            "active readers pinned payloads outside the RAM budget"
        );
        assert_eq!(readers[0].read(0, 1).unwrap(), [0]);
        assert_eq!(snap.metrics().blob_loads, 20);
    }
    #[test]
    fn blob_cache_is_bounded_and_large_partial_reads_bypass_retention() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "bounded").unwrap();
        let mut ids = Vec::new();
        for n in 0..300u32 {
            let mut data = vec![n as u8; crate::raw_blob::BLOCK];
            data[..4].copy_from_slice(&n.to_le_bytes());
            let oid = repo.blob(&data).unwrap();
            ids.push(oid);
            assert_eq!(snap.read(oid, 8192, 1).unwrap(), vec![n as u8]);
            assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        }
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
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
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        assert!(snap.metrics().blob_cache_bytes <= retained + crate::raw_blob::BLOCK + 512);
        assert!(snap.open(large).unwrap().0.data.on_disk());
    }
    #[test]
    fn touched_directories_stay_prepared_without_an_entry_cap() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "tree-reuse").unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut ids = Vec::new();
        for i in 0..300 {
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert(format!("file-{i}"), blob, 0o100644).unwrap();
            let id = tree.write().unwrap();
            snap.state().unwrap().tree(id).unwrap();
            ids.push(id);
        }
        for id in ids {
            snap.state().unwrap().tree(id).unwrap();
        }
        let stats = snap.metrics();
        assert_eq!(stats.tree_cache_entries, 300);
        assert_eq!(
            stats.tree_reads, 300,
            "already prepared trees were decoded again"
        );
        assert_eq!(stats.prepared_tree_entries, 300);
        assert_eq!(
            stats.retained_tree_bytes, 0,
            "raw trees were retained on disk"
        );
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
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
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
            assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
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
