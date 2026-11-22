//! Invocation-local verified contents. Only payload bytes spill to disk;
//! object identity, length and ordered block digests stay in protected RAM.
use crate::{
    spool::{open_private_descriptor, open_private_file, private_file_metadata},
    tree::Directory,
};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, OpenOptionsExt},
        },
    },
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock,
    },
    thread,
    time::Duration,
};

pub(crate) const MEMORY_BUDGET: usize = 16 * 1024 * 1024;
// Conservatively covers the LRU node, table slack/control bytes and Vec header.
const ENTRY_CHARGE: usize = 512;
pub(crate) const BLOCK: usize = 64 * 1024;
// Keep copy-up reads by the daemon from changing the overlay's saved atime.
#[cfg(target_os = "linux")]
pub(crate) const COPY_UP_OPEN_FLAGS: i32 = libc::O_NOATIME;
#[cfg(not(target_os = "linux"))]
pub(crate) const COPY_UP_OPEN_FLAGS: i32 = 0;
// A handful of whole blobs has a modest memory cost in the source trees
// this cache targets. Keep their verified buffers available for rereads.
// See the measured blob sizes in docs/resources.md.
const HOT_BLOBS: usize = 8;
const ACTIVE_DECODES: usize = 8;
const COMPLETION_DECODES: usize = ACTIVE_DECODES + 1;
const COPY_BUFFER: usize = 16 * BLOCK;
#[cfg(test)]
std::thread_local! {
    static BEFORE_COPY: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::Cell::new(None)
    };
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

#[derive(Debug)]
pub(crate) struct BackingCleanupError(io::Error);
impl std::fmt::Display for BackingCleanupError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "remove incomplete private backing: {}", self.0)
    }
}

#[derive(Default)]
pub(crate) struct Transfer {
    state: AtomicU8,
    served: AtomicBool,
    deferred_failure: Mutex<Option<DeferredTransferFailure>>,
}

struct DeferredTransferFailure {
    message: Arc<str>,
    reported: Arc<OnceLock<Arc<str>>>,
}

impl DeferredTransferFailure {
    fn report(self) {
        let _ = self.reported.set(self.message);
    }
}

impl Transfer {
    const VERIFIED: u8 = 1;
    const FAILED: u8 = 2;

    pub(crate) fn mark_verified(&self) {
        self.state.store(Self::VERIFIED, Ordering::Relaxed);
    }

    pub(crate) fn mark_served(&self) {
        if !self.served() {
            let mut deferred = self.deferred_failure.lock().unwrap();
            if let Some(failure) = deferred.take() {
                failure.report();
            }
            self.served.store(true, Ordering::Release);
        }
    }

    pub(crate) fn report_failure_on_use(
        &self,
        message: Arc<str>,
        reported: Arc<OnceLock<Arc<str>>>,
    ) {
        let mut deferred = self.deferred_failure.lock().unwrap();
        let failure = DeferredTransferFailure { message, reported };
        // Serialize registration with the first reader, without taking the
        // demand queue lock from inside the raw object store.
        if self.served() {
            failure.report();
        } else {
            *deferred = Some(failure);
        }
    }

    pub(crate) fn served(&self) -> bool {
        self.served.load(Ordering::Acquire)
    }

    fn verified(&self) -> bool {
        self.state.load(Ordering::Relaxed) == Self::VERIFIED
    }

    fn failed(&self) -> bool {
        self.state.load(Ordering::Relaxed) == Self::FAILED
    }

    fn check(source: Option<&Arc<Self>>) -> Result<()> {
        if source.is_some_and(|source| source.failed()) {
            return Err(crate::object::OperationCancelled.into());
        }
        Ok(())
    }
}

struct Descriptor {
    size: u64,
    blocks: Box<[[u8; 32]]>,
    file: bool,
    used: bool,
    claimed: bool,
    content_checked: bool,
    directory: Option<Arc<Directory>>,
    transfer: Option<Arc<Transfer>>,
}
impl Descriptor {
    fn mark_served(&self) {
        if !self.used && !self.claimed {
            if let Some(transfer) = &self.transfer {
                transfer.mark_served();
            }
        }
    }

    fn kind(&self) -> ObjectType {
        if self.directory.is_some() {
            ObjectType::Tree
        } else {
            ObjectType::Blob
        }
    }

    fn available(&self) -> bool {
        self.used || self.claimed || self.transfer.as_ref().is_none_or(|source| !source.failed())
    }

    fn belongs_to(&self, transfer: &Arc<Transfer>) -> bool {
        self.transfer
            .as_ref()
            .is_some_and(|source| Arc::ptr_eq(source, transfer))
    }
}
struct Resident {
    block: usize,
    bytes: BlobBytes,
}

pub(crate) struct OdbBlob(git2::OdbObject<'static>);

impl OdbBlob {
    fn new(object: git2::OdbObject<'_>) -> Self {
        // SAFETY: libgit2's git_odb_read returns an owning reference. The
        // git_odb_object contains the buffer and a reference count, with no
        // pointer to the ODB. git_odb_object_data and git_odb_object_free
        // operate on that allocation even after the originating ODB drops.
        Self(unsafe {
            std::mem::transmute::<git2::OdbObject<'_>, git2::OdbObject<'static>>(object)
        })
    }
}

// SAFETY: this wrapper exposes only the immutable data. libgit2 releases the
// owning reference through an atomic decrement, independently of the ODB.
unsafe impl Send for OdbBlob {}
unsafe impl Sync for OdbBlob {}

#[derive(Clone)]
pub(crate) enum BlobBytes {
    Rust(Arc<Vec<u8>>),
    Odb(Arc<OdbBlob>),
}

impl BlobBytes {
    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::Rust(bytes) => bytes.capacity(),
            Self::Odb(object) => object.0.len(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn shares_allocation(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Rust(left), Self::Rust(right)) => Arc::ptr_eq(left, right),
            (Self::Odb(left), Self::Odb(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl From<Vec<u8>> for BlobBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Rust(Arc::new(bytes))
    }
}

impl std::ops::Deref for BlobBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Rust(bytes) => bytes,
            Self::Odb(object) => object.0.data(),
        }
    }
}

impl std::fmt::Debug for BlobBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BlobBytes")
            .field("len", &self.len())
            .finish()
    }
}
#[derive(Debug)]
pub(crate) enum ReadData {
    Owned(Vec<u8>),
    Shared(BlobBytes, Range<usize>),
    Blocks {
        ranges: Vec<CheckedRange>,
        len: usize,
    },
}

#[derive(Debug)]
pub(crate) struct CheckedRange {
    bytes: BlobBytes,
    range: Range<usize>,
}

impl CheckedRange {
    fn slice(&self) -> &[u8] {
        &self.bytes[self.range.clone()]
    }
}

impl ReadData {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.len(),
            Self::Shared(_, range) => range.len(),
            Self::Blocks { len, .. } => *len,
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        let (one, ranges): (_, &[CheckedRange]) = match self {
            Self::Owned(bytes) => (Some(bytes.as_slice()), &[]),
            Self::Shared(bytes, range) => (Some(&bytes[range.clone()]), &[]),
            Self::Blocks { ranges, .. } => (None, ranges),
        };
        one.into_iter()
            .chain(ranges.iter().map(CheckedRange::slice))
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn chunk_count(&self) -> usize {
        match self {
            Self::Blocks { ranges, .. } => ranges.len(),
            _ => 1,
        }
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn shared_allocations(&self) -> impl Iterator<Item = &BlobBytes> {
        let (one, ranges): (_, &[CheckedRange]) = match self {
            Self::Owned(_) => (None, &[]),
            Self::Shared(bytes, _) => (Some(bytes), &[]),
            Self::Blocks { ranges, .. } => (None, ranges),
        };
        one.into_iter().chain(ranges.iter().map(|part| &part.bytes))
    }
    pub(crate) fn contiguous(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Owned(bytes) => Cow::Borrowed(bytes),
            Self::Shared(bytes, range) => Cow::Borrowed(&bytes[range.clone()]),
            Self::Blocks { .. } => Cow::Owned(self.copy_chunks()),
        }
    }
    fn copy_chunks(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.len());
        for chunk in self.chunks() {
            data.extend_from_slice(chunk);
        }
        data
    }
    pub(crate) fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(BlobBytes::Rust(bytes), range)
                if range.start == 0 && range.end == bytes.len() =>
            {
                Arc::unwrap_or_clone(bytes)
            }
            Self::Shared(bytes, range) => bytes[range].to_vec(),
            Self::Blocks { .. } => self.copy_chunks(),
        }
    }
}
#[cfg(test)]
impl<T: AsRef<[u8]>> PartialEq<T> for ReadData {
    fn eq(&self, other: &T) -> bool {
        self.len() == other.as_ref().len() && self.chunks().flatten().eq(other.as_ref())
    }
}
#[derive(Clone)]
pub(crate) struct RawBlob {
    state: Arc<Mutex<Store>>,
    oid: Oid,
    size: u64,
    kind: ObjectType,
}

enum CopySource {
    Resident(BlobBytes),
    Spilled(Arc<RawReader>),
    Empty,
}

