//! Invocation-local verified contents. Only payload bytes spill to disk;
//! object identity, length and ordered block digests stay in protected RAM.
use crate::{
    spool::{open_private_file, private_file_metadata},
    tree::Directory,
};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::{File, Metadata},
    io::{self, Read, Write},
    ops::Range,
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
};

pub(crate) const MEMORY_BUDGET: usize = 16 * 1024 * 1024;
pub(crate) const COPY_MEMORY_BUDGET: usize = 128 * 1024 * 1024;
// Conservatively covers the LRU node, table slack/control bytes and Vec header.
const ENTRY_CHARGE: usize = 512;
pub(crate) const BLOCK: usize = 64 * 1024;
pub(crate) const LARGE_SLOTS: usize = 8;
const SPILL_WRITE_CHUNK: usize = 64 * BLOCK;

fn digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn write_payload(
    output: &mut impl Write,
    mut bytes: &[u8],
    mut check: impl FnMut() -> Result<()>,
) -> Result<()> {
    while !bytes.is_empty() {
        check()?;
        match output.write(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct SealedWrite<W, F> {
    output: W,
    check: F,
    hasher: blake3::Hasher,
    filled: usize,
    blocks: Vec<[u8; 32]>,
}
impl<W, F> SealedWrite<W, F> {
    fn new(output: W, check: F) -> Self {
        Self {
            output,
            check,
            hasher: blake3::Hasher::new(),
            filled: 0,
            blocks: Vec::new(),
        }
    }
    fn finish(mut self) -> Vec<[u8; 32]> {
        if self.filled != 0 {
            self.blocks.push(*self.hasher.finalize().as_bytes());
        }
        self.blocks
    }
}
impl<W: Write, F: FnMut() -> Result<()>> Write for SealedWrite<W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (self.check)()
            .context("write verified payload")
            .map_err(io::Error::other)?;
        let bytes = &bytes[..bytes.len().min(BLOCK - self.filled)];
        let written = self.output.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        self.filled += written;
        if self.filled == BLOCK {
            self.blocks.push(*self.hasher.finalize().as_bytes());
            self.hasher.reset();
            self.filled = 0;
        }
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
fn write_sealed(
    output: &mut impl Write,
    bytes: &[u8],
    mut check: impl FnMut() -> Result<()>,
) -> Result<Vec<[u8; 32]>> {
    let mut blocks = Vec::with_capacity(bytes.len().div_ceil(BLOCK));
    for chunk in bytes.chunks(SPILL_WRITE_CHUNK) {
        blocks.extend(chunk.chunks(BLOCK).map(digest));
        write_payload(output, chunk, &mut check)?;
    }
    Ok(blocks)
}

fn backing_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<io::Error>() {
        Ok(error) => BackingError(error).into(),
        Err(error) => error,
    }
}

fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
#[derive(Debug)]
pub(crate) struct BackingError(pub io::Error);
impl std::fmt::Display for BackingError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}
impl std::error::Error for BackingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

fn capacity_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSPC | libc::EDQUOT | libc::EFBIG | libc::EMFILE | libc::ENFILE)
    )
}

pub(crate) fn is_retryable_spill(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<BackingError>()
            .is_some_and(|error| capacity_error(&error.0))
    })
}
struct Descriptor {
    size: u64,
    blocks: Box<[[u8; 32]]>,
    file: bool,
    used: bool,
    content_checked: bool,
    directory: Option<Arc<Directory>>,
    pack_readers: usize,
    discard_after_pack: bool,
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
    bytes: Arc<Vec<u8>>,
}
fn check_installed(file: &File, path: &Path, size: u64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let source = file.metadata()?;
    let installed = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        installed.is_file()
            && installed.dev() == source.dev()
            && installed.ino() == source.ino()
            && installed.len() == size,
        "installed blob changed during materialization"
    );
    Ok(())
}

fn read_verified_spill(
    file: &File,
    size: u64,
    blocks: &[[u8; 32]],
    mut consume: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    anyhow::ensure!(
        blocks.len() as u64 == size.div_ceil(BLOCK as u64),
        "incomplete raw blob digests"
    );
    let mut bytes = vec![0; size.min((16 * BLOCK) as u64) as usize];
    let mut offset = 0;
    for expected in blocks.chunks(16) {
        crate::object::check_transfer()?;
        let count = (size - offset).min(bytes.len() as u64) as usize;
        file.read_exact_at(&mut bytes[..count], offset)?;
        for (block, expected) in bytes[..count].chunks(BLOCK).zip(expected) {
            if &digest(block) != expected {
                return Err(invalid("verified raw blob bytes were modified"));
            }
        }
        consume(&bytes[..count])?;
        offset += count as u64;
    }
    Ok(())
}

