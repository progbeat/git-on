//! Incremental pkt-line and PACK decoding for the reusable fetch transports.
use crate::{object, raw_blob::RawBlobs};
use anyhow::{bail, ensure, Context, Result};
use flate2::{Decompress, FlushDecompress, Status};
use git2::{ObjectType, Oid};
use sha1_checked::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    io::BufRead,
    mem::MaybeUninit,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

mod delta;

const TRAILER: usize = 20;
const INFLATE_CHUNK: usize = 32 * 1024;

#[cfg(test)]
thread_local! {
    static DELTA_APPLICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone)]
pub(crate) struct VerifiedObject {
    oid: Oid,
    kind: ObjectType,
    data: Arc<Vec<u8>>,
}

impl VerifiedObject {
    pub(crate) fn oid(&self) -> Oid {
        self.oid
    }

    pub(crate) fn kind(&self) -> ObjectType {
        self.kind
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }

    pub(crate) fn into_data(self) -> Arc<Vec<u8>> {
        self.data
    }
}

type Publish<'a> = Box<dyn FnMut(VerifiedObject) -> Result<()> + Send + 'a>;
type DecodePriority = Box<dyn Fn(&[Oid]) -> crate::raw_blob::PackDecodePriority + Send>;
type PendingExtra = Box<dyn FnMut(Oid) + Send>;

pub(crate) struct Received {
    pub(crate) bytes: u64,
    pub(crate) eager_verified_capacity: usize,
    extras: Vec<VerifiedObject>,
}

impl Received {
    pub(crate) fn cache_extras(
        &mut self,
        scratch: &RawBlobs,
        source: Option<&Arc<crate::raw_blob::Transfer>>,
        mut record: impl FnMut(Oid),
    ) {
        let extras = std::mem::take(&mut self.extras);
        for entry in &extras {
            record(entry.oid);
        }
        scratch.remember_verified(extras, source);
    }
}

pub(crate) struct Response<'a> {
    pack: Pack<'a>,
    progress: Option<crate::terminal_ui::PackProgress>,
    phase: Phase,
    header: [u8; 4],
    header_len: usize,
    remaining: usize,
    channel: Option<u8>,
    text: Vec<u8>,
    end: [u8; 4],
    end_len: usize,
}

#[derive(Clone, Copy)]
enum Phase {
    Section,
    Shallow,
    PackHeader,
    Pack,
    Done,
}

impl<'a> Response<'a> {
    pub(crate) fn new(scratch: RawBlobs, wants: Vec<Oid>, publish: Publish<'a>) -> Self {
        Self {
            pack: Pack {
                scratch,
                stop: None,
                wants,
                publish,
                snapshot: None,
                pending_extra: None,
                decode_priority: None,
                bytes: 0,
                pending: Vec::new(),
                checksum: Checksum::default(),
                consumed: 0,
                count: None,
                entries: 0,
                entry: None,
                accepts_surplus: false,
                bases: None,
            },
            progress: None,
            phase: Phase::Section,
            header: [0; 4],
            header_len: 0,
            remaining: 0,
            channel: None,
            text: Vec::new(),
            end: [0; 4],
            end_len: 0,
        }
    }

    pub(crate) fn for_materialization(scratch: RawBlobs, oid: Oid) -> Self {
        let mut response = Self::new(scratch, vec![oid], Box::new(|_| Ok(())));
        // Eager execution starts only after the whole pack is validated.
        // Retain the selected object with the rest instead of publishing it early.
        response.pack.snapshot = Some(SnapshotSelection {
            oid,
            shallow: HashSet::new(),
        });
        response.progress =
            crate::terminal_ui::progress_enabled().then(crate::terminal_ui::PackProgress::new);
        response
    }

    pub(crate) fn set_stop(&mut self, stop: Option<Arc<AtomicBool>>) {
        self.pack.stop = stop;
    }

    pub(crate) fn set_decode_priority(
        &mut self,
        priority: impl Fn(&[Oid]) -> crate::raw_blob::PackDecodePriority + Send + 'static,
    ) {
        self.pack.decode_priority = Some(Box::new(priority));
    }

    pub(crate) fn on_pending_extra(&mut self, pending: impl FnMut(Oid) + Send + 'static) {
        self.pack.pending_extra = Some(Box::new(pending));
    }

    fn done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// Leave bytes after the response in the caller's buffer. Native helpers
    /// have a separate response-end marker. Direct SSH uses this same framer.
    fn feed(&mut self, mut input: &[u8]) -> Result<usize> {
        let len = input.len();
        while !input.is_empty() && !self.done() {
            self.pack.check_transfer()?;
            if self.remaining == 0 {
                let count = (4 - self.header_len).min(input.len());
                let (part, rest) = input.split_at(count);
                self.header[self.header_len..self.header_len + count].copy_from_slice(part);
                self.header_len += count;
                input = rest;
                if self.header_len < 4 {
                    break;
                }
                self.header_len = 0;
                match crate::transport::packet_length(&self.header)? {
                    0 if matches!(self.phase, Phase::Pack) => self.phase = Phase::Done,
                    1 if matches!(self.phase, Phase::Shallow) => self.phase = Phase::PackHeader,
                    length @ 5..=65520 => {
                        self.remaining = length - 4;
                        self.channel = None;
                    }
                    _ => bail!("unexpected pkt-line in Git fetch response"),
                }
                continue;
            }
            let count = self.remaining.min(input.len());
            let (part, rest) = input.split_at(count);
            self.remaining -= count;
            input = rest;
            if matches!(self.phase, Phase::Pack) {
                let (channel, payload) = match self.channel {
                    Some(channel) => (channel, part),
                    None => {
                        let channel = part[0];
                        self.channel = Some(channel);
                        // A Git ERR packet is not prefixed with a sideband.
                        if channel == b'E' {
                            self.text.push(channel);
                        }
                        (channel, &part[1..])
                    }
                };
                match channel {
                    1 => {
                        self.pack.write(payload)?;
                        if let (Some(progress), Some(total)) = (&mut self.progress, self.pack.count)
                        {
                            progress.received(self.pack.entries, total, self.pack.bytes);
                        }
                    }
                    2 => {
                        if let Some(progress) = &mut self.progress {
                            progress.remote(payload);
                        }
                    }
                    3 | b'E' => self.text.extend_from_slice(payload),
                    _ => bail!("invalid Git pack sideband"),
                }
            } else {
                self.text.extend_from_slice(part);
            }
            if self.remaining == 0 {
                self.finish_packet()?;
            }
        }
        Ok(len - input.len())
    }

