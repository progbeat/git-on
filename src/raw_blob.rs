//! Invocation-local verified contents. Only payload bytes spill to disk;
//! object identity, length and ordered block digests stay in protected RAM.
use crate::{
    spool::{Files, Spool},
    tree::Directory,
};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt},
    },
    path::Path,
    sync::{Arc, Mutex},
};

pub(crate) const MEMORY_BUDGET: usize = 16 * 1024 * 1024;
// Conservatively covers the LRU node, table slack/control bytes and Vec header.
const ENTRY_CHARGE: usize = 512;
pub(crate) const BLOCK: usize = 64 * 1024;
fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
struct Descriptor {
    size: u64,
    blocks: Box<[[u8; 32]]>,
    file: bool,
    used: bool,
    content_checked: bool,
    directory: Option<Arc<Directory>>,
}
impl Descriptor {
    fn kind(&self) -> ObjectType {
        if self.directory.is_some() {
            ObjectType::Tree
        } else {
            ObjectType::Blob
        }
    }
}
struct Resident {
    block: usize,
    bytes: Vec<u8>,
}
#[derive(Clone)]
pub(crate) struct RawBlob {
    state: Arc<Mutex<Store>>,
    oid: Oid,
    size: u64,
    kind: ObjectType,
}
impl RawBlob {
    pub(crate) fn kind(&self) -> ObjectType {
        self.kind
    }
    pub(crate) fn len(&self) -> u64 {
        self.size
    }
    pub(crate) fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.range(
            offset,
            self.size.saturating_sub(offset).min(u64::from(size)),
        )
    }
    pub(crate) fn read_all(&self) -> Result<Vec<u8>> {
        self.range(0, self.size)
    }
    pub(crate) fn check_content_once(&self, check: impl FnOnce(&[u8]) -> Result<()>) -> Result<()> {
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .get(&self.oid)
            .context("missing verified object descriptor")?
            .content_checked
        {
            return Ok(());
        }
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("small_blob_policy", self.size);
        let bytes = self.read_all()?;
        check(&bytes)?;
        // The descriptor identifies immutable Git contents; successful format
        // validation need not hydrate the object again on future opens. This
        // flag never replaces block authentication of bytes returned to users.
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .get_mut(&self.oid)
            .context("missing verified object descriptor")?
            .content_checked = true;
        Ok(())
    }
    pub(crate) fn on_disk(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .get(&self.oid)
            .is_some_and(|object| object.file)
    }
    fn range(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let result = self.checked_range(offset, length);
        if let Err(error) = &result {
            // Reads can outlive the Snapshot lock through an active reader.
            // Preserve storage failures for the remote runner's final status,
            // including after a caller catches EIO or all handles close.
            // A request's signal handler may retry EINTR normally.
            if !crate::git::is_request_interruption(error) {
                let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if store.failure.is_none() {
                    store.failure = Some(format!("{error:#}"));
                }
            }
        }
        result
    }
    fn checked_range(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("raw_range", length);
        if length == 0 {
            return Ok(Vec::new());
        }
        anyhow::ensure!(self.kind == ObjectType::Blob, "tree bytes are not retained");
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let end = offset.checked_add(length).context("raw range end")?;
        let mut result = Vec::with_capacity(usize::try_from(length)?);
        let mut file = None;
        for block in
            usize::try_from(offset / BLOCK as u64)?..=usize::try_from((end - 1) / BLOCK as u64)?
        {
            crate::object::check_transfer()?;
            let start = block as u64 * BLOCK as u64;
            let from = usize::try_from(offset.saturating_sub(start))?;
            let to = usize::try_from((end - start).min(BLOCK as u64))?;
            if store
                .memory
                .peek(&self.oid)
                .is_some_and(|resident| resident.block == block)
            {
                let resident = store.memory.get(&self.oid).unwrap();
                result.extend_from_slice(&resident.bytes[from..to]);
                continue;
            }
            let size = usize::try_from((self.size - start).min(BLOCK as u64))?;
            let object = store
                .objects
                .get(&self.oid)
                .context("missing verified object descriptor")?;
            anyhow::ensure!(object.file, "missing prepared object block");
            let expected = object.blocks[block];
            if file.is_none() {
                file = Some(store.directory.reader(self.oid, self.size)?);
            }
            let mut bytes = vec![0; size];
            {
                #[cfg(feature = "test-metrics")]
                let _profile = crate::profile::span("raw_disk_read", size as u64);
                file.as_ref().unwrap().read_exact_at(&mut bytes, start)?;
            }
            let digest = {
                #[cfg(feature = "test-metrics")]
                let _profile = crate::profile::span("raw_block_hash", size as u64);
                Sha256::digest(&bytes)
            };
            if digest.as_slice() != expected {
                return Err(invalid("verified raw blob bytes were modified"));
            }
            #[cfg(test)]
            {
                store.authenticated_bytes += size as u64;
            }
            // Copy only from this checked buffer, never reopen/reread after trust.
            result.extend_from_slice(&bytes[from..to]);
            store.admit(self.oid, block, bytes)?;
        }
        Ok(result)
    }
    #[cfg(test)]
    pub(crate) fn corrupt_for_test(&self) -> Result<()> {
        let store = self.state.lock().unwrap();
        store
            .directory
            .open(self.oid, true, self.size)?
            .write_all_at(b"!", 0)?;
        Ok(())
    }
}