fn open_copy(
    parent: &File,
    name: &std::ffi::CStr,
    source: Option<&File>,
) -> io::Result<(File, bool)> {
    #[cfg(target_os = "macos")]
    if let Some(source) = source {
        if unsafe { libc::fclonefileat(source.as_raw_fd(), parent.as_raw_fd(), name.as_ptr(), 0) }
            == 0
        {
            return match open_private_file(parent, name, libc::O_RDWR) {
                Ok((file, _)) => Ok((file, true)),
                Err(error) => {
                    unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
                    Err(error)
                }
            };
        }
    }

    let (file, _) = open_private_file(parent, name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
    #[cfg(target_os = "linux")]
    let cloned = source.is_some_and(|source| unsafe {
        libc::ioctl(file.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) == 0
    });
    #[cfg(not(target_os = "linux"))]
    let cloned = false;
    Ok((file, cloned))
}

#[derive(Clone)]
pub(crate) struct RawBlob {
    state: Arc<Mutex<Store>>,
    oid: Oid,
    size: u64,
    kind: ObjectType,
}
struct PackBlob(RawBlob);

impl Drop for PackBlob {
    fn drop(&mut self) {
        let mut store = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let oid = self.0.oid;
        let object = store
            .objects
            .get_mut(&oid)
            .expect("pack's source is pinned");
        object.pack_readers -= 1;
        if object.pack_readers == 0 && object.discard_after_pack {
            if let Err(error) = store.forget_blob(oid) {
                store.failure.get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }
}

struct PackBlobs {
    cache: RawBlobs,
    bases: HashMap<Oid, PackBlob>,
    checked: Option<CheckedBlock>,
}

impl crate::pack_stream::PackOutput for PackBlobs {
    fn place(
        &mut self,
        object: &crate::pack_stream::VerifiedObject,
        as_base: bool,
    ) -> Result<crate::pack_stream::Placement> {
        use crate::pack_stream::Placement;
        if object.kind() != ObjectType::Blob {
            return Ok(Placement::InPack);
        }
        let placed = if as_base {
            match self.bases.entry(object.oid()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    self.cache.pin_pack_blob(object).map(|blob| {
                        slot.insert(blob);
                    })
                }
                std::collections::hash_map::Entry::Occupied(_) => Ok(()),
            }
        } else {
            self.cache.insert_verified_blob(object)
        };
        match placed {
            Ok(()) => Ok(Placement::InOutput),
            Err(error) if is_retryable_spill(&error) => Ok(Placement::Retry(error)),
            Err(error) => Err(error),
        }
    }

    fn with_base(
        &mut self,
        oid: Oid,
        decode: &mut crate::pack_stream::DecodeWithBase<'_>,
    ) -> Result<Option<crate::pack_stream::VerifiedObject>> {
        let Some(blob) = self.bases.get(&oid) else {
            return Ok(None);
        };
        let mut visitor = blob.0.visitor(&mut self.checked);
        decode(
            ObjectType::Blob,
            usize::try_from(visitor.len())?,
            &mut |range, emit| visitor.visit(range, emit),
        )
        .map(Some)
    }
}

impl RawBlob {
    pub(crate) fn kind(&self) -> ObjectType {
        self.kind
    }
    pub(crate) fn len(&self) -> u64 {
        self.size
    }
    pub(crate) fn read(&self, offset: u64, size: u32) -> Result<ReadData> {
        self.range(
            offset,
            self.size.saturating_sub(offset).min(u64::from(size)),
        )
    }
    pub(crate) fn read_all(&self) -> Result<ReadData> {
        self.range(0, self.size)
    }
    pub(crate) fn visit(
        &self,
        range: Range<usize>,
        consume: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        self.visitor(&mut None).visit(range, consume)
    }
    fn visitor<'a>(&'a self, checked: &'a mut Option<CheckedBlock>) -> BlobVisit<'a> {
        let resident = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .whole_blob(self.oid, self.size);
        BlobVisit {
            blob: self,
            resident,
            checked,
        }
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
    fn range(&self, offset: u64, length: u64) -> Result<ReadData> {
        let result = self.checked_range(offset, length);
        if let Err(error) = &result {
            // Reads can outlive the Snapshot lock through an active reader.
            // Preserve storage failures for the remote runner's final status,
            // including after a caller catches EIO or all handles close.
            let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !crate::object::is_cancelled(error) && store.failure.is_none() {
                store.failure = Some(format!("{error:#}"));
            }
        }
        result
    }
    fn checked_range(&self, offset: u64, length: u64) -> Result<ReadData> {
        if length == 0 {
            return Ok(ReadData::Owned(Vec::new()));
        }
        anyhow::ensure!(self.kind == ObjectType::Blob, "tree bytes are not retained");
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let end = offset.checked_add(length).context("raw range end")?;
        if let Some(bytes) = store.whole_blob(self.oid, self.size) {
            drop(store);
            let range = usize::try_from(offset)?..usize::try_from(end)?;
            return Ok(ReadData::Cached { bytes, range });
        }
        let first = usize::try_from(offset / BLOCK as u64)?;
        let last = usize::try_from((end - 1) / BLOCK as u64)?;
        let mut result = Vec::new();
        if first != last {
            result.try_reserve_exact(usize::try_from(length)?)?;
        }
        let mut file = None;
        for block in first..=last {
            crate::object::check_transfer()?;
            let start = block as u64 * BLOCK as u64;
            let from = usize::try_from(offset.saturating_sub(start))?;
            let to = usize::try_from((end - start).min(BLOCK as u64))?;
            let bytes = self.checked_block(&mut store, &mut file, block)?;
            if first == last {
                return Ok(ReadData::Cached {
                    bytes,
                    range: from..to,
                });
            }
            result.extend_from_slice(&bytes[from..to]);
        }
        Ok(ReadData::Owned(result))
    }
    fn checked_block(
        &self,
        store: &mut Store,
        file: &mut Option<Arc<File>>,
        block: usize,
    ) -> Result<Arc<Vec<u8>>> {
        if let Some(resident) = store
            .memory
            .get(&self.oid)
            .filter(|resident| resident.block == block)
        {
            return Ok(resident.bytes.clone());
        }
        let start = block as u64 * BLOCK as u64;
        let size = usize::try_from((self.size - start).min(BLOCK as u64))?;
        let object = store
            .objects
            .get(&self.oid)
            .context("missing verified object descriptor")?;
        anyhow::ensure!(object.file, "missing prepared object block");
        let expected = object.blocks[block];
        if file.is_none() {
            *file = Some(store.directory.reader(self.oid, self.size)?);
        }
        let mut bytes = vec![0; size];
        file.as_ref().unwrap().read_exact_at(&mut bytes, start)?;
        if digest(&bytes) != expected {
            return Err(invalid("verified raw blob bytes were modified"));
        }
        #[cfg(test)]
        {
            store.authenticated_bytes += size as u64;
        }
        let bytes = Arc::new(bytes);
        // Copy mode leaves room for whole blobs. Mounted readers may retain
        // the checked block; a failed optional spill keeps the old resident.
        if !store.copy_cache {
            if let Err(error) = store.admit(self.oid, block, bytes.clone()) {
                if error.downcast_ref::<io::Error>().is_none() {
                    return Err(error);
                }
            }
        }
        Ok(bytes)
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

/// One spill block shared by successive reads of the same delta base.
struct CheckedBlock {
    oid: Oid,
    offset: usize,
    bytes: ReadData,
}

struct BlobVisit<'a> {
    blob: &'a RawBlob,
    resident: Option<Arc<Vec<u8>>>,
    checked: &'a mut Option<CheckedBlock>,
}

impl BlobVisit<'_> {
    fn len(&self) -> u64 {
        self.blob.size
    }

    fn visit(
        &mut self,
        range: Range<usize>,
        mut consume: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.blob.kind == ObjectType::Blob,
            "tree bytes are not retained"
        );
        anyhow::ensure!(range.end as u64 <= self.blob.size, "raw range outside blob");
        if let Some(bytes) = &self.resident {
            for chunk in bytes[range].chunks(BLOCK) {
                crate::object::check_transfer()?;
                consume(chunk)?;
            }
        } else {
            let mut cursor = range.start;
            while cursor < range.end {
                crate::object::check_transfer()?;
                let start = cursor / BLOCK * BLOCK;
                if self
                    .checked
                    .as_ref()
                    .is_none_or(|block| block.oid != self.blob.oid || block.offset != start)
                {
                    let len = (self.blob.size - start as u64).min(BLOCK as u64);
                    *self.checked = Some(CheckedBlock {
                        oid: self.blob.oid,
                        offset: start,
                        bytes: self.blob.range(start as u64, len)?,
                    });
                }
                let bytes = &self.checked.as_ref().unwrap().bytes;
                let end = range.end.min(start + bytes.len());
                consume(&bytes[cursor - start..end - start])?;
                cursor = end;
            }
        }
        Ok(())
    }
}

/// Owns the backing of a filesystem reply until the kernel has consumed it.
#[cfg_attr(test, derive(Debug))]
pub(crate) enum ReadData {
    Owned(Vec<u8>),
    Cached {
        bytes: Arc<Vec<u8>>,
        range: Range<usize>,
    },
}

impl ReadData {
    pub(crate) fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Cached { bytes, range } => bytes[range].to_vec(),
        }
    }
}