    fn finish_packet(&mut self) -> Result<()> {
        if matches!(
            self.phase,
            Phase::Section | Phase::Shallow | Phase::PackHeader
        ) || self.channel == Some(b'E')
        {
            crate::transport::check_packet_error(&self.text)?;
        }
        match self.phase {
            Phase::Section => match crate::transport::packet_text(&self.text) {
                b"packfile" => self.phase = Phase::Pack,
                b"shallow-info" => self.phase = Phase::Shallow,
                _ => bail!("invalid Git fetch response section"),
            },
            Phase::Shallow => {
                // Requests start without any advertised shallow boundary.
                let oid = crate::transport::packet_text(&self.text)
                    .strip_prefix(b"shallow ")
                    .filter(|oid| oid.len() == 40)
                    .and_then(|oid| Oid::from_str(std::str::from_utf8(oid).ok()?).ok())
                    .context("invalid Git shallow section")?;
                if let Some(snapshot) = &mut self.pack.snapshot {
                    snapshot.shallow.insert(oid);
                }
            }
            Phase::PackHeader => {
                ensure!(
                    crate::transport::packet_text(&self.text) == b"packfile",
                    "invalid Git fetch response section"
                );
                self.phase = Phase::Pack;
            }
            Phase::Pack if self.channel == Some(3) => {
                bail!(
                    "remote pack failed: {}",
                    String::from_utf8_lossy(&self.text)
                );
            }
            Phase::Pack if self.channel == Some(b'E') => bail!("invalid Git pack sideband"),
            _ => (),
        }
        self.text.clear();
        Ok(())
    }

    pub(crate) fn read(mut self, input: &mut impl BufRead) -> Result<Received> {
        while !self.done() {
            let bytes = input.fill_buf()?;
            ensure!(!bytes.is_empty(), "truncated Git fetch response");
            let consumed = self.feed(bytes)?;
            input.consume(consumed);
        }
        self.pack.finish()
    }

    pub(crate) fn write_http(&mut self, bytes: &[u8]) -> Result<()> {
        let consumed = self.feed(bytes)?;
        let end = &bytes[consumed..];
        ensure!(
            end.len() <= self.end.len() - self.end_len,
            "unexpected data after Git HTTP response"
        );
        self.end[self.end_len..self.end_len + end.len()].copy_from_slice(end);
        self.end_len += end.len();
        Ok(())
    }

    pub(crate) fn finish_http(self) -> Result<Received> {
        ensure!(self.done(), "truncated Git fetch response");
        ensure!(
            self.end_len == 0 || &self.end[..self.end_len] == b"0002",
            "unexpected data after Git HTTP response"
        );
        self.pack.finish()
    }
}

struct Pack<'a> {
    scratch: RawBlobs,
    stop: Option<Arc<AtomicBool>>,
    wants: Vec<Oid>,
    publish: Publish<'a>,
    snapshot: Option<SnapshotSelection>,
    pending_extra: Option<PendingExtra>,
    decode_priority: Option<DecodePriority>,
    bytes: u64,
    pending: Vec<u8>,
    checksum: Checksum,
    consumed: usize,
    count: Option<usize>,
    entries: usize,
    entry: Option<Entry>,
    accepts_surplus: bool,
    bases: Option<PackBases>,
}

struct SnapshotSelection {
    oid: Oid,
    shallow: HashSet<Oid>,
}

struct Entry {
    offset: usize,
    content: EntryContent,
    size: usize,
    inflater: Decompress,
    hash: Option<Sha1>,
    data: Vec<u8>,
    decode: Option<crate::raw_blob::PackDecode>,
}

#[derive(Clone, Copy)]
enum PackedKind {
    Full(ObjectType),
    OffsetDelta,
    RefDelta,
}

#[derive(Clone, Copy)]
enum EntryContent {
    Full(ObjectType),
    Delta(BaseReference),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum BaseReference {
    Offset(usize),
    Oid(Oid),
}

struct PendingDelta {
    offset: usize,
    instructions: Vec<u8>,
}

struct ReadyEntry {
    offset: usize,
    object: VerifiedObject,
    decode: Option<crate::raw_blob::PackDecode>,
}

#[derive(Default)]
struct PackBases {
    offsets: HashMap<usize, Oid>,
    objects: HashMap<Oid, VerifiedObject>,
    pending: HashMap<BaseReference, Vec<PendingDelta>>,
    extras: Vec<VerifiedObject>,
    eager_verified_capacity: usize,
}

impl PackBases {
    fn resolve(&self, reference: BaseReference) -> Option<Oid> {
        let oid = match reference {
            BaseReference::Offset(offset) => *self.offsets.get(&offset)?,
            BaseReference::Oid(oid) => oid,
        };
        self.objects.contains_key(&oid).then_some(oid)
    }