impl RawBlob {
    pub(crate) fn kind(&self) -> ObjectType {
        self.kind
    }
    pub(crate) fn len(&self) -> u64 {
        self.size
    }
    pub(crate) fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.read_range(offset, size, false).map(ReadData::into_vec)
    }
    pub(crate) fn read_shared(&self, offset: u64, size: u32) -> Result<ReadData> {
        self.read_range(offset, size, cfg!(target_os = "macos"))
    }
    fn read_range(&self, offset: u64, size: u32, scatter: bool) -> Result<ReadData> {
        self.range(
            offset,
            self.size.saturating_sub(offset).min(u64::from(size)),
            scatter,
        )
    }
    pub(crate) fn read_all(&self) -> Result<ReadData> {
        self.range(0, self.size, false)
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
        check(&bytes.contiguous())?;
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
    fn range(&self, offset: u64, length: u64, scatter: bool) -> Result<ReadData> {
        let result = self.checked_range(offset, length, scatter);
        if let Err(error) = &result {
            // Reads can outlive the Snapshot lock through an active reader.
            // Preserve storage failures for the remote runner's final status,
            // including after a caller catches EIO or all handles close.
            self.record_failure(error);
        }
        result
    }
    fn record_failure(&self, error: &anyhow::Error) {
        if !crate::object::is_cancelled(error) {
            let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
            store.failure.get_or_insert_with(|| format!("{error:#}"));
        }
    }
    fn checked_range(&self, offset: u64, length: u64, scatter: bool) -> Result<ReadData> {
        if length == 0 {
            return Ok(ReadData::Owned(Vec::new()));
        }
        anyhow::ensure!(self.kind == ObjectType::Blob, "tree bytes are not retained");
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let end = offset.checked_add(length).context("raw range end")?;
        if let Some(bytes) = store.hot_payload(self.oid) {
            crate::object::check_transfer()?;
            return Ok(ReadData::Shared(
                bytes.clone(),
                usize::try_from(offset)?..usize::try_from(end)?,
            ));
        }
        let first = usize::try_from(offset / BLOCK as u64)?;
        let last = usize::try_from((end - 1) / BLOCK as u64)?;
        let len = usize::try_from(length)?;
        let mut contiguous = Vec::new();
        let mut ranges = Vec::new();
        if first != last {
            if scatter {
                ranges.reserve_exact(last - first + 1);
            } else {
                contiguous.reserve_exact(len);
            }
        }
        let mut file = None;
        for block in first..=last {
            crate::object::check_transfer()?;
            let start = block as u64 * BLOCK as u64;
            let from = usize::try_from(offset.saturating_sub(start))?;
            let to = usize::try_from((end - start).min(BLOCK as u64))?;
            let bytes = if store
                .memory
                .peek(&self.oid)
                .is_some_and(|resident| resident.block == block)
            {
                store.memory.get(&self.oid).unwrap().bytes.clone()
            } else {
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
                // The final block can stay in the cache. Other full blocks
                // can be read and authenticated directly in a contiguous reply.
                let direct = !scatter && block < last && from == 0 && to == size;
                let mut bytes = if direct { Vec::new() } else { vec![0; size] };
                let output = if direct {
                    let output_start = contiguous.len();
                    contiguous.resize(output_start + size, 0);
                    &mut contiguous[output_start..]
                } else {
                    &mut bytes
                };
                read_checked_block(&file.as_ref().unwrap().file, start, output, &expected)?;
                #[cfg(test)]
                {
                    store.authenticated_bytes += size as u64;
                }
                if direct {
                    continue;
                }
                let bytes = BlobBytes::from(bytes);
                if block == last {
                    // The last block is the only one that can stay resident.
                    // Earlier misses must leave a later cache hit available.
                    if let Err(error) = store.admit(self.oid, block, bytes.clone()) {
                        // Optional admission can spill another resident object.
                        // A failed write preserves it and the checked bytes.
                        if error.downcast_ref::<io::Error>().is_none() {
                            return Err(error);
                        }
                    }
                }
                bytes
            };
            if first == last {
                return Ok(ReadData::Shared(bytes, from..to));
            }
            if scatter {
                ranges.push(CheckedRange {
                    bytes,
                    range: from..to,
                });
            } else {
                // Consumers that require one allocation release each scratch
                // block before reading the next, even for whole-object reads.
                contiguous.extend_from_slice(&bytes[from..to]);
            }
        }
        Ok(if scatter {
            ReadData::Blocks { ranges, len }
        } else {
            ReadData::Owned(contiguous)
        })
    }
    pub(crate) fn copy_up(&self, path: &Path, retain: u64) -> Result<File> {
        let result = self.copy_to(path, retain);
        if let Err(error) = &result {
            if error.downcast_ref::<BackingError>().is_none() {
                self.record_failure(error);
            }
        }
        result
    }
    fn copy_source(&self, retain: u64) -> Result<CopySource> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let object = store
            .objects
            .get(&self.oid)
            .context("missing prepared source")?;
        anyhow::ensure!(
            object.kind() == ObjectType::Blob,
            "mutable source is not a blob"
        );

        if object.file {
            let input = store
                .directory
                .reader(self.oid, self.size)
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
                })?;
            return Ok(CopySource::Spilled(input));
        }
        if retain == 0 {
            return Ok(CopySource::Empty);
        }
        let bytes = match store.hot_payload(self.oid) {
            Some(bytes) => bytes,
            None => {
                &store
                    .memory
                    .get(&self.oid)
                    .context("missing resident source")?
                    .bytes
            }
        };
        Ok(CopySource::Resident(bytes.clone()))
    }

    fn copy_to(&self, path: &Path, retain: u64) -> Result<File> {
        let parent = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.parent().context("backing parent")?)
            .map_err(BackingError)?;
        let name = CString::new(path.file_name().context("backing name")?.as_bytes())?;
        // A copy owns its input across eviction so backing I/O does not block
        // other reads, pack publication, or spill completion.
        let source = self.copy_source(retain)?;
        // A writer can change any byte in the private copy, including retained
        // bytes. Keep block authentication on immutable reads so copy-up can
        // clone the source without reading its contents.
        // A short retained prefix costs at most one cache block to copy.
        // Cloning the whole source and discarding most of it does more work.
        let clone_source = match &source {
            CopySource::Spilled(input) if retain == self.size || retain > BLOCK as u64 => {
                Some(&input.file)
            }
            _ => None,
        };
        #[cfg(test)]
        if let Some(before_copy) = BEFORE_COPY.take() {
            before_copy();
        }
        let (mut output, cloned_size) = open_copy(&parent, &name, clone_source)?;
        let copied = (|| -> Result<()> {
            match source {
                CopySource::Spilled(input) => {
                    if let Some(size) = cloned_size {
                        RawDirectory::validate_size(size, self.size)?;
                        if retain < self.size {
                            output.set_len(retain).map_err(BackingError)?;
                        }
                    } else {
                        // Only the sequential copy uses the cursor. Immutable
                        // readers use positional reads on the same descriptor.
                        let _cursor = input.copy_cursor.lock().unwrap_or_else(|e| e.into_inner());
                        copy_spilled(&input.file, &mut output, retain)?;
                    }
                }
                CopySource::Resident(bytes) => {
                    for chunk in bytes[..retain as usize].chunks(COPY_BUFFER) {
                        crate::object::check_transfer()?;
                        output.write_all(chunk).map_err(BackingError)?;
                    }
                }
                CopySource::Empty => (),
            }
            Ok(())
        })();
        if let Err(error) = copied {
            drop(output);
            return Err(discard_copy(&parent, &name, error));
        }
        #[cfg(test)]
        if cloned_size.is_some() {
            self.state.lock().unwrap().cloned_copies += 1;
        }
        Ok(output)
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

fn copy_spilled(mut source: &File, output: &mut File, retain: u64) -> Result<()> {
    source.rewind()?;
    let mut remaining = retain;
    while remaining != 0 {
        crate::object::check_transfer()?;
        let limit = remaining.min(COPY_BUFFER as u64);
        let count = match io::copy(&mut source.take(limit), output) {
            Ok(count) => count,
            Err(_) => {
                // The fast copy reports one error for both descriptors.
                // Restart from the failed chunk to identify its source.
                return copy_spilled_from(source, output, retain - remaining, retain);
            }
        };
        if count != limit {
            return Err(invalid("verified raw blob length was modified"));
        }
        remaining -= count;
    }
    Ok(())
}

fn copy_spilled_from(source: &File, output: &mut File, mut offset: u64, retain: u64) -> Result<()> {
    crate::object::check_transfer()?;
    output.seek(SeekFrom::Start(offset)).map_err(BackingError)?;
    let mut bytes = vec![0; (retain - offset).min(COPY_BUFFER as u64) as usize];
    while offset < retain {
        crate::object::check_transfer()?;
        let count = (retain - offset).min(bytes.len() as u64) as usize;
        let chunk = &mut bytes[..count];
        match source.read_exact_at(chunk, offset) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(invalid("verified raw blob length was modified"));
            }
            Err(error) => return Err(error.into()),
        }
        output.write_all(chunk).map_err(BackingError)?;
        offset += count as u64;
    }
    Ok(())
}

fn read_checked_block(
    file: &File,
    offset: u64,
    bytes: &mut [u8],
    expected: &[u8; 32],
) -> Result<()> {
    file.read_exact_at(bytes, offset)?;
    check_block(bytes, expected)
}

fn check_block(bytes: &[u8], expected: &[u8; 32]) -> Result<()> {
    if blake3::hash(bytes).as_bytes() != expected {
        return Err(invalid("verified raw blob bytes were modified"));
    }
    Ok(())
}

fn discard_copy(parent: &File, name: &CStr, failure: anyhow::Error) -> anyhow::Error {
    match remove_private(parent, name) {
        Ok(()) => failure,
        Err(cleanup) => failure.context(BackingCleanupError(cleanup)),
    }
}

fn remove_private(parent: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

// Include the checked length when a clone replaces the ordinary copy.
fn open_copy(parent: &File, name: &CStr, source: Option<&File>) -> Result<(File, Option<u64>)> {
    #[cfg(target_os = "macos")]
    if let Some(source) = source {
        if unsafe { libc::fclonefileat(source.as_raw_fd(), parent.as_raw_fd(), name.as_ptr(), 0) }
            == 0
        {
            return match open_private_file(parent, name, libc::O_RDWR) {
                Ok((file, metadata)) => Ok((file, Some(metadata.len()))),
                Err(error) => Err(discard_copy(parent, name, BackingError(error).into())),
            };
        }
    }

    let file = open_private_descriptor(
        parent,
        name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | COPY_UP_OPEN_FLAGS,
    )
    .map_err(BackingError)?;
    #[cfg(target_os = "linux")]
    let cloned = source.is_some_and(|source| {
        // The exclusive open created this file. Check its metadata after
        // FICLONE so the same lookup can also see the cloned length.
        unsafe { libc::ioctl(file.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) == 0 }
    });
    #[cfg(not(target_os = "linux"))]
    let cloned = false;
    match private_file_metadata(&file) {
        Ok(metadata) => Ok((file, cloned.then_some(metadata.len()))),
        Err(error) => {
            drop(file);
            Err(discard_copy(parent, name, BackingError(error).into()))
        }
    }
}

struct RawReader {
    file: File,
    copy_cursor: Mutex<()>,
}

/// A descriptor for the owned directory anchors operations even after a path
/// rename. One last-read descriptor is shared by the store; blobs and child
/// handles do not each retain a descriptor. Every disk read rechecks metadata.
struct RawDirectory {
    fd: Arc<File>,
    reader: Option<(Oid, Arc<RawReader>)>,
    _owner: tempfile::TempDir,
    writes: Arc<WriteMetrics>,
    incomplete: Arc<Mutex<HashSet<Oid>>>,
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
            writes: Arc::default(),
            incomplete: Arc::default(),
            #[cfg(test)]
            open_calls: std::cell::Cell::new(0),
            #[cfg(test)]
            spill_error: None,
        })
    }
    fn validate_size(length: u64, expected: u64) -> Result<()> {
        anyhow::ensure!(length == expected, "verified raw blob length was modified");
        Ok(())
    }
    fn open(&self, oid: Oid, write: bool, size: u64) -> Result<File> {
        #[cfg(test)]
        self.open_calls.set(self.open_calls.get() + 1);
        let (file, metadata) = open_private_file(
            &self.fd,
            &CString::new(oid.to_string())?,
            if write {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            },
        )
        .context("open private raw object")?;
        Self::validate_size(metadata.len(), size)?;
        Ok(file)
    }
    fn reader(&mut self, oid: Oid, size: u64) -> Result<Arc<RawReader>> {
        if let Some((cached, file)) = &self.reader {
            if *cached == oid {
                let metadata =
                    private_file_metadata(&file.file).context("validate private raw object")?;
                Self::validate_size(metadata.len(), size)?;
                return Ok(file.clone());
            }
        }
        // Retain one descriptor independent of object count, payload budgets
        // and open child handles. A different object replaces this one slot.
        // Release the old slot first: it may be the last available descriptor
        // under the caller's limit, so opening before replacement can fail.
        self.reader = None;
        let file = Arc::new(RawReader {
            file: self.open(oid, false, size)?,
            copy_cursor: Mutex::new(()),
        });
        self.reader = Some((oid, file.clone()));
        Ok(file)
    }
    fn begin_spill(&self, oid: Oid) -> Result<Pending> {
        #[cfg(test)]
        if let Some(error) = self.spill_error {
            return Err(io::Error::from_raw_os_error(error).into());
        }
        // Keep the descriptor resident until the full write succeeds.
        let name = CString::new(oid.to_string())?;
        let mut incomplete = self.incomplete.lock().unwrap_or_else(|e| e.into_inner());
        if incomplete.contains(&oid) {
            remove_private(&self.fd, &name)?;
            incomplete.remove(&oid);
        }
        #[cfg(test)]
        self.open_calls.set(self.open_calls.get() + 1);
        let file =
            open_private_descriptor(&self.fd, &name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)
                .context("open private raw object")?;
        drop(incomplete);
        // The exclusive name already exists. Let Pending own it before fstat
        // so an error during validation cannot block a later spill.
        let pending = Pending {
            directory: self.fd.clone(),
            incomplete: self.incomplete.clone(),
            oid,
            name,
            file,
            committed: false,
        };
        private_file_metadata(&pending.file).context("validate private raw object")?;
        Ok(pending)
    }

    fn spill(&self, oid: Oid, bytes: &[u8]) -> Result<Box<[[u8; 32]]>> {
        self.begin_spill(oid)?.write_verified(bytes, &self.writes)
    }
}