impl AsRef<[u8]> for ReadData {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Cached { bytes, range } => &bytes[range.clone()],
        }
    }
}

impl std::ops::Deref for ReadData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

#[cfg(test)]
impl<T: AsRef<[u8]> + ?Sized> PartialEq<T> for ReadData {
    fn eq(&self, other: &T) -> bool {
        self.as_ref() == other.as_ref()
    }
}

/// A descriptor for the owned directory anchors operations even after a path
/// rename. One last-read descriptor is shared by the store; blobs and child
/// handles do not each retain a descriptor. Every disk read rechecks metadata.
struct RawDirectory {
    fd: Arc<File>,
    reader: Option<(Oid, Arc<File>)>,
    _owner: tempfile::TempDir,
    writes: Arc<WriteMetrics>,
    #[cfg(test)]
    open_calls: std::cell::Cell<usize>,
    #[cfg(test)]
    spill_error: Option<i32>,
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
            fd: Arc::new(fd),
            reader: None,
            _owner: owner,
            writes: Arc::new(WriteMetrics {
                #[cfg(feature = "test-metrics")]
                gate: std::env::var_os("GIT_ON_TEST_SPILL_GATE").map(std::path::PathBuf::from),
                ..Default::default()
            }),
            #[cfg(test)]
            open_calls: std::cell::Cell::new(0),
            #[cfg(test)]
            spill_error: None,
        })
    }
    fn validate_size(metadata: &Metadata, size: Option<u64>) -> Result<()> {
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
        let (file, metadata) =
            open_private_file(&self.fd, name, flags).context("open private raw object")?;
        Self::validate_size(&metadata, size)?;
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
                let metadata =
                    private_file_metadata(file).context("validate private raw object")?;
                Self::validate_size(&metadata, Some(size))?;
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
    fn pending_name(&self, name: CString) -> Result<Pending> {
        let file = self.open_name(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, None)?;
        Ok(Pending {
            directory: self.fd.clone(),
            name,
            file,
            committed: false,
        })
    }
    fn start_spill(&self, oid: Oid) -> Result<Pending> {
        #[cfg(test)]
        if let Some(error) = self.spill_error {
            return Err(io::Error::from_raw_os_error(error).into());
        }
        self.pending_name(CString::new(oid.to_string())?)
    }
    fn spill(&self, oid: Oid, bytes: &[u8]) -> Result<Box<[[u8; 32]]>> {
        self.start_spill(oid)?.write(bytes, &self.writes)
    }
}

impl Pending {
    fn write(mut self, bytes: &[u8], writes: &WriteMetrics) -> Result<Box<[[u8; 32]]>> {
        let blocks = write_sealed(
            &mut RawWriter {
                file: &mut self.file,
                totals: writes,
            },
            bytes,
            crate::object::check_transfer,
        )?;
        self.committed = true;
        Ok(blocks.into_boxed_slice())
    }
}

#[derive(Default)]
struct WriteMetrics {
    calls: AtomicU64,
    bytes: AtomicU64,
    #[cfg(feature = "test-metrics")]
    gate: Option<std::path::PathBuf>,
}