    fn check_snapshot(
        &self,
        selection: &SnapshotSelection,
        stop: Option<&AtomicBool>,
    ) -> Result<()> {
        let get = |oid: Oid| {
            self.objects
                .get(&oid)
                .with_context(|| format!("shallow pack omitted required object {oid}"))
        };
        let mut selected = get(selection.oid)?;
        while selected.kind == ObjectType::Tag {
            check_transfer(stop)?;
            selected = get(object::selected_child(selected.kind, selected.data())?)?;
        }
        if selected.kind != ObjectType::Commit || selection.shallow.contains(&selected.oid) {
            return Ok(());
        }
        // Materialization checks the selected tree. If the server did not
        // declare the selected commit shallow, native fetch also expects the
        // reachable history and its trees up to the declared boundaries.
        let mut pending = Vec::new();
        let tree = object::commit_links(selected.data(), |parent| {
            pending.push((parent, ObjectType::Commit));
        })?;
        if pending.is_empty() {
            return Ok(());
        }
        let mut visited = HashSet::from([selected.oid, tree]);
        while let Some((oid, kind)) = pending.pop() {
            check_transfer(stop)?;
            let verified = self
                .objects
                .get(&oid)
                .with_context(|| format!("shallow pack omitted required {} {oid}", kind.str()))?;
            ensure!(
                verified.kind == kind,
                "shallow pack references {oid} as a {}",
                kind.str()
            );
            if kind == ObjectType::Blob || !visited.insert(oid) {
                continue;
            }
            if kind == ObjectType::Commit {
                let tree = object::commit_links(verified.data(), |parent| {
                    if !selection.shallow.contains(&oid) {
                        pending.push((parent, ObjectType::Commit));
                    }
                })?;
                pending.push((tree, ObjectType::Tree));
            } else {
                let directory = crate::tree::Directory::from_verified(verified)?;
                for index in 0..directory.len() {
                    let (mode, oid) = directory.metadata_at(index).expect("tree entry");
                    match mode {
                        0o040000 => pending.push((oid, ObjectType::Tree)),
                        0o160000 => (), // Gitlinks name objects outside the repository.
                        _ => pending.push((oid, ObjectType::Blob)),
                    }
                }
            }
        }
        Ok(())
    }
}

fn check_transfer(stop: Option<&AtomicBool>) -> Result<()> {
    if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
        return Err(object::OperationCancelled.into());
    }
    object::check_transfer()
}

impl Entry {
    fn inflate(&mut self, input: &[u8], stop: Option<&AtomicBool>) -> Result<(usize, bool)> {
        inflate_buffer(
            &mut self.inflater,
            self.size,
            &mut self.data,
            self.hash.as_mut(),
            input,
            stop,
        )
    }
}

fn inflate_buffer(
    inflater: &mut Decompress,
    size: usize,
    data: &mut Vec<u8>,
    mut hash: Option<&mut Sha1>,
    mut input: &[u8],
    stop: Option<&AtomicBool>,
) -> Result<(usize, bool)> {
    let length = input.len();
    loop {
        check_transfer(stop)?;
        let before_in = inflater.total_in();
        let before_out = inflater.total_out();
        let initialized = data.len();
        let output_len = (size - initialized).min(INFLATE_CHUNK);
        // Give zlib room to consume its end marker after the declared
        // output. Any further output means the object is invalid.
        let mut overflow = [MaybeUninit::uninit()];
        let output = if output_len == 0 {
            &mut overflow[..]
        } else {
            &mut data.spare_capacity_mut()[..output_len]
        };
        let status = inflater.decompress_uninit(input, output, FlushDecompress::None)?;
        let consumed = (inflater.total_in() - before_in) as usize;
        let used = (inflater.total_out() - before_out) as usize;
        input = &input[consumed..];
        ensure!(used <= output_len, "oversized Git pack object");
        if used != 0 {
            // zlib initialized these bytes in the reserved output slice.
            unsafe { data.set_len(initialized + used) };
            if let Some(hash) = hash.as_deref_mut() {
                hash.update(&data[initialized..]);
            }
        }
        if status == Status::StreamEnd || (consumed == 0 && used == 0) {
            return Ok((length - input.len(), status == Status::StreamEnd));
        }
    }
}

fn reconstruct(
    delta: delta::Delta<'_>,
    base: &[u8],
    hash: &mut Sha1,
    stop: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    #[cfg(test)]
    DELTA_APPLICATIONS.set(DELTA_APPLICATIONS.get() + 1);
    let mut data = Vec::new();
    data.try_reserve_exact(delta.target_size())?;
    delta.apply(base.len(), |part| {
        check_transfer(stop)?;
        let bytes = match part {
            delta::Part::Bytes(literal) => literal,
            delta::Part::Copy(range) => &base[range],
        };
        hash.update(bytes);
        data.extend_from_slice(bytes);
        Ok(())
    })?;
    Ok(data)
}