#[cfg(test)]
type SpillResultObserver = Box<dyn FnMut(bool) + Send>;

#[derive(Default)]
struct WriteMetrics {
    calls: AtomicU64,
    bytes: AtomicU64,
    #[cfg(test)]
    on_spill: Mutex<Option<Box<dyn Fn() + Send>>>,
    #[cfg(test)]
    on_spill_result: Mutex<Option<SpillResultObserver>>,
    #[cfg(test)]
    fail_write_after: Mutex<Option<usize>>,
    #[cfg(test)]
    cancel_after_bytes: AtomicU64,
}

struct RawWriter<'a> {
    file: &'a mut File,
    totals: &'a WriteMetrics,
}
impl Write for RawWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.totals.calls.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        let mut allowance = self.totals.fail_write_after.lock().unwrap();
        #[cfg(test)]
        let bytes = match *allowance {
            Some(0) => {
                *allowance = None;
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Some(remaining) => &bytes[..bytes.len().min(remaining)],
            None => bytes,
        };
        let count = self.file.write(bytes)?;
        #[cfg(test)]
        if let Some(remaining) = allowance.as_mut() {
            *remaining -= count;
        }
        self.totals.bytes.fetch_add(count as u64, Ordering::Relaxed);
        #[cfg(test)]
        {
            let cancel_at = self.totals.cancel_after_bytes.load(Ordering::Relaxed);
            if cancel_at != 0 && self.totals.bytes.load(Ordering::Relaxed) >= cancel_at {
                crate::git::stop_operations();
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
    incomplete: Arc<Mutex<HashSet<Oid>>>,
    oid: Oid,
    name: CString,
    file: File,
    committed: bool,
}
impl Pending {
    fn write_verified(mut self, bytes: &[u8], totals: &WriteMetrics) -> Result<Box<[[u8; 32]]>> {
        #[cfg(test)]
        if let Some(pause) = totals.on_spill.lock().unwrap().as_ref() {
            pause();
        }
        let mut digests = Vec::with_capacity(bytes.len().div_ceil(BLOCK));
        let mut output = RawWriter {
            file: &mut self.file,
            totals,
        };
        // Start writing before the complete blob has been hashed. Each chunk
        // covers whole digest blocks, with a partial block only at the end.
        let result = bytes
            .chunks(COPY_BUFFER)
            .try_for_each(|mut chunk| -> Result<()> {
                for block in chunk.chunks(BLOCK) {
                    crate::object::check_transfer()?;
                    digests.push(*blake3::hash(block).as_bytes());
                }
                // A short write leaves part of this chunk to retry.
                while !chunk.is_empty() {
                    crate::object::check_transfer()?;
                    match output.write(chunk) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "failed to write whole buffer",
                            )
                            .into());
                        }
                        Ok(written) => chunk = &chunk[written..],
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(())
            });
        #[cfg(test)]
        if let Some(observe) = totals.on_spill_result.lock().unwrap().as_mut() {
            observe(result.is_ok());
        }
        result?;
        self.committed = true;
        Ok(digests.into_boxed_slice())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.committed && remove_private(&self.directory, &self.name).is_err() {
            self.incomplete
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(self.oid);
        }
    }
}

struct HotSpill {
    oid: Oid,
    bytes: BlobBytes,
    output: Pending,
    writes: Arc<WriteMetrics>,
    result: mpsc::Sender<Result<()>>,
}

#[derive(Default)]
struct HotWriter {
    jobs: Option<mpsc::Sender<HotSpill>>,
    worker: Option<thread::JoinHandle<()>>,
    pending: Option<mpsc::Receiver<Result<()>>>,
}

impl HotWriter {
    // A stopped worker is distinct from a write failure that can be retried.
    fn wait(&mut self) -> Result<Result<()>> {
        match self.pending.take() {
            Some(pending) => pending.recv().context("verified blob spill worker stopped"),
            None => Ok(Ok(())),
        }
    }

    fn submit(
        &mut self,
        state: &Arc<Mutex<Store>>,
        store: &Store,
        oid: Oid,
        bytes: BlobBytes,
    ) -> Result<()> {
        // The command can finish while an earlier spill is being awaited.
        // Check cancellation before opening the file for each new write.
        crate::object::check_transfer()?;
        let output = store.directory.begin_spill(oid)?;
        if self.jobs.is_none() {
            let (jobs, queue) = mpsc::channel::<HotSpill>();
            let state = state.clone();
            self.worker = Some(
                thread::Builder::new()
                    .name("git-on-blob-spill".into())
                    .spawn(move || {
                        while let Ok(job) = queue.recv() {
                            let result =
                                job.output
                                    .write_verified(&job.bytes, &job.writes)
                                    .map(|blocks| {
                                        let mut store =
                                            state.lock().unwrap_or_else(|e| e.into_inner());
                                        store.finish_spill(job.oid, blocks);
                                        store.spilling = None;
                                    });
                            let _ = job.result.send(result);
                        }
                    })?,
            );
            self.jobs = Some(jobs);
        }
        let (result, pending) = mpsc::channel();
        self.jobs
            .as_ref()
            .unwrap()
            .send(HotSpill {
                oid,
                bytes,
                output,
                writes: store.directory.writes.clone(),
                result,
            })
            .map_err(|_| anyhow::anyhow!("verified blob spill worker stopped"))?;
        self.pending = Some(pending);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.jobs.take();
        self.pending.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("verified blob spill worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for HotWriter {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone)]
pub(crate) struct RawBlobs {
    state: Arc<Mutex<Store>>,
    prepared: Arc<Condvar>,
    hot_writer: Arc<Mutex<HotWriter>>,
    pack_decodes: Arc<PackDecodes>,
}

#[derive(Default)]
struct PackDecodes {
    active: Mutex<usize>,
    ready: Condvar,
}

pub(crate) struct PackDecode(Arc<PackDecodes>);

pub(crate) enum PackDecodePriority {
    Speculative,
    Demand,
    Completion,
}

#[derive(PartialEq, Eq)]
pub(crate) enum DemandLookup {
    Missing,
    Available,
    ClaimedForecast,
}

#[derive(Clone)]
pub(crate) enum ReceivedObject {
    Verified {
        object: crate::pack_stream::VerifiedObject,
        transfer: Option<Arc<Transfer>>,
    },
    Packed(Arc<tempfile::TempDir>),
}

impl Drop for PackDecode {
    fn drop(&mut self) {
        let mut active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
        *active -= 1;
        self.0.ready.notify_all();
    }
}

struct BlobPreparation<'a> {
    blobs: &'a RawBlobs,
    oid: Oid,
}

impl Drop for BlobPreparation<'_> {
    fn drop(&mut self) {
        let mut store = self.blobs.state.lock().unwrap_or_else(|e| e.into_inner());
        store.preparing.remove(&self.oid);
        store.forecasts.remove(&self.oid);
        self.blobs.prepared.notify_all();
    }
}

impl RawBlobs {
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        Ok(Self {
            state: Arc::new(Mutex::new(Store {
                directory: RawDirectory::new(directory)?,
                objects: HashMap::new(),
                preparing: HashSet::new(),
                received: HashMap::new(),
                memory: lru::LruCache::unbounded(),
                hot: lru::LruCache::unbounded(),
                spilling: None,
                decoded: HashMap::new(),
                forecasts: HashMap::new(),
                hot_limit: HOT_BLOBS,
                memory_bytes: 0,
                memory_limit: MEMORY_BUDGET,
                memory_high_water: 0,
                failure: None,
                blob_entries: 0,
                blob_bytes: 0,
                disk_entries: 0,
                disk_bytes: 0,
                #[cfg(test)]
                authenticated_bytes: 0,
                #[cfg(test)]
                cloned_copies: 0,
            })),
            prepared: Arc::default(),
            hot_writer: Arc::default(),
            pack_decodes: Arc::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn active_pack_decodes(&self) -> usize {
        *self.pack_decodes.active.lock().unwrap()
    }

    pub(crate) fn reserve_pack_decode(
        &self,
        priority: impl Fn() -> PackDecodePriority,
        check: impl Fn() -> Result<()>,
    ) -> Result<PackDecode> {
        let slots = &self.pack_decodes;
        let mut active = slots.active.lock().unwrap_or_else(|e| e.into_inner());
        // Apply backpressure before a complete buffer is allocated. Check
        // cancellation while waiting so an interrupted fetch can unwind.
        loop {
            check()?;
            if *active < ACTIVE_DECODES - 1 {
                break;
            }
            // Leave a buffer for a real read while the command runs. After
            // it exits, a used pack must also be able to finish validation
            // while unrelated transfers hold the ordinary decode permits.
            if *active <= ACTIVE_DECODES {
                let limit = match priority() {
                    PackDecodePriority::Speculative => ACTIVE_DECODES - 1,
                    PackDecodePriority::Demand => ACTIVE_DECODES,
                    PackDecodePriority::Completion => COMPLETION_DECODES,
                };
                if *active < limit {
                    break;
                }
            }
            (active, _) = slots
                .ready
                .wait_timeout(active, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        Ok(PackDecode(slots.clone()))
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
        Ok(state
            .objects
            .get(&oid)
            .filter(|object| object.available())
            .map(|object| self.blob(oid, object)))
    }
    pub(crate) fn claim(&self, oid: Oid) -> Result<bool> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !store.claim(oid) {
            return Ok(false);
        }
        let Some(payload) = store
            .decoded
            .get(&oid)
            .filter(|_| !store.preparing.contains(&oid) && store.failure.is_none())
            .cloned()
        else {
            return Ok(true);
        };
        // A decoder left verified bytes available after a placement error.
        // Only one reader retries admission, while others can keep using RAM.
        store.preparing.insert(oid);
        let _preparation = BlobPreparation { blobs: self, oid };
        if let Err(error) = self.admit_blob(store, oid, false, payload, None) {
            crate::object::check_transfer()?;
            if !error.is::<io::Error>() {
                return Err(error);
            }
        }
        Ok(true)
    }
    pub(crate) fn available(&self, oid: Oid) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.available(oid)
    }

    pub(crate) fn needs_pack_proof(&self, oid: Oid) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.objects.get(&oid).is_none_or(|object| {
            !object.used
                && !object.claimed
                && object
                    .transfer
                    .as_ref()
                    .is_some_and(|source| !source.verified())
        })
    }

    pub(crate) fn remember_verified(
        &self,
        extras: impl IntoIterator<Item = crate::pack_stream::VerifiedObject>,
        source: Option<&Arc<Transfer>>,
    ) {
        let mut extras = extras.into_iter();
        let Some(first) = extras.next() else { return };
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for extra in std::iter::once(first).chain(extras) {
            let oid = extra.oid();
            if let Some(object) = store.objects.get_mut(&oid) {
                if !object.used
                    && !object.claimed
                    && object
                        .transfer
                        .as_ref()
                        .is_some_and(|previous| source.is_none() || !previous.verified())
                {
                    // Keep the already checked bytes and bind a future read
                    // to the pack that has finished authenticating them.
                    object.transfer = source.cloned();
                }
            } else {
                use std::collections::hash_map::Entry;
                match store.received.entry(oid) {
                    Entry::Occupied(mut entry) => match entry.get_mut() {
                        ReceivedObject::Verified { transfer, .. } => {
                            if source.is_none() {
                                *transfer = None;
                            }
                        }
                        packed @ ReceivedObject::Packed(_) => {
                            *packed = ReceivedObject::Verified {
                                object: extra,
                                transfer: source.cloned(),
                            };
                        }
                    },
                    Entry::Vacant(entry) => {
                        entry.insert(ReceivedObject::Verified {
                            object: extra,
                            transfer: source.cloned(),
                        });
                    }
                }
            }
        }
    }

    pub(crate) fn has_received(&self, oid: Oid) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .received
            .contains_key(&oid)
    }

