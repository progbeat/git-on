//! Demand-loaded metadata and payloads owned by the local FUSE event loop.
//! Only the blocking Git decoder lives on the source thread. Cache hits neither
//! submit a source job nor take a cross-thread cache/state lock.
use crate::{
    raw_blob_async::{Bytes, RawBlobs},
    snapshot::SnapshotMetrics,
    source_async::Source,
    tree::Directory,
};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid, Repository};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    rc::{Rc, Weak},
    sync::Arc,
};

type MetadataGates = HashMap<(Oid, bool), Weak<tokio::sync::Mutex<()>>>;
pub(crate) struct Snapshot {
    source: Source,
    raw: RawBlobs,
    trees: RefCell<HashMap<Oid, Arc<Directory>>>,
    headers: RefCell<HashMap<Oid, u64>>,
    pending: RefCell<MetadataGates>,
    failure: RefCell<Option<String>>,
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
    #[cfg(test)]
    pub(crate) async fn tested_pause(&self) -> std::sync::mpsc::Sender<()> {
        self.source.tested_pause().await
    }
    #[cfg(test)]
    pub(crate) fn tested_demands(&self) -> usize {
        self.source.tested_demands()
    }
    pub(crate) fn new(
        repo: Repository,
        directory: File,
        owner: Arc<tempfile::TempDir>,
    ) -> Result<Self> {
        Ok(Self {
            source: Source::new(repo)?,
            raw: RawBlobs::from_anchor(directory, owner, crate::raw_blob::MEMORY_BUDGET),
            trees: RefCell::new(HashMap::new()),
            headers: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashMap::new()),
            failure: RefCell::new(None),
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
    fn remember<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            if error
                .downcast_ref::<crate::object::TransferStopped>()
                .is_none()
            {
                self.failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("{error:#}"));
            }
        }
        result
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
        let tree = Arc::new(self.remember(self.source.tree(oid).await)?);
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
        let size = self.remember(
            async {
                let (size, kind) = self.source.header(oid).await?;
                anyhow::ensure!(
                    kind == ObjectType::Blob,
                    "snapshot file does not reference a blob"
                );
                Ok(u64::try_from(size)?)
            }
            .await,
        )?;
        self.headers.borrow_mut().insert(oid, size);
        Ok(size)
    }
    pub(crate) async fn read(&self, oid: Oid, size: u64, offset: u64, count: u32) -> Result<Bytes> {
        self.remember(
            async {
                self.raw.prepare_with_size(&self.source, oid, size).await?;
                self.raw.validate_content_once(oid).await?;
                self.raw.read(oid, offset, count).await
            }
            .await,
        )
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
        self.remember(
            (!bytes.as_slice().is_empty() && !bytes.as_slice().contains(&0))
                .then_some(())
                .context("unusable symlink target"),
        )?;
        Ok(bytes)
    }
    pub(crate) async fn finish(self) -> Result<(SnapshotMetrics, Option<String>)> {
        let mut metrics = self.raw.metrics();
        let trees = self.trees.into_inner();
        metrics.tree_reads = trees.len() as u64;
        metrics.tree_cache_entries = trees.len();
        metrics.tree_cache_bytes = trees.values().map(|t| t.weight()).sum();
        metrics.prepared_tree_entries = trees.values().map(|t| t.len() as u64).sum();
        metrics.header_reads = self.headers.borrow().len() as u64;
        metrics.header_cache_entries = self.headers.into_inner().len();
        metrics.spool_bytes = metrics.retained_raw_blob_bytes;
        metrics.spool_writes = metrics.retained_raw_blob_entries;
        let failure = self.failure.into_inner().or_else(|| self.raw.failure());
        self.source.finish().await?;
        Ok((metrics, failure))
    }
}