impl Pack<'_> {
    fn check_transfer(&self) -> Result<()> {
        check_transfer(self.stop.as_deref())
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<()> {
        if self.count == Some(self.entries) {
            ensure!(
                bytes.len() <= TRAILER - self.pending.len(),
                "oversized Git pack trailer"
            );
        }
        self.bytes += bytes.len() as u64;
        self.checksum.update(bytes);
        if !self.pending.is_empty() && self.count != Some(self.entries) {
            // Complete the field that straddles these writes, then let zlib
            // consume the compressed body directly from the transport slice.
            let mut prefix = std::mem::take(&mut self.pending);
            while let Some((&byte, rest)) = bytes.split_first() {
                prefix.push(byte);
                let used = self.inflate_entries(&prefix, rest.len())?;
                prefix.drain(..used);
                self.consumed += used;
                bytes = rest;
                if prefix.is_empty() || self.count == Some(self.entries) {
                    break;
                }
            }
            self.pending = prefix;
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let used = self.inflate_entries(bytes, 0)?;
        self.pending.extend_from_slice(&bytes[used..]);
        self.consumed += used;
        Ok(())
    }

    fn inflate_entries(&mut self, input: &[u8], remaining: usize) -> Result<usize> {
        let mut position = 0;
        let count = match self.count {
            Some(count) => count,
            None if input.len() < 12 => return Ok(0),
            None => {
                let header = &input[..12];
                ensure!(
                    &header[..4] == b"PACK"
                        && matches!(u32::from_be_bytes(header[4..8].try_into()?), 2 | 3),
                    "invalid Git pack header"
                );
                let count = u32::from_be_bytes(header[8..].try_into()?) as usize;
                self.count = Some(count);
                self.accepts_surplus = count > self.wants.len();
                position = 12;
                if count > 1 || self.snapshot.is_some() {
                    self.bases = Some(PackBases::default());
                }
                count
            }
        };
        while self.entries < count {
            self.check_transfer()?;
            if self.entry.is_none() {
                let Some((kind, size, length)) = entry_header(&input[position..])? else {
                    break;
                };
                let offset = self.consumed + position;
                let base_header = &input[position + length..];
                let (content, extra_length) = match kind {
                    PackedKind::Full(kind) => (EntryContent::Full(kind), 0),
                    PackedKind::OffsetDelta => {
                        let Some((base, length)) = offset_base(base_header, offset)? else {
                            break;
                        };
                        (EntryContent::Delta(BaseReference::Offset(base)), length)
                    }
                    PackedKind::RefDelta => {
                        let Some(oid) = base_header.get(..20) else {
                            break;
                        };
                        (
                            EntryContent::Delta(BaseReference::Oid(Oid::from_bytes(oid)?)),
                            20,
                        )
                    }
                };
                let decode = self.reserve_decode(
                    !matches!(content, EntryContent::Full(kind) if kind != ObjectType::Blob)
                        && size > crate::raw_blob::BLOCK,
                )?;
                let mut data = Vec::new();
                // The pack header gives the final size. Reserve without
                // initializing it so later chunks never move decoded bytes.
                data.try_reserve_exact(size)?;
                position += length + extra_length;
                self.entry = Some(Entry {
                    offset,
                    content,
                    size,
                    inflater: Decompress::new(true),
                    hash: match content {
                        EntryContent::Full(kind) => Some(object::object_hash(kind, size)),
                        EntryContent::Delta(_) => None,
                    },
                    data,
                    decode,
                });
            }
            let entry = self.entry.as_mut().expect("entry initialized");
            let (consumed, finished) = entry.inflate(&input[position..], self.stop.as_deref())?;
            position += consumed;
            if !finished {
                break;
            }
            if self.entries + 1 == count {
                ensure!(
                    input.len() - position + remaining <= TRAILER,
                    "oversized Git pack trailer"
                );
            }
            self.publish_entry()?;
        }
        ensure!(
            self.entries < count || input.len() - position + remaining <= TRAILER,
            "oversized Git pack trailer"
        );
        Ok(position)
    }

    fn publish_entry(&mut self) -> Result<()> {
        let entry = self.entry.take().expect("entry initialized");
        ensure!(
            entry.inflater.total_out() == entry.size as u64,
            "truncated Git pack object"
        );
        self.entries += 1;
        match entry.content {
            EntryContent::Full(kind) => {
                let hash = entry.hash.expect("canonical object hash");
                let object = VerifiedObject {
                    oid: Oid::from_bytes(&object::finish_sha1(hash)?)?,
                    kind,
                    data: Arc::new(entry.data),
                };
                self.publish_objects(ReadyEntry {
                    offset: entry.offset,
                    object,
                    decode: entry.decode,
                })
            }
            EntryContent::Delta(reference) => {
                let delta = PendingDelta {
                    offset: entry.offset,
                    instructions: entry.data,
                };
                let bases = self
                    .bases
                    .as_mut()
                    .context("Git delta pack has no base entries")?;
                let Some(base) = bases.resolve(reference) else {
                    bases.pending.entry(reference).or_default().push(delta);
                    return Ok(());
                };
                let entry = self.apply_delta(delta, base, entry.decode)?;
                self.publish_objects(entry)
            }
        }
    }

    fn publish_objects(&mut self, entry: ReadyEntry) -> Result<()> {
        let mut runnable = Vec::new();
        let mut next = Some(entry);
        while let Some(ReadyEntry {
            offset,
            object,
            decode,
        }) = next
        {
            let oid = object.oid;
            let wanted = self.wants.iter().position(|&item| item == oid);
            if let Some(want) = wanted {
                self.wants.swap_remove(want);
                if self.snapshot.is_none() {
                    if self.bases.is_none() {
                        return (self.publish)(object);
                    }
                    (self.publish)(object.clone())?;
                }
            } else if self.bases.is_none() {
                bail!("remote supplied unexpected object {oid} in a single-entry pack");
            } else if !self.accepts_surplus {
                bail!("remote supplied unexpected object {oid} instead of a requested object");
            }
            let bases = self.bases.as_mut().expect("multi-object stream");
            if let std::collections::hash_map::Entry::Vacant(slot) = bases.objects.entry(oid) {
                if self.snapshot.is_some() {
                    // Pack bases and extras can own the same buffer.
                    bases.eager_verified_capacity += object.data.capacity();
                }
                if (wanted.is_none() || self.snapshot.is_some())
                    && self.scratch.needs_pack_proof(oid)
                {
                    bases.extras.push(object.clone());
                    if let Some(pending) = &mut self.pending_extra {
                        pending(oid);
                    }
                }
                // Later deltas in this pack borrow the same verified buffer
                // even if the lazy read cache spills this object in parallel.
                slot.insert(object);
            }
            bases.offsets.insert(offset, oid);
            for reference in [BaseReference::Offset(offset), BaseReference::Oid(oid)] {
                if let Some(pending) = bases.pending.remove(&reference) {
                    runnable.extend(pending.into_iter().map(|delta| (delta, oid)));
                }
            }
            drop(decode);
            next = if let Some((delta, oid)) = runnable.pop() {
                Some(self.apply_delta(delta, oid, None)?)
            } else {
                None
            };
        }
        Ok(())
    }

    fn apply_delta(
        &self,
        entry: PendingDelta,
        oid: Oid,
        decode: Option<crate::raw_blob::PackDecode>,
    ) -> Result<ReadyEntry> {
        let delta = delta::Delta::parse(&entry.instructions)?;
        let size = delta.target_size();
        let decode = match decode {
            Some(held) => Some(held),
            None => {
                self.reserve_decode(size.max(entry.instructions.len()) > crate::raw_blob::BLOCK)?
            }
        };
        let bases = self.bases.as_ref().expect("known delta base");
        let base = &bases.objects[&oid];
        let kind = base.kind();
        let mut hash = object::object_hash(kind, size);
        let data = reconstruct(delta, base.data(), &mut hash, self.stop.as_deref())?;
        Ok(ReadyEntry {
            offset: entry.offset,
            object: VerifiedObject {
                oid: Oid::from_bytes(&object::finish_sha1(hash)?)?,
                kind,
                data: Arc::new(data),
            },
            decode,
        })
    }

    fn reserve_decode(&self, large: bool) -> Result<Option<crate::raw_blob::PackDecode>> {
        large
            .then(|| {
                self.scratch.reserve_pack_decode(
                    || {
                        self.decode_priority
                            .as_ref()
                            .map_or(crate::raw_blob::PackDecodePriority::Demand, |priority| {
                                priority(&self.wants)
                            })
                    },
                    || self.check_transfer(),
                )
            })
            .transpose()
    }

    fn finish(mut self) -> Result<Received> {
        self.checksum.finish()?;
        ensure!(
            self.count == Some(self.entries) && self.pending.len() == TRAILER,
            "incomplete Git pack entries"
        );
        if let Some(oid) = self.wants.first() {
            bail!("remote did not supply requested object {oid}");
        }
        let (extras, eager_verified_capacity) = if let Some(bases) = self.bases.take() {
            ensure!(bases.pending.is_empty(), "Git pack omitted a delta base");
            if let Some(selection) = &self.snapshot {
                bases.check_snapshot(selection, self.stop.as_deref())?;
            }
            (bases.extras, bases.eager_verified_capacity)
        } else {
            (Vec::new(), 0)
        };
        Ok(Received {
            bytes: self.bytes,
            eager_verified_capacity,
            extras,
        })
    }
}

// Whole-pack integrity is independent of the canonical object OIDs checked
// during decoding. The rolling trailer keeps this to one pass over wire bytes.
#[derive(Default)]
pub(crate) struct Checksum {
    hash: Sha1,
    tail: Vec<u8>,
}

impl Checksum {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        if bytes.len() >= TRAILER {
            self.hash.update(&self.tail);
            self.hash.update(&bytes[..bytes.len() - TRAILER]);
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - TRAILER..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(TRAILER);
            self.hash.update(&self.tail[..excess]);
            self.tail.drain(..excess);
            self.tail.extend_from_slice(bytes);
        }
    }

    pub(crate) fn finish(self) -> Result<()> {
        ensure!(
            self.tail.len() == TRAILER && object::finish_sha1(self.hash)?.as_slice() == self.tail,
            "received pack checksum mismatch"
        );
        Ok(())
    }
}

