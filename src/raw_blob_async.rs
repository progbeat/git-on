//! Event-loop-owned verified payload cache. RAM hits borrow trusted bytes;
//! only misses/replacement submit native asynchronous file operations.
use crate::file_async::{File, Io};
use crate::raw_blob::{BLOCK, ENTRY_CHARGE};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    cell::RefCell, collections::HashMap, fs::File as StdFile, ops::Range, os::fd::AsRawFd, rc::Rc,
};
#[derive(Clone)]
pub(crate) struct Bytes {
    data: Rc<Vec<u8>>,
    range: Range<usize>,
}
impl Bytes {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}
// The vector is immutable for the entire lifetime of every Rc, including when
// tokio-uring retains a cancelled write. Moving this wrapper cannot move or
// reallocate its allocation. No mutable IoBuf implementation is provided.
unsafe impl tokio_uring::buf::IoBuf for Bytes {
    fn stable_ptr(&self) -> *const u8 {
        self.as_slice().as_ptr()
    }
    fn bytes_init(&self) -> usize {
        self.range.len()
    }
    fn bytes_total(&self) -> usize {
        self.range.len()
    }
}
struct Descriptor {
    size: u64,
    hashes: Box<[[u8; 32]]>,
    file: bool,
    content_checked: bool,
    used: bool,
}
struct Resident {
    block: usize,
    bytes: Rc<Vec<u8>>,
}
struct State {
    objects: HashMap<Oid, Descriptor>,
    memory: lru::LruCache<Oid, Resident>,
    memory_bytes: usize,
    reader: Option<(Oid, Rc<File>)>,
    imported_disk_bytes: u64,
    imported_disk_entries: u64,
    failure: Option<String>,
    #[cfg(test)]
    io_operations: u64,
    #[cfg(test)]
    authenticated_bytes: u64,
}
struct Directory {
    fd: File,
    _owner: std::sync::Arc<tempfile::TempDir>,
}