/// A descriptor for the owned directory anchors operations even after a path
/// rename. One last-read descriptor is shared by the store; blobs and child
/// handles do not each retain a descriptor. Every disk read rechecks metadata.
struct RawDirectory {
    fd: File,
    reader: Option<(Oid, Arc<File>)>,
    _owner: tempfile::TempDir,
    #[cfg(test)]
    open_calls: std::cell::Cell<usize>,
}
impl RawDirectory {
    fn new(parent: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let owner = tempfile::Builder::new()
            .prefix("raw-objects-")
            .tempdir_in(parent)?;
        let fd = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(owner.path())?;
        Ok(Self {
            fd,
            reader: None,
            _owner: owner,
            #[cfg(test)]
            open_calls: std::cell::Cell::new(0),
        })
    }
    fn validate(file: &File, size: Option<u64>) -> Result<()> {
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() },
            "invalid private raw object"
        );
        if let Some(size) = size {
            anyhow::ensure!(
                metadata.len() == size,
                "verified raw blob length was modified"
            );
        }
        Ok(())
    }
    fn open_name(&self, name: &CString, flags: i32, size: Option<u64>) -> Result<File> {
        #[cfg(test)]
        self.open_calls.set(self.open_calls.get() + 1);
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("raw_file_open", 0);
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("open private raw object");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        Self::validate(&file, size)?;
        Ok(file)
    }
    fn open(&self, oid: Oid, write: bool, size: u64) -> Result<File> {
        self.open_name(
            &CString::new(oid.to_string())?,
            if write {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            },
            Some(size),
        )
    }
    fn reader(&mut self, oid: Oid, size: u64) -> Result<Arc<File>> {
        if let Some((cached, file)) = &self.reader {
            if *cached == oid {
                Self::validate(file, Some(size))?;
                return Ok(file.clone());
            }
        }
        // Retain one descriptor independent of object count, payload budgets
        // and open child handles. A different object replaces this one slot.
        // Release the old slot first: it may be the last available descriptor
        // under the caller's limit, so opening before replacement can fail.
        self.reader = None;
        let file = Arc::new(self.open(oid, false, size)?);
        self.reader = Some((oid, file.clone()));
        Ok(file)
    }
    fn pending(&self) -> Result<Pending<'_>> {
        let name = CString::new(format!("pending-{}", uuid::Uuid::new_v4()))?;
        let file = self.open_name(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, None)?;
        Ok(Pending {
            directory: self,
            name,
            file,
            committed: false,
        })
    }
}
struct Pending<'a> {
    directory: &'a RawDirectory,
    name: CString,
    file: File,
    committed: bool,
}
impl Pending<'_> {
    fn commit(mut self, oid: Oid) -> Result<()> {
        let target = CString::new(oid.to_string())?;
        if unsafe {
            libc::renameat(
                self.directory.fd.as_raw_fd(),
                self.name.as_ptr(),
                self.directory.fd.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error()).context("publish raw object file");
        }
        self.committed = true;
        Ok(())
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                libc::unlinkat(self.directory.fd.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

pub(crate) struct RawBlobs {
    state: Arc<Mutex<Store>>,
}
impl RawBlobs {
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        Ok(Self {
            state: Arc::new(Mutex::new(Store {
                directory: RawDirectory::new(directory)?,
                objects: HashMap::new(),
                memory: lru::LruCache::unbounded(),
                memory_bytes: 0,
                memory_limit: MEMORY_BUDGET,
                memory_high_water: 0,
                scratch: None,
                output: None,
                failure: None,
                blob_entries: 0,
                blob_bytes: 0,
                disk_entries: 0,
                disk_bytes: 0,
                #[cfg(test)]
                authenticated_bytes: 0,
            })),
        })
    }
    fn blob(&self, oid: Oid, object: &Descriptor) -> RawBlob {
        RawBlob {
            state: self.state.clone(),
            oid,
            size: object.size,
            kind: object.kind(),
        }
    }
    pub(crate) fn get(&self, oid: Oid) -> Result<Option<RawBlob>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(state.objects.get(&oid).map(|object| self.blob(oid, object)))
    }
    pub(crate) fn for_read(&mut self, oid: Oid) -> Result<Option<(RawBlob, bool)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(object) = state.objects.get_mut(&oid) else {
            return Ok(None);
        };
        anyhow::ensure!(
            object.kind() == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        let first = !object.used;
        object.used = true;
        Ok(Some((self.blob(oid, object), first)))
    }
    pub(crate) fn directory(&self, oid: Oid) -> Option<(Arc<Directory>, bool)> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let object = store.objects.get_mut(&oid)?;
        let directory = object.directory.clone()?;
        let first = !object.used;
        object.used = true;
        Some((directory, first))
    }
    pub(crate) fn insert(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        _directory: &Path,
    ) -> Result<RawBlob> {
        self.insert_blob(oid, size, true, |output| {
            crate::object::copy_verified(input, output, ObjectType::Blob, size, oid)
        })
    }
    pub(crate) fn insert_odb_blob(
        &mut self,
        oid: Oid,
        object: &git2::OdbObject<'_>,
    ) -> Result<RawBlob> {
        anyhow::ensure!(
            object.id() == oid && object.kind() == ObjectType::Blob,
            "ODB object identity/type changed before raw import"
        );
        self.insert_blob(oid, object.len(), true, |output| {
            crate::object::copy_odb_blob(object, output)
        })
    }
    pub(crate) fn insert_fetched(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        _directory: &Path,
    ) -> Result<RawBlob> {
        self.insert_blob(oid, size, false, |output| {
            crate::object::copy_verified(input, output, ObjectType::Blob, size, oid)
        })
    }
    fn insert_blob(
        &mut self,
        oid: Oid,
        size: usize,
        used: bool,
        transfer: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<RawBlob> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(object) = store.objects.get(&oid) {
            return Ok(self.blob(oid, object));
        }
        let count = size.div_ceil(BLOCK);
        let weight = count
            .checked_mul(ENTRY_CHARGE)
            .and_then(|n| n.checked_add(size));
        let resident = size <= BLOCK && weight.is_some_and(|weight| weight <= store.memory_limit);
        let (digests, buffers) = if resident {
            let mut output = Blocks::new(Vec::with_capacity(size));
            transfer(&mut output)?;
            let (chunks, digests) = output.finish();
            let buffers = if chunks.is_empty() {
                Vec::new()
            } else {
                vec![chunks]
            };
            (digests, buffers)
        } else {
            let mut pending = store.directory.pending()?;
            let mut output = Blocks::new(&mut pending.file);
            transfer(&mut output)?;
            let (_, digests) = output.finish();
            pending.commit(oid)?;
            (digests, Vec::new())
        };
        anyhow::ensure!(digests.len() == count, "verified block count changed");
        if resident {
            store.make_room(weight.unwrap())?;
        }
        store.objects.insert(
            oid,
            Descriptor {
                size: size as u64,
                blocks: digests.into_boxed_slice(),
                file: !resident,
                used,
                content_checked: false,
                directory: None,
            },
        );
        for (block, bytes) in buffers.into_iter().enumerate() {
            store.admit(oid, block, bytes)?;
        }
        store.blob_entries += 1;
        store.blob_bytes += size as u64;
        if !resident {
            store.disk_entries += 1;
            store.disk_bytes += size as u64;
        }
        Ok(self.blob(oid, store.objects.get(&oid).unwrap()))
    }
    pub(crate) fn insert_tree(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        _directory: &Path,
    ) -> Result<RawBlob> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(object) = store.objects.get(&oid) {
            return Ok(self.blob(oid, object));
        }
        let directory = Arc::new(Directory::verified(oid, size, input)?);
        store.objects.insert(
            oid,
            Descriptor {
                size: size as u64,
                blocks: Box::new([]),
                file: false,
                used: false,
                content_checked: false,
                directory: Some(directory),
            },
        );
        Ok(self.blob(oid, store.objects.get(&oid).unwrap()))
    }
    pub(crate) fn import_blob(
        &mut self,
        oid: Oid,
        _offset: u64,
        size: usize,
        input: impl Read,
        directory: &Path,
    ) -> Result<()> {
        self.insert_fetched(oid, size, input, directory)?;
        Ok(())
    }
    pub(crate) fn import_tree(
        &mut self,
        oid: Oid,
        _offset: u64,
        size: usize,
        input: impl Read,
        metadata: impl Write,
        directory: &Path,
    ) -> Result<()> {
        self.insert_tree(
            oid,
            size,
            TeeRead {
                input,
                output: metadata,
            },
            directory,
        )?;
        Ok(())
    }
    // Scratch files belong to transport framing/decoding, never the object map.
    pub(crate) fn scratch_file(&self, directory: &Path) -> Result<Spool> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scratch_file(directory)
    }
    pub(crate) fn append_output(&self) -> Result<(RawOutput, u64)> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let parent = store
            .directory
            ._owner
            .path()
            .parent()
            .unwrap()
            .to_path_buf();
        let file = Arc::new(store.scratch_file(&parent)?);
        store.output = Some(file.clone());
        Ok((RawOutput { file }, 0))
    }
    pub(crate) fn output_reader(&self, start: u64) -> Result<impl Read> {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let file = store
            .output
            .as_ref()
            .context("no staged native output")?
            .clone();
        let size = file
            .len()?
            .checked_sub(start)
            .context("invalid native output offset")?;
        Ok(StagedReader {
            file,
            start,
            size,
            position: 0,
        })
    }
    pub(crate) fn release_output(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).output = None;
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn oids(&self) -> Result<Vec<Oid>> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .keys()
            .copied()
            .collect())
    }
    pub(crate) fn failure(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .clone()
    }
    pub(crate) fn entries(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blob_entries
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blob_bytes
    }
    pub(crate) fn memory_metrics(&self) -> (usize, usize) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (store.memory_bytes, store.memory.len())
    }
    pub(crate) fn disk_metrics(&self) -> (u64, u64) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (store.disk_bytes, store.disk_entries)
    }
    pub(crate) fn tree_metrics(&self) -> (u64, usize, usize) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        store
            .objects
            .values()
            .filter_map(|object| object.directory.as_ref())
            .fold((0, 0, 0), |(entries, bytes, count), tree| {
                (
                    entries + tree.len() as u64,
                    bytes + tree.weight(),
                    count + 1,
                )
            })
    }
    pub(crate) fn verification_bytes(&self) -> usize {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        store
            .objects
            .values()
            .map(|object| object.blocks.len() * 32)
            .sum::<usize>()
            + 2 * store.objects.capacity() * (std::mem::size_of::<(Oid, Descriptor)>() + 1)
    }
    #[cfg(test)]
    pub(crate) fn tested_authenticated_bytes(&self) -> u64 {
        self.state.lock().unwrap().authenticated_bytes
    }
    #[cfg(test)]
    pub(crate) fn evict_all_for_test(&mut self) -> Result<()> {
        self.state.lock().unwrap().make_room(MEMORY_BUDGET)
    }
    #[cfg(test)]
    fn disk_only(directory: &Path) -> Result<Self> {
        let value = Self::new(directory)?;
        value.state.lock().unwrap().memory_limit = 0;
        Ok(value)
    }
}
struct Store {
    directory: RawDirectory,
    objects: HashMap<Oid, Descriptor>,
    memory: lru::LruCache<Oid, Resident>,
    memory_bytes: usize,
    memory_limit: usize,
    memory_high_water: usize,
    scratch: Option<Files>,
    output: Option<Arc<Spool>>,
    failure: Option<String>,
    blob_entries: u64,
    blob_bytes: u64,
    disk_entries: u64,
    disk_bytes: u64,
    #[cfg(test)]
    authenticated_bytes: u64,
}
impl Store {
    fn scratch_file(&mut self, directory: &Path) -> Result<Spool> {
        if self.scratch.is_none() {
            self.scratch = Some(Files::new(directory)?);
        }
        Ok(self.scratch.as_ref().unwrap().file(directory)?)
    }
    fn make_room(&mut self, weight: usize) -> Result<()> {
        while self.memory_bytes > self.memory_limit.saturating_sub(weight) {
            let (key, resident) = self
                .memory
                .peek_lru()
                .context("resident accounting mismatch")?;
            let oid = *key;
            let descriptor = self.objects.get(&oid).context("missing resident object")?;
            if !descriptor.file {
                anyhow::ensure!(
                    resident.block == 0 && resident.bytes.len() as u64 == descriptor.size,
                    "unbacked object is not fully resident"
                );
                #[cfg(feature = "test-metrics")]
                let _profile = crate::profile::span("raw_small_spill", descriptor.size);
                let pending = self.directory.pending()?;
                pending.file.write_all_at(&resident.bytes, 0)?;
                pending.commit(oid)?;
                self.disk_entries += 1;
                self.disk_bytes += descriptor.size;
                self.objects.get_mut(&oid).unwrap().file = true;
            }
            let (_, removed) = self.memory.pop_lru().unwrap();
            self.memory_bytes -= removed.bytes.capacity() + ENTRY_CHARGE;
        }
        if self.memory.len().saturating_mul(4) < self.memory_high_water {
            self.memory
                .resize(std::num::NonZeroUsize::new(self.memory.len().max(1)).unwrap());
            self.memory
                .resize(std::num::NonZeroUsize::new(usize::MAX).unwrap());
            self.memory_high_water = self.memory.len();
        }
        Ok(())
    }
    fn admit(&mut self, oid: Oid, block: usize, bytes: Vec<u8>) -> Result<()> {
        let weight = bytes
            .capacity()
            .checked_add(ENTRY_CHARGE)
            .context("resident allocation size")?;
        if weight > self.memory_limit {
            return Ok(());
        }
        // One retained entry per original object, shared by all its readers.
        // Owned buffers of in-flight reads are separate and are not invalidated.
        if let Some(previous) = self.memory.pop(&oid) {
            self.memory_bytes -= previous.bytes.capacity() + ENTRY_CHARGE;
        }
        self.make_room(weight)?;
        self.memory_bytes += weight;
        self.memory.put(oid, Resident { block, bytes });
        self.memory_high_water = self.memory_high_water.max(self.memory.len());
        Ok(())
    }
}
struct Blocks<W> {
    output: W,
    hash: Sha256,
    used: usize,
    digests: Vec<[u8; 32]>,
}
impl<W> Blocks<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            hash: Sha256::new(),
            used: 0,
            digests: Vec::new(),
        }
    }
    fn finish(mut self) -> (W, Vec<[u8; 32]>) {
        if self.used != 0 {
            self.digests.push(self.hash.finalize().into());
        }
        (self.output, self.digests)
    }
}
impl<W: Write> Write for Blocks<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write_all(bytes)?;
        let mut rest = bytes;
        while !rest.is_empty() {
            let count = rest.len().min(BLOCK - self.used);
            self.hash.update(&rest[..count]);
            self.used += count;
            rest = &rest[count..];
            if self.used == BLOCK {
                self.digests.push(self.hash.finalize_reset().into());
                self.used = 0;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
struct TeeRead<R, W> {
    input: R,
    output: W,
}
impl<R: Read, W: Write> Read for TeeRead<R, W> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(bytes)?;
        self.output.write_all(&bytes[..count])?;
        Ok(count)
    }
}
pub(crate) struct RawOutput {
    file: Arc<Spool>,
}
impl Write for RawOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&*self.file).write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct StagedReader {
    file: Arc<Spool>,
    start: u64,
    size: u64,
    position: u64,
}
impl Read for StagedReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let length = self
            .size
            .saturating_sub(self.position)
            .min(bytes.len() as u64) as usize;
        let count = self
            .file
            .read_at(&mut bytes[..length], self.start + self.position)?;
        self.position += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stored_bytes(path: &Path) -> u64 {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    stored_bytes(&entry.path())
                } else {
                    entry.metadata().unwrap().len()
                }
            })
            .sum()
    }
    #[test]
    fn fitting_imports_do_not_write_disk_data_or_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        for number in 0..160u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&number.to_le_bytes());
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store
                .insert(oid, bytes.len(), bytes.as_slice(), directory.path())
                .unwrap();
        }
        assert_eq!(
            stored_bytes(directory.path()),
            0,
            "RAM-fitting imports wrote backing data or verification metadata"
        );
        store.evict_all_for_test().unwrap();
        assert_eq!(
            stored_bytes(directory.path()),
            160 * 8192,
            "spill rewrote verification metadata in addition to raw payloads"
        );
        // Rehydrating a previously spilled block must not write it again.
        for number in 0..160u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&number.to_le_bytes());
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            assert_eq!(store.get(oid).unwrap().unwrap().read_all().unwrap(), bytes);
        }
        store.evict_all_for_test().unwrap();
        assert_eq!(stored_bytes(directory.path()), 160 * 8192);
        assert_eq!(store.entries(), 160);
    }
    #[test]
    fn read_slot_releases_its_descriptor_before_replacing_it() {
        const CHILD: &str = "GIT_ON_TEST_RAW_SLOT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "raw_blob::tests::read_slot_releases_its_descriptor_before_replacing_it",
                    "--test-threads=1",
                ])
                .env(CHILD, "1");
            unsafe {
                command.pre_exec(|| {
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    limit.rlim_cur = limit.rlim_cur.min(64);
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let mut readers = Vec::new();
        for bytes in [b"first".as_slice(), b"second"] {
            let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
            readers.push(
                store
                    .insert(oid, bytes.len(), bytes, directory.path())
                    .unwrap(),
            );
        }
        assert_eq!(readers[0].read_all().unwrap(), b"first");
        let mut held = Vec::new();
        loop {
            match File::open("/dev/null") {
                Ok(file) => held.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    break;
                }
            }
        }
        assert_eq!(readers[1].read_all().unwrap(), b"second");
    }
    #[test]
    fn repeated_spilled_ranges_reuse_one_bounded_descriptor_and_check_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let bytes: Vec<_> = (0..4 * BLOCK).map(|n| (n % 251) as u8).collect();
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let reader = store
            .insert(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        let before = store.state.lock().unwrap().directory.open_calls.get();
        for index in 0..64 {
            let offset = (index % 4) * BLOCK;
            assert_eq!(
                reader.read(offset as u64, 1).unwrap(),
                bytes[offset..offset + 1]
            );
        }
        assert_eq!(
            store.state.lock().unwrap().directory.open_calls.get() - before,
            1,
            "consecutive reads of an available raw object reopened its file"
        );
        let path = store
            .state
            .lock()
            .unwrap()
            .directory
            ._owner
            .path()
            .join(oid.to_string());
        let writer = File::options().write(true).open(path).unwrap();
        writer.set_len(bytes.len() as u64 - 1).unwrap();
        assert!(
            reader.read(0, 1).is_err(),
            "descriptor reuse skipped fresh length validation"
        );
        writer.set_len(bytes.len() as u64).unwrap();
        writer.write_all_at(&bytes, 0).unwrap();
        writer.write_all_at(b"!", 0).unwrap();
        assert!(
            reader.read(0, 1).is_err(),
            "descriptor reuse skipped block authentication"
        );
    }
    #[test]
    fn verified_ranges_cross_block_boundaries_and_outlive_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let bytes: Vec<_> = (0..3 * BLOCK + 17).map(|n| (n % 251) as u8).collect();
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let reader = store
            .insert(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        for (start, size) in [
            (BLOCK - 1, 3),
            (BLOCK, BLOCK),
            (2 * BLOCK - 3, 11),
            (3 * BLOCK + 16, 100),
        ] {
            assert_eq!(
                reader.read(start as u64, size as u32).unwrap(),
                bytes[start..(start + size).min(bytes.len())]
            );
        }
        assert!(reader.read(u64::MAX, 100).unwrap().is_empty());
        drop(store);
        assert_eq!(reader.read(96, 700).unwrap(), bytes[96..796]);
        drop(reader);
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none());
    }
    #[test]
    fn mutated_reordered_truncated_or_substituted_blocks_never_get_exposed() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let mut bytes = vec![b'a'; BLOCK];
        bytes.extend(vec![b'b'; BLOCK]);
        bytes.extend(b"end");
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let reader = store
            .insert(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        let file = store
            .state
            .lock()
            .unwrap()
            .directory
            .open(oid, true, bytes.len() as u64)
            .unwrap();
        file.write_all_at(&vec![b'b'; BLOCK], 0).unwrap();
        file.write_all_at(&vec![b'a'; BLOCK], BLOCK as u64).unwrap();
        assert!(reader.read(0, 1).is_err());
        assert!(reader.read(BLOCK as u64, 1).is_err());
        file.write_all_at(&bytes, 0).unwrap();
        file.set_len(bytes.len() as u64 - 1).unwrap();
        assert!(
            reader.read(0, 1).is_err(),
            "untrusted file length replaced the trusted descriptor"
        );
        file.set_len(bytes.len() as u64).unwrap();
        file.write_all_at(&bytes, 0).unwrap();
        assert_eq!(reader.read(0, 1).unwrap(), b"a");
        drop(file);
        let path = store
            .state
            .lock()
            .unwrap()
            .directory
            ._owner
            .path()
            .join(oid.to_string());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &path).unwrap();
        assert!(
            reader.read(0, 1).is_err(),
            "symlink storage substitution was followed"
        );
    }
    #[test]
    fn failed_preparations_do_not_publish_digests_or_damage_existing_data() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let oid = Oid::hash_object(ObjectType::Blob, b"good").unwrap();
        let good = store
            .insert(oid, 4, b"good".as_slice(), directory.path())
            .unwrap();
        let next = Oid::hash_object(ObjectType::Blob, b"next").unwrap();
        for data in [b"evil".as_slice(), b"n", b"next-extra"] {
            assert!(store.insert(next, 4, data, directory.path()).is_err());
            assert!(store.get(next).unwrap().is_none());
            assert_eq!(good.read_all().unwrap(), b"good");
            assert_eq!(
                stored_bytes(directory.path()),
                4,
                "failed staging files leaked"
            );
        }
        let reader = store
            .insert(next, 4, b"next".as_slice(), directory.path())
            .unwrap();
        reader.corrupt_for_test().unwrap();
        assert!(reader.read(0, 1).is_err());
        assert_eq!(store.entries(), 2);
    }
    #[test]
    fn native_staging_is_bounded_and_read_marking_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        store.state.lock().unwrap().scratch =
            Some(Files::with_limit(directory.path(), Some(3)).unwrap());
        let (mut output, start) = store.append_output().unwrap();
        output.write_all(b"headergood\n").unwrap();
        drop(output);
        let oid = Oid::hash_object(ObjectType::Blob, b"good").unwrap();
        let input = store.output_reader(start + 6).unwrap().take(4);
        store
            .import_blob(oid, start + 6, 4, input, directory.path())
            .unwrap();
        store.release_output();
        for first in [true, false] {
            let (blob, used) = store.for_read(oid).unwrap().unwrap();
            assert_eq!(used, first);
            assert_eq!(blob.read_all().unwrap(), b"good");
        }
        assert_eq!(store.entries(), 1);
    }
    #[test]
    fn many_spilled_objects_remain_available_with_one_shared_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        for number in 0..257 {
            let bytes = format!("blob-{number}");
            let oid = Oid::hash_object(ObjectType::Blob, bytes.as_bytes()).unwrap();
            store
                .insert(oid, bytes.len(), bytes.as_bytes(), directory.path())
                .unwrap();
        }
        let mut previous: Option<std::sync::Weak<File>> = None;
        for number in (0..257).rev() {
            let bytes = format!("blob-{number}");
            let oid = Oid::hash_object(ObjectType::Blob, bytes.as_bytes()).unwrap();
            assert_eq!(
                store.get(oid).unwrap().unwrap().read_all().unwrap(),
                bytes.as_bytes()
            );
            if let Some(previous) = previous {
                assert!(
                    previous.upgrade().is_none(),
                    "replaced raw descriptor remained retained"
                );
            }
            let state = store.state.lock().unwrap();
            let (cached, file) = state.directory.reader.as_ref().unwrap();
            assert_eq!(*cached, oid);
            assert_eq!(
                Arc::strong_count(file),
                1,
                "a blob or child reader retained its own file descriptor"
            );
            previous = Some(Arc::downgrade(file));
        }
        assert_eq!(store.disk_metrics().1, 257);
        assert_eq!(store.memory_metrics(), (0, 0));
    }
    #[test]
    fn one_object_retains_one_block_across_readers_without_invalidating_results() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(root.path()).unwrap();
        let mut bytes = vec![b'a'; BLOCK];
        bytes.extend(vec![b'b'; BLOCK]);
        bytes.extend(vec![b'c'; BLOCK]);
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let reader = store
            .insert(oid, bytes.len(), bytes.as_slice(), root.path())
            .unwrap();
        let another = store.get(oid).unwrap().unwrap();
        let first = reader.read(3, 10).unwrap();
        let authenticated = store.tested_authenticated_bytes();
        assert_eq!(another.read(7, 2).unwrap(), b"aa");
        assert_eq!(
            store.tested_authenticated_bytes(),
            authenticated,
            "same-block hit rehashed disk"
        );
        assert_eq!(another.read(BLOCK as u64, 3).unwrap(), b"bbb");
        assert_eq!(
            store.memory_metrics().1,
            1,
            "different FDs or blocks created multiple retained entries"
        );
        assert_eq!(reader.read(3, 10).unwrap(), first);
        assert_eq!(
            store.tested_authenticated_bytes() - authenticated,
            2 * BLOCK as u64,
            "alternating blocks did not miss"
        );
        assert_eq!(
            first, b"aaaaaaaaaa",
            "cache replacement mutated a live result"
        );
        std::thread::scope(|scope| {
            let a = scope.spawn(|| reader.read(0, 10));
            let b = scope.spawn(|| another.read(BLOCK as u64, 10));
            assert_eq!(a.join().unwrap().unwrap(), b"aaaaaaaaaa");
            assert_eq!(b.join().unwrap().unwrap(), b"bbbbbbbbbb");
        });
        assert_eq!(store.memory_metrics().1, 1);
    }
    #[test]
    fn sequential_large_read_does_not_pollute_other_hot_objects() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(root.path()).unwrap();
        let mut hot = Vec::new();
        for n in 0..160u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&n.to_le_bytes());
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            hot.push(
                store
                    .insert(oid, bytes.len(), bytes.as_slice(), root.path())
                    .unwrap(),
            );
        }
        let bytes = vec![b'z'; MEMORY_BUDGET + 2 * BLOCK];
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let large = store
            .insert(oid, bytes.len(), bytes.as_slice(), root.path())
            .unwrap();
        for offset in (0..bytes.len()).step_by(BLOCK) {
            assert_eq!(large.read(offset as u64, 1).unwrap(), b"z");
        }
        let before = store.tested_authenticated_bytes();
        for (n, reader) in hot.iter().enumerate() {
            assert_eq!(reader.read(0, 4).unwrap(), (n as u32).to_le_bytes());
        }
        assert_eq!(
            store.tested_authenticated_bytes(),
            before,
            "one large scan evicted fitting hot files"
        );
        assert_eq!(store.memory_metrics().1, 161);
    }
    #[test]
    fn parsed_tree_names_and_absence_do_not_depend_on_mutable_disk_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let blob = Oid::hash_object(ObjectType::Blob, b"x").unwrap();
        let mut bytes = b"100644 tracked\0".to_vec();
        bytes.extend(blob.as_bytes());
        let oid = Oid::hash_object(ObjectType::Tree, &bytes).unwrap();
        store
            .insert_tree(oid, bytes.len(), bytes.as_slice(), directory.path())
            .unwrap();
        let (tree, first) = store.directory(oid).unwrap();
        assert!(first);
        assert_eq!(
            tree.lookup(std::ffi::OsStr::new("tracked")).unwrap().oid,
            blob
        );
        assert!(tree.lookup(std::ffi::OsStr::new("absent")).is_none());
        assert_eq!(
            stored_bytes(directory.path()),
            0,
            "parsed metadata retained raw trees or disk indexes"
        );
        assert!(!store.directory(oid).unwrap().1);
        let mut duplicate = bytes.clone();
        duplicate.extend(&bytes);
        let oid = Oid::hash_object(ObjectType::Tree, &duplicate).unwrap();
        assert!(store
            .insert_tree(oid, duplicate.len(), duplicate.as_slice(), directory.path())
            .is_err());
        assert!(store.get(oid).unwrap().is_none());
        assert_eq!(tree.children().unwrap().len(), 1);
    }
}