    pub(crate) fn received(&self, oid: Oid) -> Option<ReceivedObject> {
        // Keep the selected source alive if another transfer publishes the
        // same OID before its consumer begins decoding.
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .received
            .get(&oid)
            .cloned()
    }

    pub(crate) fn was_served(&self, oid: Oid, transfer: &Arc<Transfer>, delivered: bool) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .get(&oid)
            .is_some_and(|object| {
                (object.used || object.claimed || delivered) && object.belongs_to(transfer)
            })
    }

    /// Stop speculative publication and decide whether a consumer already
    /// depended on this pack under the same lock used to claim its objects.
    pub(crate) fn fail_forecast(
        &self,
        transfer: &Arc<Transfer>,
        deliveries: impl Iterator<Item = (Oid, bool)>,
    ) -> bool {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        transfer.state.store(Transfer::FAILED, Ordering::Relaxed);
        let mut required = false;
        for (oid, delivered) in deliveries {
            if let Some(object) = store
                .objects
                .get_mut(&oid)
                .filter(|object| object.belongs_to(transfer))
            {
                if object.used || object.claimed || delivered {
                    object.claimed = true;
                    required = true;
                }
            }
        }
        required
    }

    pub(crate) fn remember_packed(&self, oid: Oid, directory: &Arc<tempfile::TempDir>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.objects.get(&oid).is_some_and(Descriptor::available) {
            state
                .received
                .entry(oid)
                .or_insert_with(|| ReceivedObject::Packed(directory.clone()));
        }
    }