struct RawWriter<'a> {
    file: &'a mut File,
    totals: &'a WriteMetrics,
}
impl Write for RawWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _previous = self.totals.calls.fetch_add(1, Ordering::Relaxed);
        let count = self.file.write(bytes)?;
        self.totals.bytes.fetch_add(count as u64, Ordering::Relaxed);
        #[cfg(feature = "test-metrics")]
        if let Some(gate) = &self.totals.gate {
            if _previous == 0 {
                std::fs::write(gate.join("started"), [])?;
                while !gate.join("release").exists() {
                    crate::object::check_transfer().map_err(io::Error::other)?;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            if gate.join("fail").exists() {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct Pending {
    directory: Arc<File>,
    name: CString,
    file: File,
    committed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct RawBlobs {
    state: Arc<Mutex<Store>>,
    prepared: Arc<Condvar>,
}
enum BlobPayload {
    Resident(Arc<Vec<u8>>),
    Spilled(Box<[[u8; 32]]>),
}

struct BlobPreparation<'a> {
    blobs: &'a RawBlobs,
    oid: Oid,
}

impl Drop for BlobPreparation<'_> {
    fn drop(&mut self) {
        let mut store = self.blobs.state.lock().unwrap_or_else(|e| e.into_inner());
        store.preparing.remove(&self.oid);
        self.blobs.prepared.notify_all();
    }
}

impl RawBlobs {
    pub(crate) fn pack_output(&self) -> impl crate::pack_stream::PackOutput + 'static {
        PackBlobs {
            cache: self.clone(),
            bases: HashMap::new(),
            checked: None,
        }
    }

    pub(crate) fn eager_response(&self, oid: Oid) -> crate::pack_stream::Response {
        crate::pack_stream::Response::eager(oid, self.pack_output())
    }

    pub(crate) fn new(directory: &Path) -> Result<Self> {
        Ok(Self {
            state: Arc::new(Mutex::new(Store {
                directory: RawDirectory::new(directory)?,
                objects: HashMap::new(),
                preparing: HashSet::new(),
                packed: HashMap::new(),
                packed_blobs: lru::LruCache::unbounded(),
                packed_blob_bytes: 0,
                memory: lru::LruCache::unbounded(),
                large: lru::LruCache::new(std::num::NonZeroUsize::new(LARGE_SLOTS).unwrap()),
                memory_bytes: 0,
                large_bytes: 0,
                spilling_large: None,
                memory_limit: MEMORY_BUDGET,
                copy_cache: false,
                memory_high_water: 0,
                failure: None,
                blob_entries: 0,
                blob_bytes: 0,
                disk_entries: 0,
                disk_bytes: 0,
                #[cfg(test)]
                authenticated_bytes: 0,
            })),
            prepared: Arc::default(),
        })
    }
    pub(crate) fn enable_copy_cache(&self, limit: usize) {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        store.copy_cache = true;
        store.memory_limit = limit;
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
    pub(crate) fn available(&self, oid: Oid) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.objects.contains_key(&oid)
            || state.packed.contains_key(&oid)
            || state.packed_blobs.contains(&oid)
    }

    pub(crate) fn packed_object(&self, oid: Oid) -> Option<crate::pack_stream::PackedObject> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .packed
            .get(&oid)
            .cloned()
            .or_else(|| state.packed_blobs.get(&oid).cloned())
    }
    pub(crate) fn remember_packed(&self, object: crate::pack_stream::PackedObject) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let oid = object.oid();
        if state.objects.contains_key(&oid) {
            return;
        }
        let Some(capacity) = object.encoded_capacity() else {
            state.packed.entry(oid).or_insert(object);
            return;
        };
        let charge = capacity + ENTRY_CHARGE;
        let limit = state.memory_limit.min(MEMORY_BUDGET);
        if charge > limit || state.packed_blobs.get(&oid).is_some() {
            return;
        }
        while state.packed_blob_bytes > limit - charge {
            let (_, removed) = state.packed_blobs.pop_lru().unwrap();
            state.packed_blob_bytes -= removed.encoded_capacity().unwrap() + ENTRY_CHARGE;
        }
        state.packed_blob_bytes += charge;
        state.packed_blobs.put(oid, object);
    }
    pub(crate) fn forget_packed(&self, oid: Oid) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .forget_packed(oid);
    }

    #[cfg(feature = "test-metrics")]
    pub(crate) fn packed_oids(&self) -> Vec<Oid> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .packed
            .keys()
            .copied()
            .chain(state.packed_blobs.iter().map(|(&oid, _)| oid))
            .collect()
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
    pub(crate) fn insert(&mut self, oid: Oid, size: usize, input: impl Read) -> Result<RawBlob> {
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
    #[cfg(test)]
    fn insert_fetched(&mut self, oid: Oid, size: usize, input: impl Read) -> Result<RawBlob> {
        self.insert_blob(oid, size, false, |output| {
            crate::object::copy_verified(input, output, ObjectType::Blob, size, oid)
        })
    }
    pub(crate) fn insert_verified_blob(
        &self,
        object: &crate::pack_stream::VerifiedObject,
    ) -> Result<()> {
        self.insert_packed(object, false)?;
        Ok(())
    }

    fn pin_pack_blob(&self, object: &crate::pack_stream::VerifiedObject) -> Result<PackBlob> {
        Ok(PackBlob(self.insert_packed(object, true)?))
    }

    fn insert_packed(
        &self,
        object: &crate::pack_stream::VerifiedObject,
        pin: bool,
    ) -> Result<RawBlob> {
        anyhow::ensure!(
            object.kind() == ObjectType::Blob,
            "streamed object is not a blob"
        );
        self.insert_with(object.oid(), object.data().len(), false, pin, || {
            Ok(BlobPayload::Resident(object.shared_data()))
        })
        .map_err(|error| {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(capacity_error)
            {
                backing_error(error)
            } else {
                error
            }
        })
    }

    fn insert_blob(
        &mut self,
        oid: Oid,
        size: usize,
        used: bool,
        transfer: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<RawBlob> {
        self.insert_with(oid, size, used, false, || {
            let backing = {
                let store = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if size > store.whole_limit(size) {
                    Some((
                        store.directory.start_spill(oid)?,
                        store.directory.writes.clone(),
                    ))
                } else {
                    None
                }
            };
            if let Some((mut pending, totals)) = backing {
                let output = RawWriter {
                    file: &mut pending.file,
                    totals: &totals,
                };
                let mut output = SealedWrite::new(output, crate::object::check_transfer);
                transfer(&mut output)?;
                let blocks = output.finish();
                anyhow::ensure!(
                    pending.file.metadata()?.len() == size as u64,
                    "verified blob length changed"
                );
                crate::object::check_transfer()?;
                pending.committed = true;
                return Ok(BlobPayload::Spilled(blocks.into_boxed_slice()));
            }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(size)?;
            transfer(&mut bytes)?;
            Ok(BlobPayload::Resident(Arc::new(bytes)))
        })
    }

    fn insert_with(
        &self,
        oid: Oid,
        size: usize,
        used: bool,
        pin: bool,
        load: impl FnOnce() -> Result<BlobPayload>,
    ) -> Result<RawBlob> {
        let mut store = self.wait_for_preparation(oid);
        if let Some(object) = store.objects.get_mut(&oid) {
            anyhow::ensure!(object.kind() == ObjectType::Blob, "object is not a blob");
            object.pack_readers += usize::from(pin);
            return Ok(self.blob(oid, object));
        }
        store.preparing.insert(oid);
        drop(store);
        let _preparation = BlobPreparation { blobs: self, oid };
        let payload = load()?;
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let blocks = match payload {
            BlobPayload::Resident(bytes) => {
                anyhow::ensure!(bytes.len() == size, "verified blob length changed");
                if !store.copy_cache && store.memory_limit != 0 && size > BLOCK {
                    store = self.make_large_room(store)?;
                }
                store.retain_blob(oid, bytes)?
            }
            BlobPayload::Spilled(blocks) => Some(blocks),
        };
        let file = blocks.is_some();
        store.objects.insert(
            oid,
            Descriptor {
                size: size as u64,
                blocks: blocks.unwrap_or_default(),
                file,
                used,
                content_checked: false,
                directory: None,
                pack_readers: usize::from(pin),
                discard_after_pack: false,
            },
        );
        store.forget_packed(oid);
        store.blob_entries += 1;
        store.blob_bytes += size as u64;
        if file {
            store.disk_entries += 1;
            store.disk_bytes += size as u64;
        }
        Ok(self.blob(oid, store.objects.get(&oid).unwrap()))
    }

    fn wait_for_preparation(&self, oid: Oid) -> MutexGuard<'_, Store> {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.prepared
            .wait_while(store, |store| {
                store.preparing.contains(&oid) || store.spilling_large == Some(oid)
            })
            .unwrap_or_else(|e| e.into_inner())
    }

    fn make_large_room<'a>(
        &'a self,
        mut store: MutexGuard<'a, Store>,
    ) -> Result<MutexGuard<'a, Store>> {
        #[cfg(feature = "test-metrics")]
        let mut reported_wait = false;
        while store.large.len() == store.large.cap().get() {
            if store.spilling_large.is_some() {
                #[cfg(feature = "test-metrics")]
                if let Some(gate) = store
                    .directory
                    .writes
                    .gate
                    .as_ref()
                    .filter(|_| !reported_wait)
                {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(gate.join("waiting"))?
                        .write_all(b"1")?;
                    reported_wait = true;
                }
                store = self.prepared.wait(store).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            crate::object::check_transfer()?;
            let (&oid, bytes) = store
                .large
                .peek_lru()
                .context("missing large resident blob")?;
            let bytes = bytes.clone();
            let pending = store.directory.start_spill(oid)?;
            let writes = store.directory.writes.clone();
            store.spilling_large = Some(oid);
            drop(store);
            // The victim remains readable in memory until the write commits.
            let result = pending.write(&bytes, &writes);
            store = self.state.lock().unwrap_or_else(|e| e.into_inner());
            store.spilling_large = None;
            self.prepared.notify_all();
            let spill = result.map(|blocks| store.record_large_spill(oid, blocks));
            if store.objects[&oid].discard_after_pack && store.objects[&oid].pack_readers == 0 {
                store.forget_blob(oid)?;
            }
            spill?;
        }
        Ok(store)
    }
    pub(crate) fn insert_tree(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
    ) -> Result<RawBlob> {
        self.store_tree(oid, size, || Directory::verified(oid, size, input))
    }
    pub(crate) fn insert_verified_tree(
        &mut self,
        object: &crate::pack_stream::VerifiedObject,
    ) -> Result<()> {
        self.store_tree(object.oid(), object.data().len(), || {
            Directory::from_verified_pack(object)
        })?;
        Ok(())
    }
    fn store_tree(
        &mut self,
        oid: Oid,
        size: usize,
        load: impl FnOnce() -> Result<Directory>,
    ) -> Result<RawBlob> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(object) = store.objects.get(&oid) {
            return Ok(self.blob(oid, object));
        }
        let directory = Arc::new(load()?);
        store.objects.insert(
            oid,
            Descriptor {
                size: size as u64,
                blocks: Box::new([]),
                file: false,
                used: false,
                content_checked: false,
                directory: Some(directory),
                pack_readers: 0,
                discard_after_pack: false,
            },
        );
        store.forget_packed(oid);
        Ok(self.blob(oid, store.objects.get(&oid).unwrap()))
    }
    pub(crate) fn promote<T>(
        &self,
        oid: Oid,
        path: &Path,
        exclusive: bool,
        change: impl FnOnce(&File) -> io::Result<T>,
    ) -> Result<(File, u64, T)> {
        self.transfer(oid, path, exclusive, false, |file| {
            change(file).map_err(|error| BackingError(error).into())
        })
    }

    pub(crate) fn install_spill(&self, oid: Oid, path: &Path) -> Result<(File, u64)> {
        let (file, size, ()) = self.transfer(oid, path, true, true, |_| Ok(()))?;
        Ok((file, size))
    }

    fn transfer<T>(
        &self,
        oid: Oid,
        path: &Path,
        exclusive: bool,
        verify_materialization: bool,
        use_file: impl FnOnce(&File) -> Result<T>,
    ) -> Result<(File, u64, T)> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let parent = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.parent().context("backing parent")?)
            .map_err(BackingError)?;
        let name = CString::new(path.file_name().context("backing name")?.as_bytes())?;
        let mut created = false;
        let result = (|| {
            let mut store = self.wait_for_preparation(oid);
            let object = store.objects.get(&oid).context("missing prepared source")?;
            anyhow::ensure!(
                object.kind() == ObjectType::Blob,
                "mutable source is not a blob"
            );
            anyhow::ensure!(
                !verify_materialization || object.file,
                "missing materialization spill"
            );
            let size = object.size;
            // An in-flight pack or another source inode can still need these bytes.
            let movable = exclusive && object.pack_readers == 0;
            let raw_name = CString::new(oid.to_string())?;
            let mut input = if object.file {
                Some(
                    store
                        .directory
                        .open_name(
                            &raw_name,
                            if movable {
                                libc::O_RDWR
                            } else {
                                libc::O_RDONLY
                            },
                            Some(size),
                        )
                        .map_err(|error| {
                            match error
                                .downcast_ref::<io::Error>()
                                .and_then(io::Error::raw_os_error)
                            {
                                Some(code @ (libc::EMFILE | libc::ENFILE)) => {
                                    BackingError(io::Error::from_raw_os_error(code)).into()
                                }
                                _ => error,
                            }
                        })?,
                )
            } else {
                None
            };
            let linked = if movable && input.is_some() {
                // Reserve the writable name without losing the raw name.
                // A first write that changes no bytes can then leave the
                // original cache and its digests intact.
                let result = unsafe {
                    libc::linkat(
                        store.directory.fd.as_raw_fd(),
                        raw_name.as_ptr(),
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        0,
                    )
                };
                if result == 0 {
                    true
                } else {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        error.raw_os_error(),
                        Some(libc::ENOTSUP | libc::EPERM | libc::EXDEV | libc::ENOSYS)
                    ) {
                        return Err(BackingError(error)).context("transfer prepared source");
                    }
                    false
                }
            } else {
                false
            };
            let (mut output, cloned) = if linked {
                (input.take().unwrap(), true)
            } else {
                open_copy(&parent, &name, input.as_ref()).map_err(BackingError)?
            };
            created = true;
            if let Some(input) = input {
                if !cloned {
                    read_verified_spill(&input, size, &object.blocks, |bytes| {
                        write_payload(&mut output, bytes, crate::object::check_transfer)
                            .map_err(backing_error)
                    })?;
                }
            } else if !object.file && size != 0 {
                let resident = store
                    .large
                    .peek(&oid)
                    .or_else(|| {
                        store
                            .memory
                            .peek(&oid)
                            .filter(|resident| resident.block == 0)
                            .map(|resident| &resident.bytes)
                    })
                    .context("missing resident source")?;
                anyhow::ensure!(resident.len() as u64 == size, "incomplete resident source");
                for chunk in resident.chunks(16 * BLOCK) {
                    write_payload(&mut output, chunk, crate::object::check_transfer)
                        .map_err(backing_error)?;
                }
            }
            if object.file {
                // A linked or cloned name is provisional. Check its contents
                // before the write operation can publish writable backing.
                check_installed(&output, path, size)?;
                if cloned {
                    read_verified_spill(&output, size, &object.blocks, |_| Ok(()))?;
                }
            }
            let result = use_file(&output)?;
            if exclusive {
                if let Err(error) = store.forget_blob(oid) {
                    // A completed modification owns the payload even if
                    // unlink of the old private name fails. Never read it
                    // again as immutable. Invocation cleanup owns that name.
                    store.release_blob(oid);
                    store.failure.get_or_insert_with(|| format!("{error:#}"));
                }
            }
            Ok((output, size, result))
        })();
        if let Err(error) = &result {
            if error.downcast_ref::<BackingError>().is_none() && !crate::object::is_cancelled(error)
            {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failure
                    .get_or_insert_with(|| format!("{error:#}"));
            }
            if created {
                unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
            }
        }
        result
    }

    pub(crate) fn discard(&self, oid: Oid) -> Result<()> {
        self.wait_for_preparation(oid).forget_blob(oid)
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
    pub(crate) fn memory_limit(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .memory_limit
    }
    pub(crate) fn large_memory_metrics(&self) -> (usize, usize) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (store.large_bytes, store.large.len())
    }
    pub(crate) fn packed_blob_metrics(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.packed_blob_bytes, state.packed_blobs.len())
    }
    pub(crate) fn disk_metrics(&self) -> (u64, u64) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (store.disk_bytes, store.disk_entries)
    }
    pub(crate) fn write_metrics(&self) -> (u64, u64) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let writes = &store.directory.writes;
        (
            writes.calls.load(Ordering::Relaxed),
            writes.bytes.load(Ordering::Relaxed),
        )
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
        let mut store = self.state.lock().unwrap();
        store.make_room(MEMORY_BUDGET)?;
        while !store.large.is_empty() {
            store.evict_large()?;
        }
        Ok(())
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
    preparing: HashSet<Oid>,
    packed: HashMap<Oid, crate::pack_stream::PackedObject>,
    packed_blobs: lru::LruCache<Oid, crate::pack_stream::PackedObject>,
    packed_blob_bytes: usize,
    memory: lru::LruCache<Oid, Resident>,
    large: lru::LruCache<Oid, Arc<Vec<u8>>>,
    memory_bytes: usize,
    large_bytes: usize,
    spilling_large: Option<Oid>,
    memory_limit: usize,
    copy_cache: bool,
    memory_high_water: usize,
    failure: Option<String>,
    blob_entries: u64,
    blob_bytes: u64,
    disk_entries: u64,
    disk_bytes: u64,
    #[cfg(test)]
    authenticated_bytes: u64,
}
impl Store {
    fn forget_packed(&mut self, oid: Oid) {
        self.packed.remove(&oid);
        if let Some(removed) = self.packed_blobs.pop(&oid) {
            self.packed_blob_bytes -= removed.encoded_capacity().unwrap() + ENTRY_CHARGE;
        }
    }

