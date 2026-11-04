//! Demand-loaded metadata and payloads owned by the local FUSE event loop.
//! Only the blocking Git decoder lives on the source thread. Cache hits neither
//! submit a source job nor take a cross-thread cache/state lock.
use crate::{
    raw_blob_async::{Bytes, RawBlobs},
    snapshot::SnapshotMetrics,
    source_async,
    tree::Directory,
};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid, Repository};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::File,
    rc::{Rc, Weak},
    sync::Arc,
};

type MetadataGates = HashMap<(Oid, bool), Weak<tokio::sync::Mutex<()>>>;
enum Source {
    Local(Rc<source_async::Source>),
    Remote(crate::remote_async::Source),
}
pub(crate) struct Finished {
    pub(crate) metrics: SnapshotMetrics,
    pub(crate) remote: Option<crate::remote::Remote>,
    pub(crate) error: Option<std::io::Error>,
}
pub(crate) struct Snapshot {
    root: Oid,
    source: Source,
    raw: RawBlobs,
    trees: RefCell<HashMap<Oid, Arc<Directory>>>,
    headers: RefCell<HashMap<Oid, u64>>,
    pending: RefCell<MetadataGates>,
}
struct Demand<'a> {
    owner: &'a Snapshot,
    key: (Oid, bool),
    gate: Rc<tokio::sync::Mutex<()>>,
}
impl Drop for Demand<'_> {
    fn drop(&mut self) {
        if Rc::strong_count(&self.gate) == 1 {
            let mut pending = self.owner.pending.borrow_mut();
            if pending
                .get(&self.key)
                .is_some_and(|w| w.as_ptr() == Rc::as_ptr(&self.gate))
            {
                pending.remove(&self.key);
            }
        }
    }
}
impl Snapshot {
    pub(crate) fn supports_directory_headers(&self) -> bool {
        matches!(self.source, Source::Local(_))
    }
    pub(crate) fn header_ready(&self, oid: Oid) -> bool {
        self.headers.borrow().contains_key(&oid)
    }
    pub(crate) fn contents_prepared(&self, oid: Oid) -> bool {
        self.raw.contains(oid)
    }
    /// A READDIRPLUS page needs headers, never speculative leaf contents.
    /// Failed entries remain listable without attributes; later ordinary
    /// lookups retain their existing per-object error behavior.
    pub(crate) async fn directory_headers(&self, oids: &[Oid]) -> Result<HashSet<Oid>> {
        let Source::Local(source) = &self.source else {
            anyhow::bail!("source does not support header-only directory pages");
        };
        let mut missing = oids
            .iter()
            .copied()
            .filter(|oid| !self.headers.borrow().contains_key(oid))
            .collect::<Vec<_>>();
        missing.sort_unstable();
        missing.dedup();
        // The sorted gates coalesce overlapping pages and ordinary size calls
        // without a cycle between two pages. No cache borrow crosses an await.
        let demands = missing
            .iter()
            .map(|oid| self.demand(*oid, false))
            .collect::<Vec<_>>();
        let mut guards = Vec::with_capacity(demands.len());
        for demand in &demands {
            guards.push(demand.gate.lock().await);
        }
        missing.retain(|oid| !self.headers.borrow().contains_key(oid));
        if missing.is_empty() {
            return Ok(HashSet::new());
        }
        let mut failed = HashSet::new();
        for (oid, result) in source.headers(missing).await? {
            match result {
                Ok((size, ObjectType::Blob)) => {
                    self.headers.borrow_mut().insert(oid, u64::try_from(size)?);
                }
                _ => {
                    failed.insert(oid);
                }
            }
        }
        Ok(failed)
    }
    #[cfg(test)]
    pub(crate) async fn tested_pause(&self) -> std::sync::mpsc::Sender<()> {
        match &self.source {
            Source::Local(source) => source.tested_pause().await,
            Source::Remote(source) => source.tested_pause().await,
        }
    }
    #[cfg(test)]
    pub(crate) fn tested_demands(&self) -> usize {
        match &self.source {
            Source::Local(source) => source.tested_demands(),
            Source::Remote(source) => source.tested_demands(),
        }
    }
    pub(crate) fn new(
        repo: Repository,
        root: Oid,
        directory: File,
        owner: Arc<tempfile::TempDir>,
        io: Rc<crate::file_async::Io>,
    ) -> Result<Self> {
        let source = Rc::new(source_async::Source::new(repo)?);
        let raw = RawBlobs::from_anchor(directory, owner, crate::raw_blob::MEMORY_BUDGET, io);
        Ok(Self {
            root,
            source: Source::Local(source),
            raw,
            trees: RefCell::new(HashMap::new()),
            headers: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashMap::new()),
        })
    }
    pub(crate) fn new_remote(
        repo: Repository,
        root: Oid,
        remote: crate::remote::Remote,
        directory: File,
        owner: Arc<tempfile::TempDir>,
        io: Rc<crate::file_async::Io>,
    ) -> Result<Self> {
        let raw = RawBlobs::from_anchor(directory, owner, crate::raw_blob::MEMORY_BUDGET, io);
        let (source, initial) = crate::remote_async::Source::new(repo, remote)?;
        for blob in initial {
            raw.adopt_initial(blob)?;
        }
        Ok(Self {
            root,
            source: Source::Remote(source),
            raw,
            trees: RefCell::new(HashMap::new()),
            headers: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashMap::new()),
        })
    }
    fn demand(&self, oid: Oid, tree: bool) -> Demand<'_> {
        let key = (oid, tree);
        let mut pending = self.pending.borrow_mut();
        let gate = pending
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let gate = Rc::new(tokio::sync::Mutex::new(()));
                pending.insert(key, Rc::downgrade(&gate));
                gate
            });
        Demand {
            owner: self,
            key,
            gate,
        }
    }
    pub(crate) async fn tree(&self, oid: Oid) -> Result<Arc<Directory>> {
        if let Some(tree) = self.trees.borrow().get(&oid) {
            return Ok(tree.clone());
        }
        let demand = self.demand(oid, true);
        let _guard = demand.gate.lock().await;
        if let Some(tree) = self.trees.borrow().get(&oid) {
            return Ok(tree.clone());
        }
        let tree = match &self.source {
            Source::Local(source) => Arc::new(source.tree(oid).await?),
            Source::Remote(source) => {
                let (tree, batch) = source.tree(oid).await?;
                self.adopt_remote(batch).await?;
                tree
            }
        };
        self.trees.borrow_mut().insert(oid, tree.clone());
        Ok(tree)
    }
    pub(crate) async fn size(&self, oid: Oid) -> Result<u64> {
        if let Some(size) = self.headers.borrow().get(&oid) {
            return Ok(*size);
        }
        let demand = self.demand(oid, false);
        let _guard = demand.gate.lock().await;
        if let Some(size) = self.headers.borrow().get(&oid) {
            return Ok(*size);
        }
        let (size, kind) = match &self.source {
            Source::Local(source) => source.header(oid).await?,
            Source::Remote(source) => {
                if !self.raw.contains(oid) {
                    self.adopt_remote(source.blob(oid).await?).await?;
                }
                (usize::try_from(self.raw.size(oid)?)?, ObjectType::Blob)
            }
        };
        anyhow::ensure!(
            kind == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        let size = u64::try_from(size)?;
        self.headers.borrow_mut().insert(oid, size);
        Ok(size)
    }
    /// Resolve only verified ancestor trees. Existence/policy checks must not
    /// fetch a leaf header or payload merely to decide whether it is tracked.
    pub(crate) async fn metadata(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<crate::snapshot::Entry>> {
        use crate::snapshot::{Entry, EntryKind};
        let mut entry = Entry {
            kind: EntryKind::Directory,
            mode: 0o040000,
            oid: self.root,
            size: Some(0),
        };
        for component in path.components() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!("snapshot path must be relative and contain no parent components");
            };
            anyhow::ensure!(
                entry.kind == EntryKind::Directory,
                "snapshot ancestor is not a directory: {}",
                path.display()
            );
            let tree = self.tree(entry.oid).await?;
            let Some((mode, oid)) = tree.lookup(name) else {
                return Ok(None);
            };
            entry = crate::snapshot::Snapshot::metadata(mode, oid, path)?;
        }
        Ok(Some(entry))
    }
    pub(crate) async fn entry(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<crate::snapshot::Entry>> {
        let Some(mut entry) = self.metadata(path).await? else {
            return Ok(None);
        };
        if entry.kind != crate::snapshot::EntryKind::Directory {
            entry.size = Some(self.size(entry.oid).await?);
        }
        Ok(Some(entry))
    }
    pub(crate) async fn directory(&self, path: &std::path::Path) -> Result<Option<Arc<Directory>>> {
        let Some(entry) = self.metadata(path).await? else {
            return Ok(None);
        };
        anyhow::ensure!(
            entry.kind == crate::snapshot::EntryKind::Directory,
            "snapshot path is not a directory: {}",
            path.display()
        );
        self.tree(entry.oid).await.map(Some)
    }
    /// Full-object consumers (ignore parsing) explicitly need these contents.
    /// Large payload cache retention stays bounded while assembling their input.
    pub(crate) async fn with_blob<T>(
        &self,
        oid: Oid,
        size: u64,
        consume: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        if size <= crate::raw_blob::BLOCK as u64 {
            let bytes = self.read(oid, size, 0, size as u32).await?;
            return consume(bytes.as_slice());
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(usize::try_from(size)?)?;
        let mut offset = 0;
        while offset < size {
            let block = self
                .read(
                    oid,
                    size,
                    offset,
                    (size - offset).min(crate::raw_blob::BLOCK as u64) as u32,
                )
                .await?;
            anyhow::ensure!(!block.as_slice().is_empty(), "truncated full object read");
            offset += block.as_slice().len() as u64;
            bytes.extend_from_slice(block.as_slice());
        }
        consume(&bytes)
    }
    pub(crate) async fn read(&self, oid: Oid, size: u64, offset: u64, count: u32) -> Result<Bytes> {
        if !self.raw.contains(oid) {
            match &self.source {
                Source::Local(source) => self.raw.prepare_with_size(source, oid, size).await?,
                Source::Remote(source) => {
                    let batch = source.blob(oid).await?;
                    anyhow::ensure!(batch.size == size, "prepared remote size changed");
                    self.adopt_remote(batch).await?;
                }
            }
        }
        self.raw.validate_content_once(oid).await?;
        self.raw.read(oid, offset, count).await
    }
    async fn adopt_remote(&self, mut batch: crate::remote_async::Batch) -> Result<()> {
        while let Some(blob) = batch.next() {
            self.raw.adopt(blob).await?;
        }
        Ok(())
    }
    pub(crate) async fn read_link(&self, oid: Oid, size: u64) -> Result<Bytes> {
        let bytes = self
            .read(
                oid,
                size,
                0,
                u32::try_from(size).context("symlink target too long")?,
            )
            .await?;
        anyhow::ensure!(
            !bytes.as_slice().is_empty() && !bytes.as_slice().contains(&0),
            "unusable symlink target"
        );
        Ok(bytes)
    }
    pub(crate) async fn finish(self) -> Result<Finished> {
        let mut metrics = self.raw.metrics();
        let imported = self.raw.imported_disk_metrics();
        let failure = self.raw.failure();
        let trees = self.trees.into_inner();
        metrics.tree_reads = trees.len() as u64;
        metrics.tree_cache_entries = trees.len();
        metrics.tree_cache_bytes = trees.values().map(|t| t.weight()).sum();
        metrics.prepared_tree_entries = trees.values().map(|t| t.len() as u64).sum();
        metrics.header_reads = self.headers.borrow().len() as u64;
        metrics.header_cache_entries = self.headers.into_inner().len();
        metrics.spool_bytes = metrics.retained_raw_blob_bytes;
        metrics.spool_writes = metrics.retained_raw_blob_entries;
        let source = match self.source {
            Source::Local(source) => match Rc::try_unwrap(source) {
                Ok(source) => source.finish().await.map(|_| None),
                Err(_) => Err(anyhow::anyhow!("local source still owned at shutdown")),
            },
            Source::Remote(source) => source.finish().await.map(Some),
        };
        let raw = self.raw.finish().await;
        let mut remote = source?;
        if let Some(remote) = &mut remote {
            if let Some(failure) = failure {
                remote.record_async_failure(&anyhow::anyhow!(failure));
            }
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
            let disk = remote.raw_blobs.disk_metrics();
            metrics.retained_raw_blob_bytes += disk.0 - imported.0;
            metrics.retained_raw_blob_entries += disk.1 - imported.1;
            metrics.spool_bytes = metrics.retained_raw_blob_bytes;
            metrics.spool_writes = metrics.retained_raw_blob_entries;
            metrics.verification_metadata_bytes += remote.raw_blobs.verification_bytes();
        }
        Ok(Finished {
            metrics,
            remote,
            error: raw.err().map(std::io::Error::other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_headers_coalesce_aliases_and_errors_without_loading_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(temp.path().join("repo")).unwrap();
        let first = repo.blob(b"first").unwrap();
        let second = repo.blob(b"second").unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let missing = Oid::from_bytes(&[9; 20]).unwrap();
        let owner = Arc::new(tempfile::tempdir_in(temp.path()).unwrap());
        let fd = File::open(owner.path()).unwrap();
        let io = crate::file_async::Io::new(false);
        let objects = Snapshot::new(repo, tree, fd, owner, io.clone()).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let failed = objects
                    .directory_headers(&[first, first, missing, tree])
                    .await
                    .unwrap();
                assert_eq!(failed, HashSet::from([missing, tree]));
                assert_eq!(objects.size(first).await.unwrap(), 5);
                assert_eq!(objects.tested_demands(), 1);
                let requested = [second, first, second];
                let (batch, single) =
                    tokio::join!(objects.directory_headers(&requested), objects.size(second));
                assert!(batch.unwrap().is_empty());
                assert_eq!(single.unwrap(), 6);
                assert_eq!(
                    objects.tested_demands(),
                    2,
                    "overlapping metadata demands crossed the source twice"
                );
                assert!(objects.pending.borrow().is_empty());
                let resume = objects.tested_pause().await;
                assert!(objects
                    .directory_headers(&[first, second])
                    .await
                    .unwrap()
                    .is_empty());
                assert_eq!(
                    objects.tested_demands(),
                    2,
                    "cached headers waited for the paused source"
                );
                assert_eq!(objects.raw.metrics().blob_loads, 0);
                assert_eq!(objects.raw.metrics().retained_raw_blob_entries, 0);
                assert!(!io.worker_started());
                resume.send(()).unwrap();
                objects.finish().await.unwrap();
            });
    }
}