    pub(crate) fn forget_received(&self, oid: Oid) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .received
            .remove(&oid);
    }

    pub(crate) fn for_read(&mut self, oid: Oid) -> Result<Option<(RawBlob, bool)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(object) = state
            .objects
            .get_mut(&oid)
            .filter(|object| object.available())
        else {
            return Ok(None);
        };
        anyhow::ensure!(
            object.kind() == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        let first = !object.used;
        object.mark_served();
        object.used = true;
        Ok(Some((self.blob(oid, object), first)))
    }
    pub(crate) fn directory(&self, oid: Oid) -> Option<(Arc<Directory>, bool)> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let object = store.objects.get_mut(&oid)?;
        let directory = object.directory.clone()?;
        let first = !object.used;
        object.mark_served();
        object.used = true;
        Some((directory, first))
    }
    pub(crate) fn insert(&mut self, oid: Oid, size: usize, input: impl Read) -> Result<RawBlob> {
        self.insert_blob(oid, size, true, None, || {
            // Lazy reads assume each blob fits in RAM. Verifying the whole
            // blob in memory lets even the first range read use that buffer
            // before any spill.
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(size)?;
            crate::object::copy_verified(input, &mut bytes, ObjectType::Blob, size, oid)?;
            Ok(BlobBytes::from(bytes))
        })
    }
    pub(crate) fn insert_prepared_blob(&mut self, oid: Oid, bytes: Vec<u8>) -> Result<RawBlob> {
        // The local native reader authenticated the bytes and checked the
        // command's exit before transferring the allocation to the cache.
        self.insert_blob(oid, bytes.len(), true, None, || Ok(BlobBytes::from(bytes)))
    }
    pub(crate) fn insert_odb_blob(
        &mut self,
        oid: Oid,
        object: git2::OdbObject<'_>,
    ) -> Result<RawBlob> {
        anyhow::ensure!(
            object.id() == oid && object.kind() == ObjectType::Blob,
            "ODB object identity/type changed before raw import"
        );
        self.insert_blob(oid, object.len(), true, None, || {
            crate::object::verify_odb_object(&object, ObjectType::Blob)?;
            Ok(BlobBytes::Odb(Arc::new(OdbBlob::new(object))))
        })
    }
    pub(crate) fn insert_fetched(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        transfer: Option<Arc<Transfer>>,
    ) -> Result<RawBlob> {
        self.insert_blob(oid, size, false, transfer, || {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(size)?;
            crate::object::copy_verified(input, &mut bytes, ObjectType::Blob, size, oid)?;
            Ok(BlobBytes::from(bytes))
        })
    }
    pub(crate) fn insert_verified_blob(
        &mut self,
        object: crate::pack_stream::VerifiedObject,
        transfer: Option<Arc<Transfer>>,
        speculative: bool,
        waiting: impl FnOnce() -> bool,
        ready: impl FnOnce(bool),
    ) -> Result<()> {
        anyhow::ensure!(
            object.kind() == ObjectType::Blob,
            "streamed object is not a blob"
        );
        let oid = object.oid();
        self.insert_verified_bytes(
            oid,
            object.into_data(),
            transfer,
            speculative,
            waiting,
            ready,
        )
    }

    fn insert_verified_bytes(
        &mut self,
        oid: Oid,
        bytes: impl Into<Arc<Vec<u8>>>,
        transfer: Option<Arc<Transfer>>,
        speculative: bool,
        waiting: impl FnOnce() -> bool,
        ready: impl FnOnce(bool),
    ) -> Result<()> {
        let payload = BlobBytes::Rust(bytes.into());
        let _preparation;
        let mut store = self.wait_for_preparation(oid);
        Transfer::check(transfer.as_ref())?;
        if let Some(object) = store.objects.get_mut(&oid) {
            if !object.available() {
                object.transfer = transfer;
            }
            drop(store);
            ready(true);
            return Ok(());
        }
        if payload.len() <= BLOCK {
            if let Err(error) = self.admit_blob(store, oid, false, payload, transfer) {
                crate::object::check_transfer()?;
                return Err(error);
            }
            ready(false);
            return Ok(());
        }
        if let Some(error) = &store.failure {
            anyhow::bail!("verified object storage previously failed: {error}");
        }
        store.preparing.insert(oid);
        _preparation = BlobPreparation { blobs: self, oid };
        if let Some(transfer) = transfer.as_ref().filter(|_| speculative) {
            store
                .forecasts
                .insert(oid, (payload.clone(), transfer.clone()));
        }
        let mut ready = Some(ready);
        // Check demand after registering the buffer under the same store lock
        // used by a late reader to claim a completed forecast.
        if waiting() && store.can_publish_decoded(oid) {
            store.publish_decoded(oid, payload.clone(), transfer.clone());
            drop(store);
            ready.take().unwrap()(false);
            store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        }

        let admission = self.admit_blob(store, oid, false, payload, transfer);
        if let Err(error) = admission {
            let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
            store.forecasts.remove(&oid);
            // The verified data may already have served a read. Once the
            // command ends, a late cache placement failure is cancellation.
            crate::object::check_transfer()?;
            if !store.decoded.contains_key(&oid) || crate::object::is_cancelled(&error) {
                return Err(error);
            }
            if !error.is::<io::Error>() {
                store.failure.get_or_insert_with(|| format!("{error:#}"));
                return Err(error);
            }
            // Placement can recover while the decoder continues to validate
            // the pack. A later admission can reuse this verified buffer.
        }
        if let Some(ready) = ready {
            ready(false);
        }
        Ok(())
    }

    pub(crate) fn lookup_demand(&self, oid: Oid) -> DemandLookup {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if store.claim(oid) || store.received.contains_key(&oid) {
            return DemandLookup::Available;
        }
        let Some((payload, transfer)) = store.forecasts.get(&oid) else {
            return DemandLookup::Missing;
        };
        if transfer.failed() || !store.can_publish_decoded(oid) {
            return DemandLookup::Missing;
        }
        let payload = payload.clone();
        let transfer = transfer.clone();
        store.publish_decoded(oid, payload, Some(transfer));
        store.claim(oid);
        DemandLookup::ClaimedForecast
    }

    fn insert_blob(
        &mut self,
        oid: Oid,
        size: usize,
        used: bool,
        transfer: Option<Arc<Transfer>>,
        decode: impl FnOnce() -> Result<BlobBytes>,
    ) -> Result<RawBlob> {
        let _preparation;
        let mut store = self.wait_for_preparation(oid);
        if let Some(object) = store.objects.get(&oid).filter(|object| object.available()) {
            return Ok(self.blob(oid, object));
        }
        // A large decode can wait for Git. Reserve its OID without holding
        // the cache lock, so unrelated reads and publications can proceed.
        let bytes = if size > BLOCK {
            store.preparing.insert(oid);
            drop(store);
            _preparation = BlobPreparation { blobs: self, oid };
            let bytes = decode()?;
            store = self.state.lock().unwrap_or_else(|e| e.into_inner());
            bytes
        } else {
            decode()?
        };
        anyhow::ensure!(bytes.len() == size, "verified blob length changed");
        Transfer::check(transfer.as_ref())?;
        if let Some(object) = store.objects.get_mut(&oid) {
            if !object.available() {
                object.transfer = transfer;
            }
            return Ok(self.blob(oid, object));
        }
        let admission = self.admit_blob(store, oid, used, bytes, transfer);
        if admission.is_err() {
            crate::object::check_transfer()?;
        }
        admission
    }

    fn admit_blob<'a>(
        &'a self,
        mut store: MutexGuard<'a, Store>,
        oid: Oid,
        used: bool,
        payload: BlobBytes,
        transfer: Option<Arc<Transfer>>,
    ) -> Result<RawBlob> {
        let size = payload.len();
        let weight = payload
            .capacity()
            .checked_add(ENTRY_CHARGE)
            .context("resident allocation size")?;
        let resident = size != 0 && size <= BLOCK && weight <= store.memory_limit;
        let hot = size > BLOCK && store.hot_limit != 0;

        if resident {
            store.make_room(weight)?;
        }
        if hot {
            drop(store);
            let mut writer = self.hot_writer.lock().unwrap_or_else(|e| e.into_inner());
            store = self.complete_eviction(&mut writer)?;
            Transfer::check(transfer.as_ref())?;
            while let Some(deferred) = store
                .decoded
                .keys()
                .find(|oid| !store.preparing.contains(*oid))
                .copied()
            {
                self.evict_hot_if_full(&mut store, &mut writer)?;
                let bytes = store.decoded.remove(&deferred).expect("deferred blob");
                store.hot.put(deferred, bytes);
                drop(store);
                store = self.complete_eviction(&mut writer)?;
                Transfer::check(transfer.as_ref())?;
            }
            self.evict_hot_if_full(&mut store, &mut writer)?;
        }
        let file = size != 0 && !resident && !hot;
        let blocks = if file {
            store.directory.spill(oid, &payload)?
        } else {
            Box::new([])
        };
        if !store.objects.contains_key(&oid) {
            store.publish_blob(oid, size, used, transfer);
        }
        if resident {
            store.admit(oid, 0, payload)?;
        } else if hot {
            store.hot.put(oid, payload);
        }
        if file {
            store.finish_spill(oid, blocks);
        }
        store.decoded.remove(&oid);
        Ok(self.blob(oid, &store.objects[&oid]))
    }

    fn evict_hot_if_full(&self, store: &mut Store, writer: &mut HotWriter) -> Result<()> {
        if store.hot.len() == store.hot_limit {
            let (&victim, bytes) = store.hot.peek_lru().context("missing hot blob")?;
            writer.submit(&self.state, store, victim, bytes.clone())?;
            store.spilling = store.hot.pop_lru();
        }
        Ok(())
    }

    fn complete_eviction(&self, writer: &mut HotWriter) -> Result<MutexGuard<'_, Store>> {
        let previous_write = writer.wait()?;
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((oid, bytes)) = &store.spilling {
            // Keep the verified victim readable after a failed spill and
            // retry that write before allowing another eviction.
            writer.submit(&self.state, &store, *oid, bytes.clone())?;
            drop(store);
            writer.wait()??;
            store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        } else {
            previous_write?;
        }
        Ok(store)
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.hot_writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop()
    }

    fn wait_for_preparation(&self, oid: Oid) -> MutexGuard<'_, Store> {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.prepared
            .wait_while(store, |store| store.preparing.contains(&oid))
            .unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn insert_tree(&self, oid: Oid, size: usize, input: impl Read) -> Result<RawBlob> {
        self.cache_directory(oid, size, None, || Directory::verified(oid, size, input))
    }
    pub(crate) fn insert_prepared_tree(
        &self,
        oid: Oid,
        size: usize,
        directory: Directory,
    ) -> Result<RawBlob> {
        self.cache_directory(oid, size, None, || Ok(directory))
    }
    pub(crate) fn insert_verified_tree(
        &self,
        object: &crate::pack_stream::VerifiedObject,
        transfer: Option<Arc<Transfer>>,
    ) -> Result<RawBlob> {
        self.cache_directory(object.oid(), object.data().len(), transfer, || {
            Directory::from_verified(object)
        })
    }
    pub(crate) fn insert_odb_tree(
        &self,
        oid: Oid,
        object: &git2::OdbObject<'_>,
    ) -> Result<RawBlob> {
        self.cache_directory(oid, object.len(), None, || Directory::from_odb(oid, object))
    }
    fn cache_directory(
        &self,
        oid: Oid,
        size: usize,
        transfer: Option<Arc<Transfer>>,
        prepare: impl FnOnce() -> Result<Directory>,
    ) -> Result<RawBlob> {
        let mut store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(object) = store.objects.get(&oid) {
            return Ok(self.blob(oid, object));
        }
        let directory = Arc::new(prepare()?);
        store.objects.insert(
            oid,
            Descriptor {
                size: size as u64,
                blocks: Box::new([]),
                file: false,
                used: false,
                claimed: false,
                content_checked: false,
                directory: Some(directory),
                transfer,
            },
        );
        store.received.remove(&oid);
        Ok(self.blob(oid, store.objects.get(&oid).unwrap()))
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn oids(&self) -> Result<Vec<Oid>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(state
            .objects
            .keys()
            .chain(state.received.keys())
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
    pub(crate) fn hot_metrics(&self) -> (usize, usize, usize, usize) {
        let store = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            store.hot.iter().map(|(_, bytes)| bytes.capacity()).sum(),
            store.hot.len(),
            store
                .spilling
                .as_ref()
                .map_or(0, |(_, bytes)| bytes.capacity()),
            store.decoded.values().map(|bytes| bytes.capacity()).sum(),
        )
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
        self.wait_for_spill()?;
        let mut store = self.state.lock().unwrap();
        while let Some((&oid, bytes)) = store.hot.peek_lru() {
            let blocks = store.directory.spill(oid, bytes)?;
            store.finish_spill(oid, blocks);
            store.hot.pop(&oid);
        }
        store.make_room(MEMORY_BUDGET)
    }
    #[cfg(test)]
    fn wait_for_spill(&self) -> Result<()> {
        self.hot_writer.lock().unwrap().wait()?
    }
    #[cfg(test)]
    pub(crate) fn disk_only(directory: &Path) -> Result<Self> {
        let value = Self::new(directory)?;
        {
            let mut store = value.state.lock().unwrap();
            store.memory_limit = 0;
            store.hot_limit = 0;
        }
        Ok(value)
    }
}
struct Store {
    directory: RawDirectory,
    objects: HashMap<Oid, Descriptor>,
    preparing: HashSet<Oid>,
    received: HashMap<Oid, ReceivedObject>,
    memory: lru::LruCache<Oid, Resident>,
    hot: lru::LruCache<Oid, BlobBytes>,
    spilling: Option<(Oid, BlobBytes)>,
    decoded: HashMap<Oid, BlobBytes>,
    forecasts: HashMap<Oid, (BlobBytes, Arc<Transfer>)>,
    hot_limit: usize,
    memory_bytes: usize,
    memory_limit: usize,
    memory_high_water: usize,
    failure: Option<String>,
    blob_entries: u64,
    blob_bytes: u64,
    disk_entries: u64,
    disk_bytes: u64,
    #[cfg(test)]
    authenticated_bytes: u64,
    #[cfg(test)]
    cloned_copies: u64,
}
impl Store {
    fn available(&self, oid: Oid) -> bool {
        self.objects.get(&oid).is_some_and(Descriptor::available)
            || self.received.contains_key(&oid)
    }

    fn claim(&mut self, oid: Oid) -> bool {
        if let Some(object) = self
            .objects
            .get_mut(&oid)
            .filter(|object| object.available())
        {
            object.mark_served();
            object.claimed = true;
            return true;
        }
        false
    }

    fn can_publish_decoded(&self, oid: Oid) -> bool {
        // An early publication can outlive its decoder after a failed spill.
        self.hot_limit != 0
            && self.decoded.len() < COMPLETION_DECODES
            && self.failure.is_none()
            && !self.objects.contains_key(&oid)
    }

    fn publish_decoded(&mut self, oid: Oid, bytes: BlobBytes, transfer: Option<Arc<Transfer>>) {
        self.publish_blob(oid, bytes.len(), false, transfer);
        self.decoded.insert(oid, bytes);
    }

    fn hot_payload(&mut self, oid: Oid) -> Option<&BlobBytes> {
        self.hot
            .get(&oid)
            .or_else(|| self.decoded.get(&oid))
            .or_else(|| {
                self.spilling
                    .as_ref()
                    .filter(|(pending, _)| *pending == oid)
                    .map(|(_, bytes)| bytes)
            })
    }

    fn publish_blob(&mut self, oid: Oid, size: usize, used: bool, transfer: Option<Arc<Transfer>>) {
        let received = self.received.remove(&oid);
        let transfer = match &received {
            Some(ReceivedObject::Verified {
                transfer: proof, ..
            }) if transfer.as_ref().is_some_and(|source| !source.verified()) => proof.clone(),
            _ => transfer,
        };
        self.objects.insert(
            oid,
            Descriptor {
                size: size as u64,
                blocks: Box::new([]),
                file: false,
                used,
                claimed: false,
                content_checked: false,
                directory: None,
                transfer,
            },
        );
        self.blob_entries += 1;
        self.blob_bytes += size as u64;
    }