    fn whole_blob(&mut self, oid: Oid, size: u64) -> Option<Arc<Vec<u8>>> {
        self.large.get(&oid).cloned().or_else(|| {
            let resident = self.memory.get(&oid)?;
            (resident.block == 0 && resident.bytes.len() as u64 == size)
                .then(|| resident.bytes.clone())
        })
    }
    fn forget_blob(&mut self, oid: Oid) -> Result<()> {
        if let Some(object) = self.objects.get_mut(&oid) {
            anyhow::ensure!(
                object.kind() == ObjectType::Blob,
                "cannot discard a directory"
            );
            if object.pack_readers != 0 || self.spilling_large == Some(oid) {
                object.discard_after_pack = true;
                return Ok(());
            }
            if object.file
                && unsafe {
                    libc::unlinkat(
                        self.directory.fd.as_raw_fd(),
                        CString::new(oid.to_string())?.as_ptr(),
                        0,
                    )
                } != 0
            {
                return Err(io::Error::last_os_error()).context("release consumed source");
            }
        }
        self.release_blob(oid);
        Ok(())
    }
    fn release_blob(&mut self, oid: Oid) {
        if let Some(object) = self.objects.remove(&oid) {
            if object.file {
                if self
                    .directory
                    .reader
                    .as_ref()
                    .is_some_and(|(key, _)| *key == oid)
                {
                    self.directory.reader = None;
                }
                self.disk_entries -= 1;
                self.disk_bytes -= object.size;
            }
        }
        if let Some(bytes) = self.memory.pop(&oid) {
            self.memory_bytes -= bytes.bytes.capacity() + ENTRY_CHARGE;
        }
        if let Some(bytes) = self.large.pop(&oid) {
            self.large_bytes -= bytes.capacity();
        }
    }
    fn whole_limit(&self, size: usize) -> usize {
        if self.copy_cache || size <= BLOCK || self.memory_limit == 0 {
            self.memory_limit.saturating_sub(ENTRY_CHARGE)
        } else {
            usize::MAX
        }
    }
    fn retain_blob(&mut self, oid: Oid, bytes: Arc<Vec<u8>>) -> Result<Option<Box<[[u8; 32]]>>> {
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.capacity() > self.whole_limit(bytes.len()) {
            return Ok(Some(self.directory.spill(oid, &bytes)?));
        }
        if self.copy_cache || bytes.len() <= BLOCK {
            self.admit(oid, 0, bytes)?;
        } else {
            debug_assert!(self.large.len() < self.large.cap().get());
            self.large_bytes += bytes.capacity();
            self.large.put(oid, bytes);
        }
        Ok(None)
    }
    #[cfg(test)]
    fn evict_large(&mut self) -> Result<()> {
        let (&oid, bytes) = self
            .large
            .peek_lru()
            .context("missing large resident blob")?;
        let blocks = self.directory.spill(oid, bytes)?;
        self.record_large_spill(oid, blocks);
        Ok(())
    }
    fn record_large_spill(&mut self, oid: Oid, blocks: Box<[[u8; 32]]>) {
        self.record_spill(oid, blocks);
        self.large_bytes -= self.large.pop(&oid).unwrap().capacity();
    }
    fn record_spill(&mut self, oid: Oid, blocks: Box<[[u8; 32]]>) {
        let object = self.objects.get_mut(&oid).unwrap();
        object.blocks = blocks;
        object.file = true;
        self.disk_entries += 1;
        self.disk_bytes += object.size;
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
                let blocks = self.directory.spill(oid, &resident.bytes)?;
                self.record_spill(oid, blocks);
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
    fn admit(&mut self, oid: Oid, block: usize, bytes: Arc<Vec<u8>>) -> Result<()> {
        let weight = bytes
            .capacity()
            .checked_add(ENTRY_CHARGE)
            .context("resident allocation size")?;
        if weight > self.memory_limit {
            return Ok(());
        }
        if let Some(previous) = self.memory.peek(&oid) {
            let previous_weight = previous.bytes.capacity() + ENTRY_CHARGE;
            if weight <= previous_weight {
                // The existing node already covers this allocation. Replacing
                // its payload and promoting it cannot require another eviction.
                // In particular, a one-object scan must not free/reallocate its
                // LRU node and shrink/regrow the table for every next block.
                *self.memory.get_mut(&oid).unwrap() = Resident { block, bytes };
                self.memory_bytes = self.memory_bytes - previous_weight + weight;
                return Ok(());
            }
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
#[cfg(test)]
mod tests {
    use super::*;
    fn under_descriptor_limit(name: &str) -> bool {
        const CHILD: &str = "GIT_ON_TEST_RAW_NOFILE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
                0
            );
            let mut limit = unsafe { limit.assume_init() };
            limit.rlim_cur = limit.rlim_cur.min(64);
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }
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
    fn source_promotion_survives_descriptor_pressure() {
        if !under_descriptor_limit("raw_blob::tests::source_promotion_survives_descriptor_pressure")
        {
            return;
        }
        for available in [1, 2] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = RawBlobs::disk_only(directory.path()).unwrap();
            let data = vec![b'x'; BLOCK + 3];
            let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
            let original = store.insert(oid, data.len(), data.as_slice()).unwrap();
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
            for _ in 0..available {
                held.pop();
            }
            let result = store.promote(oid, &directory.path().join("mutable"), true, |_| Ok(()));
            drop(held);
            match result {
                Ok((file, _, ())) => {
                    assert_eq!(available, 2);
                    let mut bytes = vec![0; data.len()];
                    file.read_exact_at(&mut bytes, 0).unwrap();
                    assert_eq!(bytes, data);
                }
                Err(error) => {
                    assert_eq!(available, 1);
                    assert!(
                        error.chain().any(|cause| cause
                            .downcast_ref::<io::Error>()
                            .is_some_and(|e| e.raw_os_error() == Some(libc::EMFILE))),
                        "{error:#}"
                    );
                    assert_eq!(
                        original
                            .read_all()
                            .expect("EMFILE lost the prepared source"),
                        data
                    );
                    assert!(
                        store.failure().is_none(),
                        "descriptor pressure poisoned the source"
                    );
                    let (file, size, ()) = store
                        .promote(oid, &directory.path().join("retry"), true, |_| Ok(()))
                        .unwrap();
                    let mut bytes = vec![0; size as usize];
                    file.read_exact_at(&mut bytes, 0).unwrap();
                    assert_eq!(bytes, data);
                }
            }
        }
    }
    #[test]
    fn source_promotion_splits_aliases_then_transfers_the_last_inode() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let data = vec![b'x'; BLOCK + 3];
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let original = store.insert(oid, data.len(), data.as_slice()).unwrap();
        let source_inode = store
            .state
            .lock()
            .unwrap()
            .directory
            .open(oid, false, data.len() as u64)
            .unwrap()
            .metadata()
            .unwrap()
            .ino();
        store
            .promote(oid, &directory.path().join("first"), false, |file| {
                file.write_all_at(b"y", 0)
            })
            .unwrap();
        assert_eq!(original.read_all().unwrap(), data);
        let destination = directory.path().join("last");
        let failure = store
            .promote::<()>(oid, &destination, true, |_| {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            })
            .unwrap_err();
        assert!(is_retryable_spill(&failure));
        assert!(!destination.exists());
        assert_eq!(original.read_all().unwrap(), data);
        let (mut last, _) = store.install_spill(oid, &destination).unwrap();
        assert_eq!(
            last.metadata().unwrap().ino(),
            source_inode,
            "last source was copied"
        );
        let mut bytes = Vec::new();
        last.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, data);
        assert!(store.get(oid).unwrap().is_none());
        assert_eq!(store.disk_metrics(), (0, 0));
    }
    #[test]
    fn empty_resident_source_can_become_writable_without_a_payload_entry() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let oid = Oid::hash_object(ObjectType::Blob, b"").unwrap();
        store.insert(oid, 0, b"".as_slice()).unwrap();
        let (file, _, ()) = store
            .promote(oid, &directory.path().join("empty"), true, |file| {
                file.write_all_at(b"new", 0)
            })
            .unwrap();
        assert_eq!(file.metadata().unwrap().len(), 3);
        assert!(store.get(oid).unwrap().is_none());
    }
    #[test]
    fn installing_or_promoting_a_spill_rejects_corruption() {
        for exclusive in [None, Some(true), Some(false)] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = RawBlobs::disk_only(directory.path()).unwrap();
            let data = vec![b'x'; BLOCK + 3];
            let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
            let original = store.insert(oid, data.len(), data.as_slice()).unwrap();
            original.corrupt_for_test().unwrap();
            let destination = directory.path().join("destination");
            let result = if let Some(exclusive) = exclusive {
                store
                    .promote::<()>(oid, &destination, exclusive, |_| {
                        panic!("corrupted source accepted")
                    })
                    .map(|_| ())
            } else {
                store.install_spill(oid, &destination).map(|_| ())
            };
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("bytes were modified"));
            assert!(!destination.exists());
            assert!(store.failure().is_some());
        }
    }
    #[test]
    fn large_hot_cache_retains_two_objects_over_the_former_byte_limit() {
        let directory = tempfile::tempdir().unwrap();
        let store = RawBlobs::new(directory.path()).unwrap();
        let size = 65 * 1024 * 1024;
        for fill in [1, 2] {
            let oid = Oid::from_bytes(&[fill; 20]).unwrap();
            let bytes = Arc::new(vec![fill; size]);
            store
                .insert_with(oid, size, false, false, || Ok(BlobPayload::Resident(bytes)))
                .unwrap();
        }
        assert_eq!(store.large_memory_metrics(), (2 * size, 2));
        assert_eq!(store.write_metrics(), (0, 0));
    }
    #[test]
    fn fitting_imports_do_not_write_disk_data_or_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        for number in 0..160u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&number.to_le_bytes());
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
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
    fn spill_collision_preserves_the_resident_and_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(root.path()).unwrap();
        let bytes = b"verified resident";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let reader = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
        let path = store
            .state
            .lock()
            .unwrap()
            .directory
            ._owner
            .path()
            .join(oid.to_string());
        std::fs::write(&path, b"unexpected existing entry").unwrap();
        assert!(store.evict_all_for_test().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"unexpected existing entry");
        assert_eq!(reader.read_all().unwrap(), bytes);
        assert_eq!(store.memory_metrics().1, 1);
        assert_eq!(store.disk_metrics(), (0, 0));
        assert!(!reader.on_disk());
        std::fs::remove_file(&path).unwrap();
        store.evict_all_for_test().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(reader.read_all().unwrap(), bytes);
    }
    #[test]
    fn failed_spill_removes_partial_file_without_publishing_or_evicting() {
        const CHILD: &str = "GIT_ON_TEST_PARTIAL_SPILL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "raw_blob::tests::failed_spill_removes_partial_file_without_publishing_or_evicting",
                    "--test-threads=1",
                ])
                .env(CHILD, "1");
            unsafe {
                command.pre_exec(|| {
                    libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                    let limit = libc::rlimit {
                        rlim_cur: 3,
                        rlim_max: 3,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
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
        let root = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(root.path()).unwrap();
        let bytes = b"verified resident";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let reader = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
        assert!(store.evict_all_for_test().is_err());
        let state = store.state.lock().unwrap();
        assert!(std::fs::read_dir(state.directory._owner.path())
            .unwrap()
            .next()
            .is_none());
        assert!(!state.objects[&oid].file);
        drop(state);
        assert_eq!(store.disk_metrics(), (0, 0));
        assert_eq!(store.memory_metrics().1, 1);
        assert_eq!(reader.read_all().unwrap(), bytes);
    }
    #[test]
    fn optional_cache_spill_failure_preserves_checked_bytes_and_resident() {
        for error in [libc::ENOSPC, libc::EDQUOT] {
            let root = tempfile::tempdir().unwrap();
            let mut store = RawBlobs::new(root.path()).unwrap();
            store.state.lock().unwrap().memory_limit = BLOCK + ENTRY_CHARGE;
            let mut large = vec![b'a'; BLOCK];
            large.extend(vec![b'b'; BLOCK]);
            let large_oid = Oid::hash_object(ObjectType::Blob, &large).unwrap();
            let large_reader = store
                .insert(large_oid, large.len(), large.as_slice())
                .unwrap();
            store.evict_all_for_test().unwrap();
            assert_eq!(large_reader.read(0, 1).unwrap(), b"a");
            let small = vec![b's'; BLOCK];
            let small_oid = Oid::hash_object(ObjectType::Blob, &small).unwrap();
            let small_reader = store
                .insert(small_oid, small.len(), small.as_slice())
                .unwrap();
            assert!(!small_reader.on_disk());
            store.state.lock().unwrap().directory.spill_error = Some(error);

            assert_eq!(
                large_reader.read(BLOCK as u64, 1).unwrap(),
                b"b",
                "cache spill errno {error}"
            );
            assert_eq!(store.failure(), None);
            assert_eq!(store.memory_metrics(), (BLOCK + ENTRY_CHARGE, 1));
            assert_eq!(store.disk_metrics(), (2 * BLOCK as u64, 1));
            assert!(!small_reader.on_disk());
            assert_eq!(small_reader.read(0, 1).unwrap(), b"s");
            assert_eq!(stored_bytes(root.path()), (2 * BLOCK) as u64);

            store.state.lock().unwrap().directory.spill_error = None;
            assert_eq!(large_reader.read(BLOCK as u64, 1).unwrap(), b"b");
            assert!(small_reader.on_disk());
            assert_eq!(store.failure(), None);
            assert_eq!(small_reader.read(0, 1).unwrap(), b"s");
        }
    }
    #[test]
    fn read_slot_releases_its_descriptor_before_replacing_it() {
        if !under_descriptor_limit(
            "raw_blob::tests::read_slot_releases_its_descriptor_before_replacing_it",
        ) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let mut readers = Vec::new();
        for bytes in [b"first".as_slice(), b"second"] {
            let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
            readers.push(store.insert(oid, bytes.len(), bytes).unwrap());
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
        let reader = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
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
        let reader = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
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
        let reader = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
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
        let good = store.insert(oid, 4, b"good".as_slice()).unwrap();
        let next = Oid::hash_object(ObjectType::Blob, b"next").unwrap();
        for data in [b"evil".as_slice(), b"n", b"next-extra"] {
            assert!(store.insert(next, 4, data).is_err());
            assert!(store.get(next).unwrap().is_none());
            assert_eq!(good.read_all().unwrap(), b"good");
            assert_eq!(
                stored_bytes(directory.path()),
                4,
                "failed staging files leaked"
            );
        }
        let reader = store.insert(next, 4, b"next".as_slice()).unwrap();
        reader.corrupt_for_test().unwrap();
        assert!(reader.read(0, 1).is_err());
        assert_eq!(store.entries(), 2);
    }
    #[test]
    fn fetched_read_marking_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let oid = Oid::hash_object(ObjectType::Blob, b"good").unwrap();
        store.insert_fetched(oid, 4, b"good".as_slice()).unwrap();
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
            store.insert(oid, bytes.len(), bytes.as_bytes()).unwrap();
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
        let reader = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
        store.state.lock().unwrap().evict_large().unwrap();
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
            hot.push(store.insert(oid, bytes.len(), bytes.as_slice()).unwrap());
        }
        let bytes = vec![b'z'; MEMORY_BUDGET + 2 * BLOCK];
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let large = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
        store.state.lock().unwrap().evict_large().unwrap();
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
    fn block_replacement_preserves_byte_budget_recency_and_spill_victims() {
        fn check(store: &RawBlobs, expected: &[(Oid, usize)]) {
            let state = store.state.lock().unwrap();
            assert_eq!(
                state
                    .memory
                    .iter()
                    .map(|(oid, resident)| (*oid, resident.block))
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                state.memory_bytes,
                state
                    .memory
                    .iter()
                    .map(|(_, resident)| resident.bytes.capacity() + ENTRY_CHARGE)
                    .sum::<usize>()
            );
            assert!(state.memory_bytes <= state.memory_limit);
        }
        let root = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(root.path()).unwrap();
        store.state.lock().unwrap().memory_limit = 2 * (BLOCK + ENTRY_CHARGE);
        let hot_data = vec![b'h'; BLOCK];
        let hot_oid = Oid::hash_object(ObjectType::Blob, &hot_data).unwrap();
        let hot = store
            .insert(hot_oid, hot_data.len(), hot_data.as_slice())
            .unwrap();
        let mut data = vec![b'a'; BLOCK];
        data.extend(vec![b'b'; BLOCK]);
        data.extend([b'c'; 17]);
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let reader = store.insert(oid, data.len(), data.as_slice()).unwrap();
        store.state.lock().unwrap().evict_large().unwrap();
        let first = reader.read(0, 3).unwrap();
        check(&store, &[(oid, 0), (hot_oid, 0)]);
        assert_eq!(reader.read(BLOCK as u64, 3).unwrap(), b"bbb");
        check(&store, &[(oid, 1), (hot_oid, 0)]);
        assert_eq!(reader.read((2 * BLOCK) as u64, 3).unwrap(), b"ccc");
        check(&store, &[(oid, 2), (hot_oid, 0)]);
        assert_eq!(store.memory_metrics().0, BLOCK + 17 + 2 * ENTRY_CHARGE);
        assert!(!store.state.lock().unwrap().objects[&hot_oid].file);
        let extra_data = vec![b'e'; BLOCK - 17 - ENTRY_CHARGE];
        let extra_oid = Oid::hash_object(ObjectType::Blob, &extra_data).unwrap();
        store
            .insert(extra_oid, extra_data.len(), extra_data.as_slice())
            .unwrap();
        check(&store, &[(extra_oid, 0), (oid, 2), (hot_oid, 0)]);
        // A growing replacement must still evict/spill the actual LRU victim.
        assert_eq!(reader.read(0, 3).unwrap(), b"aaa");
        check(&store, &[(oid, 0), (extra_oid, 0)]);
        assert!(store.state.lock().unwrap().objects[&hot_oid].file);
        assert_eq!(hot.read(0, 3).unwrap(), b"hhh");
        check(&store, &[(hot_oid, 0), (oid, 0)]);
        assert_eq!(first, b"aaa", "replacement changed an active result");
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
            .insert_tree(oid, bytes.len(), bytes.as_slice())
            .unwrap();
        let (tree, first) = store.directory(oid).unwrap();
        assert!(first);
        assert_eq!(
            tree.lookup(std::ffi::OsStr::new("tracked")).unwrap(),
            (0o100644, blob)
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
            .insert_tree(oid, duplicate.len(), duplicate.as_slice())
            .is_err());
        assert!(store.get(oid).unwrap().is_none());
        assert_eq!(tree.children().unwrap().len(), 1);
    }
    #[test]
    fn large_blobs_spill_only_on_eviction() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let mut readers: Vec<RawBlob> = Vec::new();
        let mut held = None;
        for index in 0..=LARGE_SLOTS {
            let data = vec![index as u8; BLOCK + 1];
            let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
            if index == LARGE_SLOTS {
                assert_eq!(store.write_metrics(), (0, 0));
                assert_eq!(readers[0].read(0, 1).unwrap(), b"\0");
            }
            readers.push(store.insert(oid, data.len(), data.as_slice()).unwrap());
            if index == 1 {
                let cached = readers[1].read(1, 3).unwrap();
                let bytes = store
                    .state
                    .lock()
                    .unwrap()
                    .whole_blob(oid, data.len() as u64)
                    .unwrap();
                assert_eq!(cached.as_ptr(), bytes[1..].as_ptr());
                held = Some(cached);
            }
        }
        assert!(!readers[0].on_disk());
        assert!(readers[1].on_disk());
        assert_eq!(store.disk_metrics(), ((BLOCK + 1) as u64, 1));
        assert_eq!(held.unwrap(), [1; 3]);
        readers[1].corrupt_for_test().unwrap();
        assert!(readers[1].read(0, 1).is_err());
    }

    #[test]
    fn spill_write_checks_for_cancellation_after_a_partial_write() {
        struct Partial(Vec<u8>);
        impl Write for Partial {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(bytes.len() <= SPILL_WRITE_CHUNK);
                self.0.extend_from_slice(&bytes[..1]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut checks = 0;
        let mut output = Partial(Vec::new());
        let error = write_sealed(&mut output, &vec![b'a'; SPILL_WRITE_CHUNK + 1], || {
            checks += 1;
            if checks == 2 {
                return Err(crate::object::OperationCancelled.into());
            }
            Ok(())
        })
        .unwrap_err();
        assert!(crate::object::is_cancelled(&error));
        assert_eq!(output.0, b"a");
    }
}