struct Pending {
    directory: Rc<Directory>,
    name: std::ffi::CString,
    file: File,
    committed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                libc::unlinkat(self.directory.fd.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}
pub(crate) struct RawBlobs {
    state: RefCell<State>,
    directory: Rc<Directory>,
    limit: usize,
    io: Rc<Io>,
    // This gate coordinates cold admissions only. RAM reads do not take it and
    // no RefCell borrow or interthread cache lock survives a disk await.
    admission: tokio::sync::Mutex<()>,
    preparing: RefCell<HashMap<Oid, std::rc::Weak<tokio::sync::Mutex<()>>>>,
}
struct Preparing<'a> {
    owner: &'a RawBlobs,
    oid: Oid,
    gate: Rc<tokio::sync::Mutex<()>>,
}
impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        if Rc::strong_count(&self.gate) == 1 {
            let mut entries = self.owner.preparing.borrow_mut();
            if entries
                .get(&self.oid)
                .is_some_and(|weak| weak.as_ptr() == Rc::as_ptr(&self.gate))
            {
                entries.remove(&self.oid);
            }
        }
    }
}
impl RawBlobs {
    #[cfg(test)]
    pub(crate) fn new(parent: &std::path::Path) -> Result<Self> {
        Self::with_limit(parent, crate::raw_blob::MEMORY_BUDGET)
    }
    #[cfg(test)]
    fn with_limit(parent: &std::path::Path, limit: usize) -> Result<Self> {
        Self::with_limit_io(parent, limit, Io::new(true))
    }
    #[cfg(test)]
    fn with_limit_io(parent: &std::path::Path, limit: usize, io: Rc<Io>) -> Result<Self> {
        let owner = tempfile::Builder::new()
            .prefix("raw-objects-")
            .tempdir_in(parent)?;
        let fd = StdFile::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(owner.path())?;
        Ok(Self::from_anchor(fd, std::sync::Arc::new(owner), limit, io))
    }
    pub(crate) fn from_anchor(
        fd: StdFile,
        owner: std::sync::Arc<tempfile::TempDir>,
        limit: usize,
        io: Rc<Io>,
    ) -> Self {
        Self {
            state: RefCell::new(State {
                objects: HashMap::new(),
                memory: lru::LruCache::unbounded(),
                memory_bytes: 0,
                reader: None,
                imported_disk_bytes: 0,
                imported_disk_entries: 0,
                failure: None,
                #[cfg(test)]
                io_operations: 0,
                #[cfg(test)]
                authenticated_bytes: 0,
            }),
            directory: Rc::new(Directory {
                fd: io.owned(fd),
                _owner: owner,
            }),
            limit,
            io,
            admission: tokio::sync::Mutex::new(()),
            preparing: RefCell::new(HashMap::new()),
        }
    }
    pub(crate) async fn finish(self) -> Result<()> {
        if let Some((_, file)) = self.state.into_inner().reader {
            Rc::try_unwrap(file)
                .map_err(|_| anyhow::anyhow!("raw reader still owned"))?
                .close()
                .await?;
        }
        let directory = Rc::try_unwrap(self.directory)
            .map_err(|_| anyhow::anyhow!("raw directory still owned"))?;
        directory.fd.close_directory_anchor().await?;
        Ok(())
    }
    fn record_io(&self) {
        #[cfg(test)]
        {
            self.state.borrow_mut().io_operations += 1;
        }
    }
    async fn validate(&self, file: &File, size: Option<u64>) -> Result<()> {
        self.record_io();
        let m = file.statx().await?;
        anyhow::ensure!(
            u32::from(m.stx_mode) & libc::S_IFMT == libc::S_IFREG
                && m.stx_nlink == 1
                && m.stx_uid == unsafe { libc::geteuid() },
            "invalid private raw object"
        );
        if let Some(size) = size {
            anyhow::ensure!(m.stx_size == size, "verified raw blob length was modified");
        }
        Ok(())
    }
    async fn pending(&self, name: String) -> Result<Pending> {
        self.record_io();
        let file = self.io.open_raw(&self.directory.fd, &name, true).await?;
        let pending = Pending {
            directory: self.directory.clone(),
            name: std::ffi::CString::new(name)?,
            file,
            committed: false,
        };
        self.validate(&pending.file, None).await?;
        Ok(pending)
    }
    async fn make_room(&self, weight: usize) -> Result<()> {
        anyhow::ensure!(
            weight <= self.limit,
            "payload exceeds cache admission limit"
        );
        loop {
            let victim = {
                let s = self.state.borrow();
                if s.memory_bytes <= self.limit - weight {
                    return Ok(());
                }
                let (oid, r) = s
                    .memory
                    .peek_lru()
                    .context("resident accounting mismatch")?;
                (
                    *oid,
                    Bytes {
                        data: r.bytes.clone(),
                        range: 0..r.bytes.len(),
                    },
                    s.objects[oid].file,
                )
            };
            if !victim.2 {
                let mut pending = self.pending(victim.0.to_string()).await?;
                self.record_io();
                pending.file.write_all_at(victim.1.clone(), 0).await?;
                pending.committed = true;
            }
            // The victim remains available in RAM until the complete write succeeds.
            // A failed or cancelled eviction therefore cannot lose verified contents.
            let mut s = self.state.borrow_mut();
            s.objects.get_mut(&victim.0).unwrap().file = true;
            let old = s
                .memory
                .pop(&victim.0)
                .context("resident disappeared during admission")?;
            s.memory_bytes -= old.bytes.capacity() + ENTRY_CHARGE;
        }
    }
    fn admit(&self, oid: Oid, block: usize, bytes: Vec<u8>) -> Bytes {
        let data = Rc::new(bytes);
        let range = 0..data.len();
        let mut s = self.state.borrow_mut();
        if let Some(old) = s.memory.put(
            oid,
            Resident {
                block,
                bytes: data.clone(),
            },
        ) {
            s.memory_bytes -= old.bytes.capacity() + ENTRY_CHARGE;
        }
        s.memory_bytes += data.capacity() + ENTRY_CHARGE;
        debug_assert!(s.memory_bytes <= self.limit);
        Bytes { data, range }
    }
    /// Adopt the source's canonical verification and allocation. The source
    /// and loop must share the same anchored raw directory.
    pub(crate) async fn adopt(&self, handoff: crate::raw_blob::BlobHandoff) -> Result<()> {
        let result = self.adopt_inner(handoff).await;
        self.record_result(result)
    }
    async fn adopt_inner(&self, handoff: crate::raw_blob::BlobHandoff) -> Result<()> {
        let oid = handoff.blob().oid();
        let _admission = self.admission.lock().await;
        anyhow::ensure!(
            !self.state.borrow().objects.contains_key(&oid),
            "duplicate prepared object"
        );
        self.make_room(handoff.blob().resident_weight()).await?;
        // Keep the return guard intact across waits. Move the allocation and
        // descriptor only once admission can commit without yielding.
        self.adopt_initial(handoff.commit())?;
        Ok(())
    }
    pub(crate) fn contains(&self, oid: Oid) -> bool {
        self.state.borrow().objects.contains_key(&oid)
    }
    pub(crate) fn size(&self, oid: Oid) -> Result<u64> {
        Ok(self
            .state
            .borrow()
            .objects
            .get(&oid)
            .context("missing prepared object")?
            .size)
    }
    pub(crate) fn failure(&self) -> Option<String> {
        self.state.borrow().failure.clone()
    }
    /// Startup migration starts with an empty loop cache and the same budget.
    pub(crate) fn adopt_initial(&self, blob: crate::raw_blob::PreparedBlob) -> Result<()> {
        anyhow::ensure!(
            self.state.borrow().memory_bytes + blob.resident_weight() <= self.limit,
            "prepared cache exceeds the shared payload budget"
        );
        let b = blob.into_parts();
        let mut s = self.state.borrow_mut();
        anyhow::ensure!(!s.objects.contains_key(&b.oid), "duplicate prepared object");
        s.imported_disk_bytes += if b.file { b.size } else { 0 };
        s.imported_disk_entries += u64::from(b.file);
        s.objects.insert(
            b.oid,
            Descriptor {
                size: b.size,
                hashes: b.hashes,
                file: b.file,
                content_checked: b.content_checked,
                used: b.used,
            },
        );
        drop(s);
        if let Some((block, bytes)) = b.resident {
            self.admit(b.oid, block, bytes);
        }
        Ok(())
    }
    pub(crate) fn imported_disk_metrics(&self) -> (u64, u64) {
        let s = self.state.borrow();
        (s.imported_disk_bytes, s.imported_disk_entries)
    }
    /// Publish an owned small source object after canonical OID verification.
    /// Source decoders can eventually pass their typed proof instead of rehashing;
    /// this entry point never accepts unverified bytes as resident data.
    #[cfg(test)]
    pub(crate) async fn insert(&self, oid: Oid, bytes: Vec<u8>) -> Result<()> {
        self.insert_inner(oid, bytes, None).await
    }
    async fn insert_inner(
        &self,
        oid: Oid,
        bytes: Vec<u8>,
        proof: Option<crate::source_async::Verified>,
    ) -> Result<()> {
        if self.state.borrow().objects.contains_key(&oid) {
            return Ok(());
        }
        anyhow::ensure!(
            bytes.len() <= BLOCK,
            "use streaming import for large objects"
        );
        if let Some(proof) = proof {
            anyhow::ensure!(
                proof.oid() == oid && proof.size() == bytes.len(),
                "source proof does not identify these bytes"
            );
        } else {
            crate::object::copy_verified(
                bytes.as_slice(),
                std::io::sink(),
                ObjectType::Blob,
                bytes.len(),
                oid,
            )?;
        }
        let hashes = if bytes.is_empty() {
            Vec::new()
        } else {
            vec![Sha256::digest(&bytes).into()]
        };
        let weight = bytes.capacity() + ENTRY_CHARGE;
        let _admission = self.admission.lock().await;
        if self.state.borrow().objects.contains_key(&oid) {
            return Ok(());
        }
        if bytes.is_empty() {
            self.state.borrow_mut().objects.insert(
                oid,
                Descriptor {
                    size: 0,
                    hashes: hashes.into_boxed_slice(),
                    file: false,
                    content_checked: false,
                    used: true,
                },
            );
            return Ok(());
        }
        self.make_room(weight).await?;
        self.state.borrow_mut().objects.insert(
            oid,
            Descriptor {
                size: bytes.len() as u64,
                hashes: hashes.into_boxed_slice(),
                file: false,
                content_checked: false,
                used: true,
            },
        );
        self.admit(oid, 0, bytes);
        Ok(())
    }
    /// Reuse prepared objects without touching the decoder. Concurrent cold
    /// requests for an OID share a single transfer, outside all state borrows.
    #[cfg(test)]
    pub(crate) async fn prepare(
        &self,
        source: &crate::source_async::Source,
        oid: Oid,
    ) -> Result<()> {
        self.prepare_inner(source, oid, None).await
    }
    pub(crate) async fn prepare_with_size(
        &self,
        source: &crate::source_async::Source,
        oid: Oid,
        size: u64,
    ) -> Result<()> {
        self.prepare_inner(source, oid, Some(usize::try_from(size)?))
            .await
    }
    async fn prepare_inner(
        &self,
        source: &crate::source_async::Source,
        oid: Oid,
        size: Option<usize>,
    ) -> Result<()> {
        if self.state.borrow().objects.contains_key(&oid) {
            return Ok(());
        }
        let gate = {
            let mut pending = self.preparing.borrow_mut();
            if let Some(gate) = pending.get(&oid).and_then(std::rc::Weak::upgrade) {
                gate
            } else {
                let gate = Rc::new(tokio::sync::Mutex::new(()));
                pending.insert(oid, Rc::downgrade(&gate));
                gate
            }
        };
        let preparing = Preparing {
            owner: self,
            oid,
            gate,
        };
        let _guard = preparing.gate.lock().await;
        if self.state.borrow().objects.contains_key(&oid) {
            return Ok(());
        }
        if let Some(size) = size.filter(|size| *size <= BLOCK) {
            let blob = source.small_blob(oid, size).await?;
            return self.insert_inner(oid, blob.bytes, Some(blob.proof)).await;
        }
        let mut stream = if let Some(size) = size {
            source.blob_with_size(oid, size).await?
        } else {
            source.blob(oid).await?
        };
        if stream.size > BLOCK {
            let mut pending = self.pending(oid.to_string()).await?;
            let mut written = 0usize;
            let mut hashes = Vec::with_capacity(stream.size.div_ceil(BLOCK));
            while let Some(event) = stream.next().await {
                match event {
                    crate::source_async::BlobEvent::Chunk(bytes) => {
                        let count = bytes.len();
                        anyhow::ensure!(
                            count == (stream.size - written).min(BLOCK),
                            "source block boundary changed"
                        );
                        hashes.push(Sha256::digest(&bytes).into());
                        self.record_io();
                        pending.file.write_all_at(bytes, written as u64).await?;
                        written = written
                            .checked_add(count)
                            .context("source object length overflow")?;
                    }
                    crate::source_async::BlobEvent::Verified(proof) => {
                        anyhow::ensure!(
                            proof.oid() == oid
                                && proof.size() == stream.size
                                && written == stream.size,
                            "source proof does not identify the streamed object"
                        );
                        pending.committed = true;
                        self.state.borrow_mut().objects.insert(
                            oid,
                            Descriptor {
                                size: written as u64,
                                hashes: hashes.into_boxed_slice(),
                                file: true,
                                content_checked: false,
                                used: true,
                            },
                        );
                        return Ok(());
                    }
                    crate::source_async::BlobEvent::Failed(error) => return Err(error),
                }
            }
            anyhow::bail!("source stream ended before complete object verification");
        }
        let mut bytes = None::<Vec<u8>>;
        while let Some(event) = stream.next().await {
            match event {
                crate::source_async::BlobEvent::Chunk(chunk) => {
                    if let Some(bytes) = &mut bytes {
                        bytes.extend_from_slice(&chunk);
                    } else {
                        bytes = Some(chunk);
                    }
                    anyhow::ensure!(
                        bytes.as_ref().unwrap().len() <= stream.size,
                        "oversized source stream"
                    );
                }
                crate::source_async::BlobEvent::Verified(proof) => {
                    return self
                        .insert_inner(oid, bytes.unwrap_or_default(), Some(proof))
                        .await;
                }
                crate::source_async::BlobEvent::Failed(error) => return Err(error),
            }
        }
        anyhow::bail!("source stream ended before complete object verification")
    }
    pub(crate) async fn validate_content_once(&self, oid: Oid) -> Result<()> {
        let size = {
            let s = self.state.borrow();
            let object = s.objects.get(&oid).context("missing verified object")?;
            if object.content_checked || object.size >= 1024 {
                return Ok(());
            }
            object.size
        };
        let bytes = self.read(oid, 0, size as u32).await?;
        crate::snapshot::validate_source_blob(bytes.as_slice())?;
        self.state
            .borrow_mut()
            .objects
            .get_mut(&oid)
            .unwrap()
            .content_checked = true;
        Ok(())
    }
    pub(crate) fn metrics(&self) -> crate::snapshot::SnapshotMetrics {
        let s = self.state.borrow();
        crate::snapshot::SnapshotMetrics {
            blob_loads: s.objects.values().filter(|v| v.used).count() as u64,
            blob_bytes: s.objects.values().filter(|v| v.used).map(|v| v.size).sum(),
            blob_cache_bytes: s.memory_bytes,
            blob_cache_entries: s.memory.len(),
            blob_cache_limit: self.limit,
            retained_raw_blob_entries: s.objects.values().filter(|v| v.file).count() as u64,
            retained_raw_blob_bytes: s.objects.values().filter(|v| v.file).map(|v| v.size).sum(),
            verification_metadata_bytes: s
                .objects
                .values()
                .map(|v| v.hashes.len() * 32)
                .sum::<usize>()
                + 2 * s.objects.capacity() * (std::mem::size_of::<(Oid, Descriptor)>() + 1),
            ..Default::default()
        }
    }
    pub(crate) fn into_local_parts(bytes: Bytes) -> (Rc<Vec<u8>>, Range<usize>) {
        (bytes.data, bytes.range)
    }
    #[cfg(test)]
    pub(crate) fn tested_io_operations(&self) -> u64 {
        self.state.borrow().io_operations
    }
    async fn reader(&self, oid: Oid, size: u64) -> Result<Rc<File>> {
        let cached = {
            let s = self.state.borrow();
            s.reader
                .as_ref()
                .filter(|(key, _)| *key == oid)
                .map(|(_, f)| f.clone())
        };
        if let Some(file) = cached {
            self.validate(&file, Some(size)).await?;
            return Ok(file);
        }
        let old = self.state.borrow_mut().reader.take();
        if let Some((_, old)) = old {
            if let Ok(file) = Rc::try_unwrap(old) {
                file.close().await?;
            }
        }
        self.record_io();
        let file = Rc::new(
            self.io
                .open_raw(&self.directory.fd, &oid.to_string(), false)
                .await?,
        );
        self.validate(&file, Some(size)).await?;
        self.state.borrow_mut().reader = Some((oid, file.clone()));
        Ok(file)
    }
    pub(crate) async fn read(&self, oid: Oid, offset: u64, size: u32) -> Result<Bytes> {
        let result = self.read_inner(oid, offset, size).await;
        self.record_result(result)
    }
    fn record_result<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            if error
                .downcast_ref::<crate::object::TransferStopped>()
                .is_none()
            {
                self.state
                    .borrow_mut()
                    .failure
                    .get_or_insert_with(|| format!("{error:#}"));
            }
        }
        result
    }
    async fn read_inner(&self, oid: Oid, offset: u64, size: u32) -> Result<Bytes> {
        let length = {
            let mut s = self.state.borrow_mut();
            let object = s.objects.get_mut(&oid).context("missing verified object")?;
            if size != 0 && offset < object.size {
                object.used = true;
            }
            object.size.saturating_sub(offset).min(u64::from(size))
        };
        if length == 0 {
            return Ok(Bytes {
                data: Rc::new(Vec::new()),
                range: 0..0,
            });
        }
        let end = offset.checked_add(length).context("raw range end")?;
        let first = (offset / BLOCK as u64) as usize;
        let last = ((end - 1) / BLOCK as u64) as usize;
        let mut output = if first == last {
            Vec::new()
        } else {
            Vec::with_capacity(length as usize)
        };
        let mut range_file = None;
        for block in first..=last {
            crate::object::check_transfer()?;
            let start = block as u64 * BLOCK as u64;
            let from = offset.saturating_sub(start) as usize;
            let to = (end - start).min(BLOCK as u64) as usize;
            let resident = {
                let mut s = self.state.borrow_mut();
                s.memory
                    .get(&oid)
                    .filter(|r| r.block == block)
                    .map(|r| Bytes {
                        data: r.bytes.clone(),
                        range: from..to,
                    })
            };
            let bytes = if let Some(bytes) = resident {
                bytes
            } else {
                // Only cold cache replacement is serialized; hits above remain immediate.
                let _admission = self.admission.lock().await;
                let resident = {
                    let mut s = self.state.borrow_mut();
                    s.memory
                        .get(&oid)
                        .filter(|r| r.block == block)
                        .map(|r| Bytes {
                            data: r.bytes.clone(),
                            range: from..to,
                        })
                };
                if let Some(bytes) = resident {
                    bytes
                } else {
                    let (size, expected) = {
                        let s = self.state.borrow();
                        let d = &s.objects[&oid];
                        anyhow::ensure!(d.file, "missing prepared object block");
                        (d.size, d.hashes[block])
                    };
                    if range_file.is_none() {
                        range_file = Some(self.reader(oid, size).await?);
                    }
                    let file = range_file.as_ref().unwrap();
                    self.record_io();
                    let (r, buffer) = file
                        .read_exact_at(vec![0; ((size - start).min(BLOCK as u64)) as usize], start)
                        .await;
                    r?;
                    anyhow::ensure!(
                        Sha256::digest(&buffer).as_slice() == expected,
                        "verified raw blob bytes were modified"
                    );
                    #[cfg(test)]
                    {
                        self.state.borrow_mut().authenticated_bytes += buffer.len() as u64;
                    }
                    let old = self.state.borrow_mut().memory.pop(&oid);
                    if let Some(old) = old {
                        self.state.borrow_mut().memory_bytes -= old.bytes.capacity() + ENTRY_CHARGE;
                    }
                    self.make_room(buffer.capacity() + ENTRY_CHARGE).await?;
                    let mut bytes = self.admit(oid, block, buffer);
                    bytes.range = from..to;
                    bytes
                }
            };
            if first == last {
                return Ok(bytes);
            }
            output.extend_from_slice(bytes.as_slice());
        }
        let range = 0..output.len();
        Ok(Bytes {
            data: Rc::new(output),
            range,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn oid(b: &[u8]) -> Oid {
        Oid::hash_object(ObjectType::Blob, b).unwrap()
    }
    #[test]
    fn prepared_handoff_moves_resident_allocation_without_file_io() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let bytes = b"verified remote payload";
        let oid = oid(bytes);
        source
            .insert_fetched(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        let prepared = source.take_blob(oid).unwrap().unwrap();
        let pointer = prepared.resident_pointer().unwrap();
        assert_eq!(source.memory_metrics(), (0, 0));
        let (fd, owner) = source.directory_anchor().unwrap();
        let (returns, returned) = std::sync::mpsc::channel();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let store =
                    RawBlobs::from_anchor(fd, owner, crate::raw_blob::MEMORY_BUDGET, io.clone());
                store
                    .adopt(crate::raw_blob::BlobHandoff::new(prepared, returns))
                    .await
                    .unwrap();
                let actual = store.read(oid, 0, 100).await.unwrap();
                assert_eq!(actual.as_slice(), bytes);
                assert_eq!(
                    actual.as_slice().as_ptr(),
                    pointer,
                    "handoff copied the source allocation"
                );
                assert_eq!(store.tested_io_operations(), 0);
                assert!(!io.worker_started());
                assert!(returned.try_recv().is_err());
                drop(actual);
                store.finish().await.unwrap();
                io.finish().await.unwrap();
            });
        assert_eq!(source.disk_metrics(), (0, 0));
    }
    #[test]
    fn cancelled_handoff_returns_prepared_bytes_without_starting_file_worker() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let directory = tempfile::tempdir().unwrap();
        let mut source = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let bytes = b"cancelled remote payload";
        let oid = oid(bytes);
        source
            .insert_fetched(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        let prepared = source.take_blob(oid).unwrap().unwrap();
        let (fd, owner) = source.directory_anchor().unwrap();
        let (returns, returned) = std::sync::mpsc::channel();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let store =
                    RawBlobs::from_anchor(fd, owner, crate::raw_blob::MEMORY_BUDGET, io.clone());
                let admission = store.admission.lock().await;
                let mut pending =
                    Box::pin(store.adopt(crate::raw_blob::BlobHandoff::new(prepared, returns)));
                poll_fn(|cx| {
                    assert!(pending.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(pending);
                drop(admission);
                assert!(!io.worker_started());
                store.finish().await.unwrap();
                io.finish().await.unwrap();
            });
        source
            .restore_blob(
                returned
                    .try_recv()
                    .expect("cancelled handoff lost prepared bytes"),
            )
            .unwrap();
        assert_eq!(source.memory_metrics(), (0, 0));
        assert_eq!(source.get(oid).unwrap().unwrap().read_all().unwrap(), bytes);
    }
    #[test]
    fn prepared_disk_handoff_authenticates_first_returned_buffer() {
        use std::os::unix::fs::FileExt;
        let directory = tempfile::tempdir().unwrap();
        let mut source = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let bytes = vec![b'v'; BLOCK + 17];
        let oid = oid(&bytes);
        source
            .insert_fetched(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        let prepared = source.take_blob(oid).unwrap().unwrap();
        let (fd, owner) = source.directory_anchor().unwrap();
        StdFile::options()
            .write(true)
            .open(owner.path().join(oid.to_string()))
            .unwrap()
            .write_all_at(b"!", 0)
            .unwrap();
        let (returns, _) = std::sync::mpsc::channel();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let store =
                    RawBlobs::from_anchor(fd, owner, crate::raw_blob::MEMORY_BUDGET, io.clone());
                store
                    .adopt(crate::raw_blob::BlobHandoff::new(prepared, returns))
                    .await
                    .unwrap();
                assert!(
                    store.read(oid, 0, 1).await.is_err(),
                    "handoff trusted modified disk bytes"
                );
                store.finish().await.unwrap();
                io.finish().await.unwrap();
            });
    }
    #[test]
    fn fallback_ram_only_run_does_not_start_file_worker_even_during_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let store = RawBlobs::with_limit_io(directory.path(), 8192, io.clone()).unwrap();
                let bytes = b"trusted RAM bytes";
                store.insert(oid(bytes), bytes.to_vec()).await.unwrap();
                assert_eq!(
                    store.read(oid(bytes), 0, 100).await.unwrap().as_slice(),
                    bytes
                );
                assert!(!io.worker_started());
                store.finish().await.unwrap();
                assert!(
                    !io.worker_started(),
                    "directory cleanup started an unnecessary worker"
                );
                io.finish().await.unwrap();
            });
    }
    #[test]
    fn fallback_streams_large_objects_and_authenticates_reloaded_buffers() {
        use std::os::unix::fs::FileExt;
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(directory.path().join("repo")).unwrap();
        let bytes: Vec<_> = (0..2 * BLOCK + 17).map(|i| (i % 251) as u8).collect();
        let oid = repo.blob(&bytes).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let store =
                    RawBlobs::with_limit_io(directory.path(), BLOCK + ENTRY_CHARGE, io.clone())
                        .unwrap();
                let source = crate::source_async::Source::new(repo).unwrap();
                store.prepare(&source, oid).await.unwrap();
                assert_eq!(
                    store.read(oid, 0, BLOCK as u32).await.unwrap().as_slice(),
                    &bytes[..BLOCK]
                );
                let authenticated = store.state.borrow().authenticated_bytes;
                let path = store.directory._owner.path().join(oid.to_string());
                let attacker = StdFile::options().write(true).open(&path).unwrap();
                attacker.write_all_at(&[255], 0).unwrap();
                // Trusted resident bytes remain usable even after disk mutation.
                assert_eq!(
                    store.read(oid, 0, BLOCK as u32).await.unwrap().as_slice(),
                    &bytes[..BLOCK]
                );
                assert_eq!(store.state.borrow().authenticated_bytes, authenticated);
                // One retained block per OID: another block removes the first.
                assert_eq!(
                    store
                        .read(oid, BLOCK as u64, BLOCK as u32)
                        .await
                        .unwrap()
                        .as_slice(),
                    &bytes[BLOCK..2 * BLOCK]
                );
                assert_eq!(store.state.borrow().memory.len(), 1);
                assert!(store.read(oid, 0, 1).await.is_err());
                assert!(store.state.borrow().memory_bytes <= store.limit);
                source.finish().await.unwrap();
                store.finish().await.unwrap();
                io.finish().await.unwrap();
                assert!(!io.worker_started());
            });
    }
    #[test]
    fn fitting_working_set_stays_in_ram_and_shared_results_survive_eviction() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let store = RawBlobs::with_limit(dir.path(), 2 * (2048 + ENTRY_CHARGE)).unwrap();
            let a = vec![b'a'; 2048];
            let b = vec![b'b'; 2048];
            let c = vec![b'c'; 2048];
            store.insert(oid(&a), a.clone()).await.unwrap();
            store.insert(oid(&b), b.clone()).await.unwrap();
            let first = store.read(oid(&a), 17, 1024).await.unwrap();
            let repeat = store.read(oid(&a), 17, 1024).await.unwrap();
            assert!(Rc::ptr_eq(&first.data, &repeat.data));
            assert_eq!(first.as_slice(), &a[17..1041]);
            assert_eq!(store.state.borrow().io_operations, 0);
            assert_eq!(store.state.borrow().authenticated_bytes, 0);
            assert_eq!(
                std::fs::read_dir(store.directory._owner.path())
                    .unwrap()
                    .count(),
                0
            );
            store.insert(oid(&c), c).await.unwrap();
            assert_eq!(first.as_slice(), &a[17..1041]);
            assert_eq!(store.read(oid(&b), 0, 2048).await.unwrap().as_slice(), b);
            assert!(store.state.borrow().authenticated_bytes > 0);
            assert!(store.state.borrow().memory_bytes <= store.limit);
        });
    }
    #[test]
    fn failed_handoff_admission_returns_payload_and_preserves_storage_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let incoming = vec![b'i'; 6000];
        let incoming_oid = oid(&incoming);
        source
            .insert_fetched(
                incoming_oid,
                incoming.len(),
                incoming.as_slice(),
                directory.path(),
            )
            .unwrap();
        let prepared = source.take_blob(incoming_oid).unwrap().unwrap();
        let (fd, owner) = source.directory_anchor().unwrap();
        let anchor = owner.clone();
        let (returns, returned) = std::sync::mpsc::channel();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let io = Io::new(false);
                let store = RawBlobs::from_anchor(fd, owner, 8192, io.clone());
                let victim = vec![b'v'; 6000];
                let victim_oid = oid(&victim);
                store.insert(victim_oid, victim.clone()).await.unwrap();
                let collision = anchor.path().join(victim_oid.to_string());
                std::fs::write(&collision, b"untrusted collision").unwrap();
                assert!(store
                    .adopt(crate::raw_blob::BlobHandoff::new(prepared, returns))
                    .await
                    .is_err());
                assert!(
                    store.failure().is_some(),
                    "caught admission error was not preserved"
                );
                assert_eq!(
                    store.read(victim_oid, 0, 6000).await.unwrap().as_slice(),
                    victim
                );
                assert_eq!(std::fs::read(collision).unwrap(), b"untrusted collision");
                store.finish().await.unwrap();
                io.finish().await.unwrap();
            });
        source
            .restore_blob(
                returned
                    .try_recv()
                    .expect("failed handoff lost the incoming object"),
            )
            .unwrap();
        assert_eq!(
            source
                .get(incoming_oid)
                .unwrap()
                .unwrap()
                .read_all()
                .unwrap(),
            incoming
        );
    }
    #[test]
    fn async_reload_checks_same_buffer_and_resident_reads_ignore_modified_spill() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let store = RawBlobs::with_limit(dir.path(), 1024 + ENTRY_CHARGE).unwrap();
            let a = vec![b'a'; 1024];
            let b = vec![b'b'; 1024];
            store.insert(oid(&a), a.clone()).await.unwrap();
            store.insert(oid(&b), b.clone()).await.unwrap();
            let path = store.directory._owner.path().join(oid(&a).to_string());
            assert_eq!(store.read(oid(&a), 0, 1024).await.unwrap().as_slice(), a);
            std::fs::write(&path, vec![b'x'; 1024]).unwrap();
            let io = store.state.borrow().io_operations;
            assert_eq!(store.read(oid(&a), 0, 1024).await.unwrap().as_slice(), a);
            assert_eq!(store.state.borrow().io_operations, io);
            store.read(oid(&b), 0, 1024).await.unwrap();
            assert!(store.read(oid(&a), 0, 1024).await.is_err());
        });
    }
    #[test]
    fn streamed_source_import_stays_bounded_and_authenticates_reloaded_ranges() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let repo = git2::Repository::init_bare(dir.path()).unwrap();
            let bytes = (0..BLOCK * 3 + 13)
                .map(|i| (i % 251) as u8)
                .collect::<Vec<_>>();
            let key = repo.blob(&bytes).unwrap();
            let source = crate::source_async::Source::new(repo).unwrap();
            let store = RawBlobs::with_limit(dir.path(), BLOCK + ENTRY_CHARGE).unwrap();
            store.prepare(&source, key).await.unwrap();
            assert_eq!(store.state.borrow().memory_bytes, 0);
            let range = store
                .read(key, (BLOCK - 17) as u64, (BLOCK + 41) as u32)
                .await
                .unwrap();
            assert_eq!(range.as_slice(), &bytes[BLOCK - 17..BLOCK * 2 + 24]);
            assert_eq!(store.state.borrow().memory.len(), 1);
            assert!(store.state.borrow().memory_bytes <= store.limit);
            assert_eq!(source.tested_demands(), 1);
            store.prepare(&source, key).await.unwrap();
            assert_eq!(
                source.tested_demands(),
                1,
                "prepared data was decoded again"
            );
            source.finish().await.unwrap();
        });
    }
    #[test]
    fn asynchronous_opens_remain_anchored_after_directory_path_replacement() {
        tokio_uring::start(async {
            use std::os::unix::fs::symlink;
            let dir = tempfile::tempdir().unwrap();
            let store = RawBlobs::with_limit(dir.path(), 1024 + ENTRY_CHARGE).unwrap();
            let a = vec![b'a'; 1024];
            let b = vec![b'b'; 1024];
            store.insert(oid(&a), a.clone()).await.unwrap();
            store.insert(oid(&b), b).await.unwrap();
            let original = store.directory._owner.path().to_owned();
            let moved = dir.path().join("moved-owned-directory");
            std::fs::rename(&original, &moved).unwrap();
            let attacker = dir.path().join("replacement");
            std::fs::create_dir(&attacker).unwrap();
            std::fs::write(attacker.join(oid(&a).to_string()), vec![b'x'; 1024]).unwrap();
            symlink(&attacker, &original).unwrap();
            assert_eq!(store.read(oid(&a), 0, 1024).await.unwrap().as_slice(), a);
            assert_eq!(
                std::fs::read(attacker.join(oid(&a).to_string())).unwrap(),
                vec![b'x'; 1024]
            );
        });
    }
    #[test]
    fn asynchronous_eviction_collision_keeps_verified_ram_and_existing_file() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let store = RawBlobs::with_limit(dir.path(), 1024 + ENTRY_CHARGE).unwrap();
            let a = vec![b'a'; 1024];
            let b = vec![b'b'; 1024];
            store.insert(oid(&a), a.clone()).await.unwrap();
            let path = store.directory._owner.path().join(oid(&a).to_string());
            std::fs::write(&path, b"existing").unwrap();
            assert!(store.insert(oid(&b), b).await.is_err());
            assert_eq!(store.read(oid(&a), 0, 1024).await.unwrap().as_slice(), a);
            assert_eq!(std::fs::read(&path).unwrap(), b"existing");
            assert!(!store.state.borrow().objects[&oid(&a)].file);
        });
    }
}