    fn finish_spill(&mut self, oid: Oid, blocks: Box<[[u8; 32]]>) {
        let object = self.objects.get_mut(&oid).expect("resident object");
        object.file = true;
        object.blocks = blocks;
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
                self.finish_spill(oid, blocks);
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
    fn admit(&mut self, oid: Oid, block: usize, bytes: BlobBytes) -> Result<()> {
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
pub(crate) fn under_descriptor_limit(name: &str) -> bool {
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
    fn copy_up_survives_descriptor_pressure() {
        if !under_descriptor_limit("raw_blob::tests::copy_up_survives_descriptor_pressure") {
            return;
        }
        for available in [2, 3] {
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
            let result = original.copy_up(&directory.path().join("mutable"), original.len());
            drop(held);
            match result {
                Ok(file) => {
                    let mut bytes = vec![0; data.len()];
                    file.read_exact_at(&mut bytes, 0).unwrap();
                    assert_eq!(bytes, data);
                }
                Err(error) => {
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
                    let file = original
                        .copy_up(&directory.path().join("retry"), original.len())
                        .unwrap();
                    let mut bytes = vec![0; data.len()];
                    file.read_exact_at(&mut bytes, 0).unwrap();
                    assert_eq!(bytes, data);
                }
            }
        }
    }

    #[test]
    fn copy_up_leaves_the_store_available_during_backing_io() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let data = vec![b'x'; BLOCK + 3];
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let original = store.insert(oid, data.len(), data.as_slice()).unwrap();

        for name in ["resident", "spilled"] {
            let mut cache = store.clone();
            let reader = original.clone();
            BEFORE_COPY.set(Some(Box::new(move || {
                assert!(
                    cache.state.try_lock().is_ok(),
                    "{name} copy holds the store lock"
                );
                if name == "resident" {
                    cache.evict_all_for_test().unwrap();
                }
                assert_eq!(reader.read(0, 1).unwrap(), b"x");
            })));
            let file = original
                .copy_up(&directory.path().join(name), original.len())
                .unwrap();
            let mut bytes = vec![0; data.len()];
            file.read_exact_at(&mut bytes, 0).unwrap();
            assert_eq!(bytes, data);
        }
    }

    #[test]
    fn a_spilled_copy_distinguishes_source_and_destination_io_failures() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("source");
        let destination_path = root.path().join("destination");
        let mut write_only = File::create(&source_path).unwrap();
        write_only.write_all(b"source").unwrap();
        let mut destination = File::create(&destination_path).unwrap();
        let error = copy_spilled(&write_only, &mut destination, 6).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(libc::EBADF)
        );

        let source = File::open(source_path).unwrap();
        let mut read_only = File::open(destination_path).unwrap();
        let error = copy_spilled(&source, &mut read_only, 6).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<BackingError>()
                .unwrap()
                .0
                .raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[test]
    fn copy_up_keeps_spilled_source_and_private_aliases() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let data = vec![b'x'; BLOCK + 3];
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let original = store.insert(oid, data.len(), data.as_slice()).unwrap();
        let source = store
            .state
            .lock()
            .unwrap()
            .directory
            .open(oid, false, data.len() as u64)
            .unwrap();
        let source_inode = source.metadata().unwrap().ino();
        let parent = File::open(directory.path()).unwrap();
        let (_, cloned_size) = open_copy(&parent, c"probe", Some(&source)).unwrap();
        let supports_clone = cloned_size.is_some();
        let first_path = directory.path().join("first");
        if supports_clone {
            crate::spool::METADATA_ERROR.set(Some((1, libc::EIO)));
            let error = original.copy_up(&first_path, original.len()).unwrap_err();
            assert!(error.downcast_ref::<BackingError>().is_some());
            assert_eq!(store.failure(), None);
            assert!(!first_path.exists());

            // Fail a third metadata lookup. The source and destination
            // have already been checked when the clone can be served.
            crate::spool::METADATA_ERROR.set(Some((2, libc::EIO)));
        }
        let first = original.copy_up(&first_path, original.len());
        let remaining_fault = crate::spool::METADATA_ERROR.take();
        let first = first.unwrap();
        if supports_clone {
            assert_eq!(remaining_fault, Some((0, libc::EIO)));
        }
        first.write_all_at(b"y", 0).unwrap();
        let retained = data.len() - 1;
        let second = original
            .copy_up(&directory.path().join("second"), retained as u64)
            .unwrap();
        let prefix = original
            .copy_up(&directory.path().join("prefix"), 1)
            .unwrap();
        assert_eq!(prefix.metadata().unwrap().len(), 1);
        let mut first_byte = [0];
        prefix.read_exact_at(&mut first_byte, 0).unwrap();
        assert_eq!(first_byte, [b'x']);
        assert_ne!(first.metadata().unwrap().ino(), source_inode);
        assert_ne!(
            second.metadata().unwrap().ino(),
            first.metadata().unwrap().ino()
        );
        assert_eq!(second.metadata().unwrap().len(), retained as u64);
        let mut bytes = vec![0; retained];
        second.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, data[..retained]);
        assert_eq!(original.read_all().unwrap(), data);
        assert_eq!(
            store.state.lock().unwrap().cloned_copies,
            2 * u64::from(supports_clone)
        );
        assert_eq!(store.disk_metrics(), (data.len() as u64, 1));
    }

    #[test]
    fn empty_resident_source_can_become_writable_without_a_payload_entry() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let oid = Oid::hash_object(ObjectType::Blob, b"").unwrap();
        let original = store.insert(oid, 0, b"".as_slice()).unwrap();
        let mut file = original
            .copy_up(&directory.path().join("empty"), original.len())
            .unwrap();
        file.write_all(b"new").unwrap();
        assert_eq!(file.metadata().unwrap().len(), 3);
        assert_eq!(original.len(), 0);
        assert_eq!(store.memory_metrics(), (0, 0));
    }

    #[test]
    fn copy_up_leaves_block_checks_with_the_immutable_source() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::disk_only(directory.path()).unwrap();
        let data = vec![b'x'; BLOCK + 3];
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let original = store.insert(oid, data.len(), data.as_slice()).unwrap();
        original.corrupt_for_test().unwrap();
        let destination = directory.path().join("mutable");
        let private = original.copy_up(&destination, original.len()).unwrap();
        private.write_all_at(b"y", 1).unwrap();
        let mut head = [0; 3];
        private.read_exact_at(&mut head, 0).unwrap();
        assert_eq!(&head, b"!yx");
        assert!(original.read(0, 1).is_err());
        assert!(store.failure().is_some());
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
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    limit.rlim_cur = 3;
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
        let directory = state.directory._owner.path().to_path_buf();
        let writes = state.directory.writes.clone();
        assert!(std::fs::read_dir(&directory).unwrap().next().is_none());
        assert!(!state.objects[&oid].file);
        drop(state);
        assert_eq!(store.disk_metrics(), (0, 0));
        assert_eq!(store.memory_metrics().1, 1);
        assert_eq!(reader.read_all().unwrap(), bytes);

        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let read_only = directory.clone();
            *writes.on_spill.lock().unwrap() = Some(Box::new(move || {
                std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o500))
                    .unwrap();
            }));
            assert!(store.evict_all_for_test().is_err());
            *writes.on_spill.lock().unwrap() = None;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                std::fs::metadata(directory.join(oid.to_string()))
                    .unwrap()
                    .len(),
                3
            );
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            unsafe {
                assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit), 0);
                limit.rlim_cur = limit.rlim_max;
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
            store.evict_all_for_test().unwrap();
            assert!(reader.on_disk());
            assert_eq!(reader.read_all().unwrap(), bytes);
        }
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
    fn resident_range_reads_share_verified_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        for size in [32, BLOCK + 1] {
            let data = vec![7; size];
            let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
            let blob = store.insert(oid, size, data.as_slice()).unwrap();
            let head = blob.read_shared(0, 16).unwrap();
            let next = blob.read_shared(4, 8).unwrap();
            let head = head.chunks().next().unwrap();
            let next = next.chunks().next().unwrap();
            assert_eq!(next, &data[4..12]);
            assert_eq!(head.as_ptr().wrapping_add(4), next.as_ptr());
        }
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
        store.state.lock().unwrap().memory_limit = BLOCK + ENTRY_CHARGE;
        let cached = reader.read_shared(BLOCK as u64, 1).unwrap();
        let range = reader.read_range((BLOCK - 1) as u64, 3, true).unwrap();
        let mut chunks = range.chunks();
        assert_eq!(chunks.next().unwrap(), &bytes[BLOCK - 1..BLOCK]);
        let second = chunks.next().unwrap();
        assert_eq!(second, &bytes[BLOCK..BLOCK + 2]);
        assert!(chunks.next().is_none());
        assert_eq!(cached.chunks().next().unwrap().as_ptr(), second.as_ptr());
        let contiguous = reader.read_range((BLOCK - 1) as u64, 3, false).unwrap();
        assert!(matches!(contiguous, ReadData::Owned(_)));
        assert_eq!(contiguous, &bytes[BLOCK - 1..BLOCK + 2]);
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
        file.write_all_at(&bytes[..BLOCK], 0).unwrap();
        let error = reader.read(0, bytes.len() as u32).unwrap_err();
        assert!(error
            .to_string()
            .contains("verified raw blob bytes were modified"));
        file.write_all_at(&bytes[BLOCK..], BLOCK as u64).unwrap();
        assert_eq!(reader.read(0, bytes.len() as u32).unwrap(), bytes);
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
        let writes = store.write_metrics();
        for data in [b"evil".as_slice(), b"n", b"next-extra"] {
            assert!(store.insert(next, 4, data).is_err());
            assert!(store.get(next).unwrap().is_none());
            assert_eq!(
                store.write_metrics(),
                writes,
                "unverified bytes reached disk"
            );
            assert_eq!(good.read_all().unwrap(), b"good");
            assert_eq!(
                stored_bytes(directory.path()),
                4,
                "failed staging files leaked"
            );
        }
        let large = vec![b'x'; BLOCK + 1];
        let expected = Oid::hash_object(ObjectType::Blob, &large).unwrap();
        let wrong = vec![b'y'; large.len()];
        assert!(store
            .insert(expected, wrong.len(), wrong.as_slice())
            .is_err());
        assert!(store.get(expected).unwrap().is_none());
        assert_eq!(
            store.write_metrics(),
            writes,
            "wrong large OID reached disk"
        );
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
        store
            .insert_fetched(oid, 4, b"good".as_slice(), None)
            .unwrap();
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
        let mut previous: Option<std::sync::Weak<RawReader>> = None;
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
        store.evict_all_for_test().unwrap();
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
        {
            let mut state = store.state.lock().unwrap();
            state.memory_limit = 512 * 1024;
            state.hot_limit = 0;
        }
        let mut hot = Vec::new();
        for n in 0..8u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&n.to_le_bytes());
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            hot.push(store.insert(oid, bytes.len(), bytes.as_slice()).unwrap());
        }
        let bytes = vec![b'z'; 10 * BLOCK];
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let large = store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
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
        assert_eq!(store.memory_metrics().1, 9);
        assert_eq!(store.write_metrics().1, bytes.len() as u64);
    }
    #[test]
    fn hot_blobs_spill_only_after_eviction_and_follow_recency() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let insert = |store: &mut RawBlobs, value| {
            let bytes = vec![value; BLOCK + 1];
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store.insert(oid, bytes.len(), bytes.as_slice()).unwrap()
        };
        let hot: Vec<_> = (0..HOT_BLOBS as u8)
            .map(|value| insert(&mut store, value))
            .collect();
        assert_eq!(hot[0].read(0, 1).unwrap(), [0]);
        assert_eq!(store.write_metrics(), (0, 0));
        assert_eq!(store.hot_metrics().1, HOT_BLOBS);
        insert(&mut store, HOT_BLOBS as u8);
        store.wait_for_spill().unwrap();
        assert!(!hot[0].on_disk());
        assert!(hot[1].on_disk());
        assert_eq!(store.disk_metrics(), ((BLOCK + 1) as u64, 1));
        assert_eq!(hot[1].read(BLOCK as u64, 1).unwrap(), [1]);
        assert_eq!(store.tested_authenticated_bytes(), 1);
        assert_eq!(store.hot_metrics().1, HOT_BLOBS);
    }
    #[test]
    fn unused_prefetch_does_not_poison_the_store_if_spill_fails() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        for value in 0..HOT_BLOBS as u8 {
            let bytes = vec![value; BLOCK + 1];
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
        }
        store.state.lock().unwrap().directory.spill_error = Some(libc::ENOSPC);
        let forecast = vec![HOT_BLOBS as u8; BLOCK + 1];
        let oid = Oid::hash_object(ObjectType::Blob, &forecast).unwrap();
        let mut notified = false;
        assert!(store
            .insert_verified_bytes(
                oid,
                forecast,
                Some(Arc::default()),
                true,
                || false,
                |_| notified = true
            )
            .is_err());
        assert_eq!(store.failure(), None);
        assert!(!store.available(oid));
        assert!(!notified);
    }

    #[test]
    fn forecasts_leave_a_decode_buffer_for_required_reads() {
        use crate::object::check_transfer;
        use std::sync::atomic::AtomicBool;

        let directory = tempfile::tempdir().unwrap();
        let cache = RawBlobs::new(directory.path()).unwrap();
        let busy: Vec<_> = (1..HOT_BLOBS)
            .map(|_| {
                cache
                    .reserve_pack_decode(|| PackDecodePriority::Speculative, check_transfer)
                    .unwrap()
            })
            .collect();
        let required = AtomicBool::new(false);
        let (forecast, pending) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let (foreground, demanded) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                started.send(()).unwrap();
                let permit = cache
                    .reserve_pack_decode(
                        || {
                            if required.load(Ordering::Relaxed) {
                                PackDecodePriority::Demand
                            } else {
                                PackDecodePriority::Speculative
                            }
                        },
                        check_transfer,
                    )
                    .unwrap();
                forecast.send(permit).unwrap();
            });
            let entered = running.recv_timeout(Duration::from_secs(3)).is_ok();
            if let Ok(permit) = pending.recv_timeout(Duration::from_millis(100)) {
                drop(permit);
                drop(busy);
                panic!("a forecast used the reserved foreground buffer");
            }
            scope.spawn(|| {
                let permit = cache
                    .reserve_pack_decode(|| PackDecodePriority::Demand, check_transfer)
                    .unwrap();
                foreground.send(permit).unwrap();
            });
            let independent = demanded.recv_timeout(Duration::from_secs(3));
            let served = independent.is_ok();
            drop(independent);
            required.store(true, Ordering::Relaxed);
            let promoted = pending.recv_timeout(Duration::from_secs(3)).is_ok();
            drop(busy);
            assert!(entered && served && promoted);
        });
    }

    #[test]
    fn spill_can_retry_after_validation_of_the_new_file_fails() {
        let directory = tempfile::tempdir().unwrap();
        let raw = RawDirectory::new(directory.path()).unwrap();
        let bytes = b"verified bytes";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        crate::spool::METADATA_ERROR.set(Some((0, libc::EIO)));
        let error = raw.spill(oid, bytes).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(libc::EIO)
        );
        raw.spill(oid, bytes).unwrap();
        let mut file = raw.open(oid, false, bytes.len() as u64).unwrap();
        let mut retained = Vec::new();
        file.read_to_end(&mut retained).unwrap();
        assert_eq!(retained, bytes);
    }

    #[test]
    fn copy_can_retry_after_validation_of_the_new_file_fails() {
        let directory = tempfile::tempdir().unwrap();
        let parent = File::open(directory.path()).unwrap();
        crate::spool::METADATA_ERROR.set(Some((0, libc::EIO)));
        let error = open_copy(&parent, c"copy", None).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<BackingError>()
                .unwrap()
                .0
                .raw_os_error(),
            Some(libc::EIO)
        );
        open_copy(&parent, c"copy", None).unwrap();
    }

    #[test]
    fn hot_spill_retries_a_partial_write_on_the_next_admission() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let writes = {
            let mut state = store.state.lock().unwrap();
            state.hot_limit = 1;
            state.directory.writes.clone()
        };
        let size = COPY_BUFFER + BLOCK + 1;
        let contents = |seed: u8| {
            let mut bytes = vec![seed; size];
            for (index, block) in bytes.chunks_mut(BLOCK).enumerate() {
                block.fill(seed.wrapping_add(index as u8));
            }
            bytes
        };
        let insert = |store: &mut RawBlobs, byte| {
            let bytes = contents(byte);
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store.insert(oid, bytes.len(), bytes.as_slice())
        };
        let victim = insert(&mut store, b'a').unwrap();
        let failed_after = COPY_BUFFER + 3;
        *writes.fail_write_after.lock().unwrap() = Some(failed_after);
        let (failed, failure) = mpsc::channel();
        *writes.on_spill_result.lock().unwrap() = Some(Box::new(move |succeeded| {
            if !succeeded {
                failed.send(()).unwrap();
            }
        }));

        insert(&mut store, b'b').unwrap();
        failure.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!victim.on_disk());
        assert_eq!(victim.read(0, 1).unwrap(), b"a");

        insert(&mut store, b'c').unwrap();
        assert!(victim.on_disk());
        assert_eq!(victim.read_all().unwrap(), contents(b'a'));
        store.wait_for_spill().unwrap();
        assert_eq!(
            writes.bytes.load(Ordering::Relaxed),
            (failed_after + 2 * size) as u64
        );
    }

    #[test]
    fn packed_blob_reads_retain_the_odb_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("source");
        let repo = git2::Repository::init_bare(&repository).unwrap();
        let bytes = vec![b'p'; BLOCK + 1];
        let oid = repo.blob(&bytes).unwrap();
        {
            let mut pack = repo.packbuilder().unwrap();
            pack.insert_object(oid, None).unwrap();
            pack.write(&repo.path().join("objects/pack"), 0).unwrap();
        }
        let hex = oid.to_string();
        std::fs::remove_file(repo.path().join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();
        drop(repo);

        let mut store = RawBlobs::new(directory.path()).unwrap();
        let (raw, allocation) = {
            let repo = git2::Repository::open_bare(&repository).unwrap();
            let odb = repo.odb().unwrap();
            assert!(
                odb.reader(oid).is_err(),
                "fixture must use the packed ODB path"
            );
            let object = odb.read(oid).unwrap();
            let allocation = object.data().as_ptr() as usize;
            (store.insert_odb_blob(oid, object).unwrap(), allocation)
        };
        std::thread::spawn(move || {
            let range = raw.read_shared(0, 128).unwrap();
            let slice = range.chunks().next().unwrap();
            assert_eq!(slice, &bytes[..128]);
            assert_eq!(slice.as_ptr() as usize, allocation);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_second_partial_spill_failure_keeps_the_admission_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let writes = {
            let mut state = store.state.lock().unwrap();
            state.hot_limit = 1;
            state.directory.writes.clone()
        };
        let insert = |store: &mut RawBlobs, byte| {
            let bytes = vec![byte; BLOCK + 1];
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store.insert(oid, bytes.len(), bytes.as_slice()).unwrap()
        };
        let victim = insert(&mut store, b'a');
        *writes.fail_write_after.lock().unwrap() = Some(3);
        let mut rearm = true;
        let metrics = Arc::downgrade(&writes);
        *writes.on_spill_result.lock().unwrap() = Some(Box::new(move |succeeded| {
            if !succeeded && rearm {
                *metrics.upgrade().unwrap().fail_write_after.lock().unwrap() = Some(3);
                rearm = false;
            }
        }));
        insert(&mut store, b'b');

        let bytes = vec![b'c'; BLOCK + 1];
        let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        store
            .insert_verified_bytes(oid, bytes, None, false, || true, |_| {})
            .unwrap();
        assert!(!victim.on_disk());
        assert_eq!(victim.read(0, 1).unwrap(), b"a");
        assert_eq!(store.failure(), None);
        let (blob, _) = store.for_read(oid).unwrap().unwrap();
        assert_eq!(blob.read(0, 1).unwrap(), b"c");

        assert!(store.claim(oid).unwrap());
        store.wait_for_spill().unwrap();
        assert!(victim.on_disk());
        assert_eq!(victim.read(0, 1).unwrap(), b"a");
        assert!(store.state.lock().unwrap().decoded.is_empty());
        assert_eq!(
            writes.bytes.load(Ordering::Relaxed),
            (6 + 2 * (BLOCK + 1)) as u64
        );
    }

    #[test]
    fn a_different_admission_drains_bounded_buffers_after_repeated_spill_failures() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let writes = {
            let mut state = store.state.lock().unwrap();
            state.hot_limit = 1;
            state.directory.writes.clone()
        };
        let full = Arc::new(AtomicBool::new(true));
        let spill_full = full.clone();
        let metrics = Arc::downgrade(&writes);
        *writes.fail_write_after.lock().unwrap() = Some(3);
        *writes.on_spill_result.lock().unwrap() = Some(Box::new(move |succeeded| {
            if !succeeded && spill_full.load(Ordering::Relaxed) {
                *metrics.upgrade().unwrap().fail_write_after.lock().unwrap() = Some(3);
            }
        }));
        let insert = |store: &mut RawBlobs, byte| {
            let bytes = vec![byte; BLOCK + 1];
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            let result = store.insert_verified_bytes(oid, bytes, None, false, || true, |_| {});
            (oid, result)
        };
        insert(&mut store, b'a').1.unwrap();
        insert(&mut store, b'b').1.unwrap();
        let mut accepted = Vec::new();
        for byte in b'c'..=b'n' {
            let (oid, result) = insert(&mut store, byte);
            match result {
                Ok(()) => accepted.push((oid, byte)),
                Err(error) => assert_eq!(
                    error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
                    Some(libc::ENOSPC),
                ),
            }
        }
        let waiting = store.state.lock().unwrap().decoded.len();
        assert_eq!(waiting, accepted.len());
        assert!(accepted.len() > 1);

        full.store(false, Ordering::Relaxed);
        *writes.fail_write_after.lock().unwrap() = None;
        insert(&mut store, b'z').1.unwrap();
        store.wait_for_spill().unwrap();
        assert!(
            store.state.lock().unwrap().decoded.is_empty(),
            "a new admission left {waiting} previously decoded blobs outside the LRU"
        );
        assert!(waiting < usize::from(b'n' - b'c' + 1));
        assert_eq!(store.failure(), None);
        for (oid, byte) in accepted {
            let (blob, _) = store.for_read(oid).unwrap().unwrap();
            assert_eq!(blob.read(0, 1).unwrap(), [byte]);
        }
    }

    #[test]
    fn cancelled_hot_spill_does_not_poison_the_store() {
        const CHILD: &str = "GIT_ON_TEST_CANCELLED_SPILL_CHILD";
        const SHORT_WRITE: usize = 3;
        // The shutdown latch is process-wide.
        let Ok(phase) = std::env::var(CHILD) else {
            for phase in [
                "during-write",
                "after-first-chunk",
                "after-short-write",
                "after-retry",
                "failed-retry",
                "native-failed-retry",
            ] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "raw_blob::tests::cancelled_hot_spill_does_not_poison_the_store",
                    ])
                    .env(CHILD, phase)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{phase}\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };

        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let writes = {
            let mut state = store.state.lock().unwrap();
            state.hot_limit = 1;
            state.directory.writes.clone()
        };
        let cancel_after = match phase.as_str() {
            "after-first-chunk" => Some(COPY_BUFFER),
            "after-short-write" => Some(SHORT_WRITE),
            _ => None,
        };
        if let Some(bytes) = cancel_after {
            writes
                .cancel_after_bytes
                .store(bytes as u64, Ordering::Relaxed);
        }
        match phase.as_str() {
            "during-write" => {
                *writes.on_spill.lock().unwrap() = Some(Box::new(crate::git::stop_operations));
            }
            "after-first-chunk" => {}
            "after-short-write" => *writes.fail_write_after.lock().unwrap() = Some(SHORT_WRITE),
            "after-retry" => {
                *writes.fail_write_after.lock().unwrap() = Some(0);
                *writes.on_spill_result.lock().unwrap() = Some(Box::new(move |succeeded| {
                    if succeeded {
                        crate::git::stop_operations();
                    }
                }));
            }
            "failed-retry" | "native-failed-retry" => {
                *writes.fail_write_after.lock().unwrap() = Some(0);
                let mut retry = false;
                let metrics = Arc::downgrade(&writes);
                *writes.on_spill_result.lock().unwrap() = Some(Box::new(move |succeeded| {
                    assert!(!succeeded);
                    if retry {
                        crate::git::stop_operations();
                    } else {
                        let metrics = metrics.upgrade().unwrap();
                        *metrics.fail_write_after.lock().unwrap() = Some(0);
                        retry = true;
                    }
                }));
            }
            _ => panic!("unexpected shutdown phase {phase}"),
        }
        let size = if phase == "after-first-chunk" {
            COPY_BUFFER + BLOCK + 1
        } else {
            BLOCK + 1
        };
        let admit = |store: &mut RawBlobs, byte| {
            let bytes = vec![byte; size];
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            if phase == "native-failed-retry" {
                store
                    .insert_fetched(oid, bytes.len(), bytes.as_slice(), None)
                    .map(|_| ())
            } else {
                store.insert_verified_bytes(oid, bytes, None, false, || true, |_| {})
            }
        };
        admit(&mut store, b'a').unwrap();
        admit(&mut store, b'b').unwrap();

        let error = admit(&mut store, b'c').unwrap_err();
        assert!(crate::object::is_cancelled(&error), "{error:#}");
        assert_eq!(store.failure(), None);
        assert_eq!(store.disk_metrics().1, u64::from(phase == "after-retry"));
        let mut state = store.state.lock().unwrap();
        let first_attempt = phase == "during-write" || cancel_after.is_some();
        assert_eq!(
            state.directory.open_calls.get(),
            if first_attempt { 1 } else { 2 }
        );
        if let Some(written) = cancel_after {
            let original = vec![b'a'; size];
            let oid = Oid::hash_object(ObjectType::Blob, &original).unwrap();
            assert_eq!(writes.bytes.load(Ordering::Relaxed), written as u64);
            if phase == "after-short-write" {
                assert_eq!(writes.calls.load(Ordering::Relaxed), 1);
            }
            assert!(!state.directory._owner.path().join(oid.to_string()).exists());
            let descriptor = &state.objects[&oid];
            assert!(!descriptor.file);
            assert!(descriptor.blocks.is_empty());
            assert_eq!(&**state.hot_payload(oid).unwrap(), original.as_slice());
        }
    }

    #[test]
    fn hot_eviction_allows_verified_reads_during_a_spill() {
        use std::{
            sync::{atomic::AtomicBool, mpsc},
            time::Duration,
        };

        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let data = b"unrelated";
        let oid = Oid::hash_object(ObjectType::Blob, data).unwrap();
        let reader = store.insert(oid, data.len(), data.as_slice()).unwrap();
        let insert = |store: &mut RawBlobs, value| {
            let bytes = vec![value; BLOCK + 1];
            let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
            store.insert(oid, bytes.len(), bytes.as_slice()).unwrap();
        };
        for value in 0..HOT_BLOBS as u8 {
            insert(&mut store, value);
        }
        let newest = vec![HOT_BLOBS as u8; BLOCK + 1];
        let newest_oid = Oid::hash_object(ObjectType::Blob, &newest).unwrap();
        let received = vec![HOT_BLOBS as u8 + 1; BLOCK + 1];
        let received_oid = Oid::hash_object(ObjectType::Blob, &received).unwrap();
        let mut decoder = store.clone();
        let published = store.clone();
        let observer = store.clone();
        let (entered, wait_for_spill) = mpsc::channel();
        let (resume, wait_for_resume) = mpsc::channel();
        let (decoded, wait_for_decoded) = mpsc::channel();
        let (additional, wait_for_additional) = mpsc::channel();
        let mut another_decoder = store.clone();
        let last_decoder = store.clone();
        let _active: Vec<_> = (2..HOT_BLOBS)
            .map(|_| {
                store
                    .reserve_pack_decode(
                        || PackDecodePriority::Demand,
                        crate::object::check_transfer,
                    )
                    .unwrap()
            })
            .collect();
        let writes = store.state.lock().unwrap().directory.writes.clone();
        let first = AtomicBool::new(true);
        *writes.on_spill.lock().unwrap() = Some(Box::new(move || {
            if first.swap(false, Ordering::Relaxed) {
                entered.send(()).unwrap();
                wait_for_resume.recv().unwrap();
            }
        }));

        std::thread::scope(|scope| {
            scope.spawn(|| insert(&mut store, HOT_BLOBS as u8));
            let spill_started = wait_for_spill.recv_timeout(Duration::from_secs(3));
            let (read, wait_for_read) = mpsc::channel();
            if spill_started.is_ok() {
                let bytes = received.clone();
                scope.spawn(move || {
                    let _decode = decoder
                        .reserve_pack_decode(
                            || PackDecodePriority::Demand,
                            crate::object::check_transfer,
                        )
                        .unwrap();
                    decoder
                        .insert_verified_bytes(
                            received_oid,
                            bytes,
                            Some(Arc::default()),
                            true,
                            || false,
                            |_| {},
                        )
                        .unwrap();
                });
                scope.spawn(move || {
                    let first = reader.read_all();
                    let second = published
                        .get(newest_oid)
                        .and_then(|blob| blob.context("verified blob is still unavailable"))
                        .and_then(|blob| blob.read_all());
                    let _ = read.send((first, second, published.hot_metrics()));
                });
            }
            let outcome = wait_for_read.recv_timeout(Duration::from_secs(3));
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while spill_started.is_ok()
                && !observer
                    .state
                    .lock()
                    .unwrap()
                    .preparing
                    .contains(&received_oid)
                && std::time::Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            let was_forecast = !observer.available(received_oid);
            let streamed = (observer.lookup_demand(received_oid) == DemandLookup::ClaimedForecast)
                .then(|| {
                    let fetched = observer.get(received_oid).unwrap().unwrap().read_all();
                    (fetched, observer.hot_metrics().3)
                });
            let second_ready = if streamed.is_some() {
                scope.spawn(move || {
                    let _decode = another_decoder
                        .reserve_pack_decode(
                            || PackDecodePriority::Demand,
                            crate::object::check_transfer,
                        )
                        .unwrap();
                    let bytes = vec![HOT_BLOBS as u8 + 2; BLOCK + 1];
                    let oid = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
                    another_decoder
                        .insert_verified_bytes(
                            oid,
                            bytes,
                            None,
                            false,
                            || {
                                let _ = decoded.send(());
                                true
                            },
                            move |_| {
                                let _ = additional.send(());
                            },
                        )
                        .unwrap();
                });
                wait_for_decoded
                    .recv_timeout(Duration::from_secs(3))
                    .is_ok()
                    && wait_for_additional
                        .recv_timeout(Duration::from_millis(100))
                        .is_ok()
            } else {
                false
            };
            let active_bytes = observer.hot_metrics().3;
            let (started, wait_for_start) = mpsc::channel();
            let (reserved, wait_for_slot) = mpsc::channel();
            let waits_before_decoding = if second_ready {
                scope.spawn(move || {
                    started.send(()).unwrap();
                    let _decode = last_decoder
                        .reserve_pack_decode(
                            || PackDecodePriority::Demand,
                            crate::object::check_transfer,
                        )
                        .unwrap();
                    reserved.send(()).unwrap();
                });
                wait_for_start.recv_timeout(Duration::from_secs(3)).is_ok()
                    && matches!(
                        wait_for_slot.recv_timeout(Duration::from_millis(100)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    )
            } else {
                false
            };
            let _ = resume.send(());
            spill_started.expect("eviction must attempt a spill");
            let (first, second, (hot_bytes, hot_entries, spill_bytes, _)) =
                outcome.expect("spill blocked a read");
            assert_eq!(first.unwrap(), data);
            assert_eq!(second.unwrap(), newest);
            assert_eq!(hot_entries, HOT_BLOBS);
            assert_eq!(hot_bytes, HOT_BLOBS * (BLOCK + 1));
            assert_eq!(spill_bytes, BLOCK + 1);
            assert!(was_forecast, "unused prediction was published");
            let (fetched, decode_bytes) =
                streamed.expect("a verified pack blob waited for an unrelated spill");
            assert_eq!(fetched.unwrap(), received);
            assert_eq!(decode_bytes, BLOCK + 1);
            assert!(
                second_ready,
                "a second verified decoder waited for the paused spill"
            );
            assert_eq!(active_bytes, 2 * (BLOCK + 1));
            assert!(
                waits_before_decoding,
                "large decode started without a buffer slot"
            );
            wait_for_slot.recv_timeout(Duration::from_secs(3)).unwrap();
        });
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
        {
            let mut state = store.state.lock().unwrap();
            state.memory_limit = 2 * (BLOCK + ENTRY_CHARGE);
            state.hot_limit = 0;
        }
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
        let store = RawBlobs::new(directory.path()).unwrap();
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
        assert_eq!(tree.children().len(), 1);
    }
}