fn entry_header(bytes: &[u8]) -> Result<Option<(PackedKind, usize, usize)>> {
    let Some((&first, rest)) = bytes.split_first() else {
        return Ok(None);
    };
    let kind = match (first >> 4) & 7 {
        1 => PackedKind::Full(ObjectType::Commit),
        2 => PackedKind::Full(ObjectType::Tree),
        3 => PackedKind::Full(ObjectType::Blob),
        4 => PackedKind::Full(ObjectType::Tag),
        6 => PackedKind::OffsetDelta,
        7 => PackedKind::RefDelta,
        _ => bail!("invalid Git pack object type"),
    };
    let mut size = usize::from(first & 15);
    if first & 128 == 0 {
        return Ok(Some((kind, size, 1)));
    }
    let mut shift = 4;
    for (index, &byte) in rest.iter().enumerate() {
        let next = usize::from(byte & 127);
        ensure!(
            shift < usize::BITS && next <= usize::MAX >> shift,
            "Git pack object size overflow"
        );
        size |= next << shift;
        if byte & 128 == 0 {
            return Ok(Some((kind, size, index + 2)));
        }
        shift += 7;
    }
    Ok(None)
}

fn offset_base(bytes: &[u8], offset: usize) -> Result<Option<(usize, usize)>> {
    let Some((&first, remaining)) = bytes.split_first() else {
        return Ok(None);
    };
    let mut distance = usize::from(first & 127);
    let mut latest = first;
    let mut length = 1;
    for &byte in remaining {
        if latest & 128 == 0 {
            break;
        }
        distance = distance
            .checked_add(1)
            .and_then(|value| value.checked_mul(128))
            .context("Git delta offset overflow")?
            | usize::from(byte & 127);
        latest = byte;
        length += 1;
    }
    if latest & 128 != 0 {
        return Ok(None);
    }
    ensure!(
        distance != 0 && distance <= offset.saturating_sub(12),
        "invalid Git delta base offset"
    );
    Ok(Some((offset - distance, length)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_blob::ReceivedObject;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    fn verified_extra(store: &RawBlobs, oid: Oid) -> VerifiedObject {
        match store.received(oid) {
            Some(ReceivedObject::Verified { object, .. }) => object,
            _ => panic!("expected a verified extra Git object"),
        }
    }

    fn pack_entry(kind: u8, base: &[u8], data: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        let mut size = data.len() >> 4;
        let mut byte = kind << 4 | (data.len() & 15) as u8;
        while size != 0 {
            header.push(byte | 128);
            byte = (size & 127) as u8;
            size >>= 7;
        }
        header.push(byte);
        header.extend_from_slice(base);
        let mut encoder = ZlibEncoder::new(header, Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn blob_pack_header(size: usize) -> Vec<u8> {
        let mut packet = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
        let mut remaining = size >> 4;
        let mut byte = 0x30 | (size & 15) as u8;
        while remaining != 0 {
            packet.push(byte | 0x80);
            byte = (remaining & 0x7f) as u8;
            remaining >>= 7;
        }
        packet.push(byte);
        packet
    }

    #[test]
    fn an_offset_delta_in_the_first_pack_uses_the_verified_base_without_staging() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let base = vec![b'x'; 258];
        let expected = [&base[..200], b"!"].concat();
        let wanted = Oid::hash_object(ObjectType::Blob, &expected).unwrap();
        let base_oid = Oid::hash_object(ObjectType::Blob, &base).unwrap();
        let mut header = blob_pack_header(base.len());
        header[11] = 2;
        let mut encoder = flate2::write::ZlibEncoder::new(header, flate2::Compression::none());
        encoder.write_all(&base).unwrap();
        let mut pack = encoder.finish().unwrap();
        let distance = pack.len() - 12;
        assert!((128..16_512).contains(&distance));
        let instructions = [0x82, 2, 0xc9, 1, 0x90, 200, 1, b'!'];
        pack.extend_from_slice(&[
            0x60 | instructions.len() as u8,
            0x80 | ((distance >> 7) - 1) as u8,
            (distance & 127) as u8,
        ]);
        let mut encoder = flate2::write::ZlibEncoder::new(pack, flate2::Compression::fast());
        encoder.write_all(&instructions).unwrap();
        let pack = encoder.finish().unwrap();
        let published = AtomicBool::new(false);
        let mut response = Response::new(
            scratch.clone(),
            vec![wanted],
            Box::new(|object| {
                assert_eq!(object.data(), expected);
                published.store(true, Ordering::Relaxed);
                Ok(())
            }),
        );
        let base_end = distance + 12;
        for fragment in pack[..base_end].chunks(1) {
            response.pack.write(fragment).unwrap();
        }
        for fragment in pack[base_end..].chunks(1) {
            response.pack.write(fragment).unwrap();
        }
        // The wanted delta can be read before the trailer. Its unsolicited
        // base must stay private until the whole response passes validation.
        assert!(published.load(Ordering::Relaxed));
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "only the raw cache directory should exist"
        );
        assert!(scratch.get(base_oid).unwrap().is_none());
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        let mut received = response.pack.finish().unwrap();
        received.cache_extras(&scratch, None, |_| {});
        assert_eq!(verified_extra(&scratch, base_oid).data(), base);
    }

    #[test]
    fn a_pack_carried_base_reuses_its_freshly_verified_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let mut scratch = RawBlobs::new(directory.path()).unwrap();
        let base = b"verified";
        let base_oid = Oid::hash_object(ObjectType::Blob, base).unwrap();
        scratch
            .insert_fetched(base_oid, base.len(), base.as_slice(), None)
            .unwrap();
        scratch.evict_all_for_test().unwrap();
        let backing = std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(base_oid.to_string());
        std::fs::write(backing, b"tampered").unwrap();
        let wanted = Oid::hash_object(ObjectType::Blob, b"verified!").unwrap();
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        pack.extend_from_slice(&pack_entry(3, &[], base));
        pack.extend_from_slice(&pack_entry(
            7,
            base_oid.as_bytes(),
            &[8, 9, 0x90, 8, 1, b'!'],
        ));
        let mut response = Response::new(
            scratch,
            vec![wanted],
            Box::new(|object| {
                assert_eq!(object.data(), b"verified!");
                Ok(())
            }),
        );
        response.pack.write(&pack).unwrap();
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        response.pack.finish().unwrap();
    }

    #[test]
    fn forward_deltas_wait_for_their_base_without_holding_decode_slots() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let base = vec![b'x'; 65_537];
        let base_oid = Oid::hash_object(ObjectType::Blob, &base).unwrap();
        let wanted = Oid::hash_object(ObjectType::Blob, b"ok").unwrap();
        let mut response = Response::new(
            scratch.clone(),
            vec![wanted],
            Box::new(|object| {
                assert_eq!(object.data(), b"ok");
                Ok(())
            }),
        );
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x0c".to_vec();
        response.pack.write(&pack).unwrap();
        for byte in 0..10 {
            let mut instructions = vec![0x81, 0x80, 4, 0x81, 0x80, 4];
            let output = vec![byte; base.len()];
            for literal in output.chunks(127) {
                instructions.push(literal.len() as u8);
                instructions.extend_from_slice(literal);
            }
            let entry = pack_entry(7, base_oid.as_bytes(), &instructions);
            pack.extend_from_slice(&entry);
            response.pack.write(&entry).unwrap();
        }
        let pending = &response.pack.bases.as_ref().unwrap().pending[&BaseReference::Oid(base_oid)];
        assert_eq!(pending.len(), 10);
        assert_eq!(scratch.active_pack_decodes(), 0);
        for entry in [pack_entry(3, &[], &base), pack_entry(3, &[], b"ok")] {
            pack.extend_from_slice(&entry);
            response.pack.write(&entry).unwrap();
        }
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        let mut received = response.pack.finish().unwrap();
        assert_eq!(received.extras.len(), 11);
        received.cache_extras(&scratch, None, |_| {});
        let surplus = Oid::hash_object(ObjectType::Blob, &vec![9; base.len()]).unwrap();
        assert!(scratch.get(surplus).unwrap().is_none());
        assert_eq!(
            verified_extra(&scratch, surplus).data(),
            vec![9; base.len()]
        );
        assert_eq!(scratch.disk_metrics(), (0, 0));
    }

    #[test]
    fn interleaved_delta_chains_apply_each_entry_once() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let size = 256 * 1024;
        let depth = 4;
        let mut bases = [vec![b'a'; size], vec![b'b'; size]];
        let mut oids = bases
            .each_ref()
            .map(|data| Oid::hash_object(ObjectType::Blob, data).unwrap());
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x0a".to_vec();
        for base in &bases {
            pack.extend_from_slice(&pack_entry(3, &[], base));
        }
        for byte in 0..depth {
            for (base, oid) in bases.iter_mut().zip(&mut oids) {
                let instructions = [
                    0x80, 0x80, 0x10, 0x80, 0x80, 0x10, 1, byte, 0xf1, 1, 0xff, 0xff, 3,
                ];
                pack.extend_from_slice(&pack_entry(7, oid.as_bytes(), &instructions));
                base[0] = byte;
                *oid = Oid::hash_object(ObjectType::Blob, base).unwrap();
            }
        }
        DELTA_APPLICATIONS.set(0);
        let mut response = Response::new(
            scratch.clone(),
            vec![oids[0]],
            Box::new(|object| {
                assert_eq!(object.data(), bases[0]);
                Ok(())
            }),
        );
        response.pack.write(&pack).unwrap();
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        let mut received = response.pack.finish().unwrap();
        received.cache_extras(&scratch, None, |_| {});
        assert_eq!(verified_extra(&scratch, oids[1]).data(), bases[1]);
        assert_eq!(DELTA_APPLICATIONS.get(), usize::from(depth) * 2);
    }

    #[test]
    fn publishing_a_verified_extra_preserves_an_already_selected_pack() {
        let root = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(root.path()).unwrap();
        let base = b"base";
        let oid = Oid::hash_object(ObjectType::Blob, base).unwrap();
        let directory = Arc::new(tempfile::tempdir_in(root.path()).unwrap());
        let packed = directory.path().join("pack");
        std::fs::write(&packed, base).unwrap();
        scratch.remember_packed(oid, &directory);
        drop(directory);
        let Some(ReceivedObject::Packed(selected)) = scratch.received(oid) else {
            panic!("expected a native pack source");
        };

        let wanted = Oid::hash_object(ObjectType::Blob, b"wanted").unwrap();
        let mut response = Response::new(scratch.clone(), vec![wanted], Box::new(|_| Ok(())));
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        pack.extend_from_slice(&pack_entry(3, &[], base));
        pack.extend_from_slice(&pack_entry(3, &[], b"wanted"));
        response.pack.write(&pack).unwrap();
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        response
            .pack
            .finish()
            .unwrap()
            .cache_extras(&scratch, None, |_| {});

        assert_eq!(verified_extra(&scratch, oid).data(), base);
        assert_eq!(std::fs::read(selected.path().join("pack")).unwrap(), base);
        drop(selected);
        assert!(!packed.exists());
    }

    #[test]
    fn expanded_instructions_hold_a_slot_even_for_a_small_delta_result() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let base = Oid::hash_object(ObjectType::Blob, b"x").unwrap();
        let expected = vec![b'y'; 40 * 1024];
        let wanted = Oid::hash_object(ObjectType::Blob, &expected).unwrap();
        let mut program = vec![1, 0x80, 0xc0, 2]; // Base size 1 and result size 40 KiB.
        program.extend([1, b'y'].repeat(expected.len()));
        assert!(program.len() > crate::raw_blob::BLOCK);
        let mut response = Response::new(
            scratch.clone(),
            vec![wanted],
            Box::new(|object| {
                assert_eq!(object.data(), expected);
                assert_eq!(scratch.active_pack_decodes(), 1);
                Ok(())
            }),
        );
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        pack.extend_from_slice(&pack_entry(3, &[], b"x"));
        pack.extend_from_slice(&pack_entry(7, base.as_bytes(), &program));
        response.pack.write(&pack).unwrap();
        assert_eq!(scratch.active_pack_decodes(), 0);
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        response.pack.finish().unwrap();
    }

    #[test]
    fn split_pack_entry_keeps_the_blob_within_its_declared_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let data = vec![3; INFLATE_CHUNK + 1];
        let wanted = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let later = Oid::hash_object(ObjectType::Blob, b"later").unwrap();
        let mut header = blob_pack_header(data.len());
        header[11] = 2;
        let mut encoder = flate2::write::ZlibEncoder::new(header, flate2::Compression::fast());
        let first_chunk = 20_000;
        encoder.write_all(&data[..first_chunk]).unwrap();
        encoder.flush().unwrap();
        let split = encoder.get_ref().len();
        encoder.write_all(&data[first_chunk..]).unwrap();
        let mut pack = encoder.finish().unwrap();
        pack.extend_from_slice(&pack_entry(3, &[], b"later"));
        let mut response = Response::new(
            scratch,
            vec![wanted, later],
            Box::new(move |object| {
                if object.oid() == wanted {
                    assert_eq!(object.data(), data);
                    assert!(object.data.capacity() <= data.len());
                } else {
                    assert_eq!(object.data(), b"later");
                }
                Ok(())
            }),
        );
        response.pack.write(&pack[..split]).unwrap();
        response.pack.write(&pack[split..]).unwrap();
        response.pack.write(&Sha1::digest(&pack)).unwrap();
        response.pack.finish().unwrap();
    }

    #[test]
    fn large_full_object_is_published_before_the_trailer_without_staging() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let data = vec![3; 1025 * 1024];
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let header = blob_pack_header(data.len());
        let mut encoder = flate2::write::ZlibEncoder::new(header, flate2::Compression::none());
        encoder.write_all(&data).unwrap();
        let packet = encoder.finish().unwrap();
        let trailer = Sha1::digest(&packet);
        let delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let published = delivered.clone();
        let mut response = Response::new(
            scratch,
            vec![oid],
            Box::new(move |object| {
                assert_eq!(object.oid(), oid);
                assert_eq!(object.data(), data);
                published.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }),
        );
        // Leave the first entry's size field incomplete before supplying a
        // much larger transport chunk containing its compressed payload.
        let split = 13;
        response.pack.write(&packet[..split]).unwrap();
        for chunk in packet[split..].chunks(4096) {
            response.pack.write(chunk).unwrap();
        }
        assert!(response.pack.pending.capacity() <= TRAILER);
        assert!(delivered.load(std::sync::atomic::Ordering::Relaxed));
        response.pack.write(&trailer).unwrap();
        response.pack.finish().unwrap();
    }

    #[test]
    fn rejects_excess_pack_trailer_before_finish() {
        let temporary = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(temporary.path()).unwrap();
        let wanted = Oid::hash_object(ObjectType::Blob, b"x").unwrap();
        let mut encoder = flate2::write::ZlibEncoder::new(
            b"PACK\0\0\0\x02\0\0\0\x01\x31".to_vec(),
            flate2::Compression::fast(),
        );
        encoder.write_all(b"x").unwrap();
        let entry = encoder.finish().unwrap();
        for separate in [false, true] {
            let mut response = Response::new(scratch.clone(), vec![wanted], Box::new(|_| Ok(())));
            let extra_trailer = [0; TRAILER + 1];
            let error = if separate {
                response.pack.write(&entry).unwrap();
                response.pack.write(&extra_trailer)
            } else {
                response
                    .pack
                    .write(&[entry.as_slice(), &extra_trailer].concat())
            };
            assert!(error.unwrap_err().to_string().contains("trailer"));
        }
    }

    #[test]
    fn snapshot_rejects_an_invalid_shallow_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let mut response = Response::for_materialization(scratch, Oid::zero());
        let error = response
            .write_http(b"0011shallow-info\n0014shallow invalid\n")
            .unwrap_err();
        assert!(error.to_string().contains("invalid Git shallow section"));
    }

    #[test]
    fn streaming_fetch_keeps_server_rejection_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let reason = "upload-pack denied the requested object";
        let packet = format!("{:04x}ERR {reason}", 8 + reason.len());
        for prefix in [b"".as_slice(), b"0011shallow-info\n", b"000dpackfile\n"] {
            let mut response = Response::new(
                scratch.clone(),
                vec![Oid::zero()],
                Box::new(|_| panic!("a rejected fetch published an object")),
            );
            response.write_http(prefix).unwrap();
            let error = packet
                .as_bytes()
                .chunks(2)
                .find_map(|chunk| response.write_http(chunk).err())
                .expect("expected upload-pack rejection");
            assert!(error.to_string().contains(reason), "{error:#}");
        }
    }
}
