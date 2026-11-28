//! Incremental pkt-line and PACK decoding for the reusable fetch transports.
use crate::object;
use anyhow::{bail, ensure, Context, Result};
use flate2::{Decompress, FlushDecompress, Status};
use git2::{ObjectType, Oid};
use sha1_checked::{Digest, Sha1};
use std::{
    collections::HashMap,
    io::{BufRead, Read, Write},
    mem::MaybeUninit,
    ops::Range,
    sync::Arc,
};

mod delta;

const TRAILER: usize = 20;
const ACTIVE_BASE_LIMIT: usize = 16 * 1024 * 1024;

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

    pub(crate) fn shared_data(&self) -> Arc<Vec<u8>> {
        self.data.clone()
    }
}

#[derive(Clone)]
pub(crate) enum PackedObject {
    Decoded(VerifiedObject),
    Blob {
        oid: Oid,
        size: usize,
        zlib: Arc<Vec<u8>>,
    },
}

impl PackedObject {
    fn retain(object: VerifiedObject, original: Option<&[u8]>) -> Result<Self> {
        if object.kind != ObjectType::Blob {
            return Ok(Self::Decoded(object));
        }
        let zlib = if let Some(original) = original {
            original.to_vec()
        } else {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            for bytes in object.data.chunks(32 * 1024) {
                object::check_transfer()?;
                encoder.write_all(bytes)?;
            }
            encoder.finish()?
        };
        Ok(Self::Blob {
            oid: object.oid,
            size: object.data.len(),
            zlib: Arc::new(zlib),
        })
    }

    pub(crate) fn oid(&self) -> Oid {
        match self {
            Self::Decoded(object) => object.oid,
            Self::Blob { oid, .. } => *oid,
        }
    }

    pub(crate) fn encoded_capacity(&self) -> Option<usize> {
        match self {
            Self::Decoded(_) => None,
            Self::Blob { zlib, .. } => Some(zlib.capacity()),
        }
    }

    pub(crate) fn decode(&self) -> Result<VerifiedObject> {
        let (oid, size, zlib) = match self {
            Self::Decoded(object) => return Ok(object.clone()),
            Self::Blob { oid, size, zlib } => (oid, size, zlib),
        };
        let mut input = flate2::read::ZlibDecoder::new(zlib.as_slice());
        let mut data = Vec::new();
        data.try_reserve_exact(*size)?;
        while data.len() < *size {
            object::check_transfer()?;
            let count = (*size - data.len()).min(32 * 1024);
            ensure!(
                (&mut input).take(count as u64).read_to_end(&mut data)? == count,
                "retained Git blob is truncated"
            );
        }
        ensure!(input.read(&mut [0])? == 0, "retained Git blob is oversized");
        Ok(VerifiedObject {
            oid: *oid,
            kind: ObjectType::Blob,
            data: Arc::new(data),
        })
    }
}

pub(crate) type Publish = Box<dyn FnMut(VerifiedObject) -> Result<()> + Send>;

pub(crate) enum Placement {
    InPack,
    InOutput,
    Retry(anyhow::Error),
}

pub(crate) type VisitRange<'a> =
    dyn FnMut(Range<usize>, &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> + 'a;
pub(crate) type DecodeWithBase<'a> =
    dyn FnMut(ObjectType, usize, &mut VisitRange<'_>) -> Result<VerifiedObject> + 'a;

pub(crate) trait PackOutput: Send {
    fn place(&mut self, object: &VerifiedObject, as_base: bool) -> Result<Placement>;
    fn with_base(
        &mut self,
        oid: Oid,
        decode: &mut DecodeWithBase<'_>,
    ) -> Result<Option<VerifiedObject>>;
}

pub(crate) struct Received {
    pub(crate) bytes: u64,
    pub(crate) incomplete: bool,
    pub(crate) objects: HashMap<Oid, PackedObject>,
    pub(crate) object_errors: HashMap<Oid, anyhow::Error>,
}

pub(crate) struct Response {
    pack: Pack,
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
    Pack,
    Done,
}

impl Response {
    pub(crate) fn new(wants: Vec<Oid>, publish: Publish) -> Self {
        Self {
            pack: Pack {
                allow_omission: wants.len() > 1,
                requested: wants.clone(),
                wants,
                publish,
                object_errors: HashMap::new(),
                bytes: 0,
                pending_input: Vec::new(),
                objects: PackObjects::default(),
                eager: false,
                consumed: 0,
                checksum: Checksum::default(),
                progress_started: None,
                count: None,
                entries: 0,
                entry: None,
            },
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

    pub(crate) fn eager(oid: Oid, output: impl PackOutput + 'static) -> Self {
        let mut response = Self::new(vec![oid], Box::new(|_| Ok(())));
        response.pack.eager = true;
        response.with_output(output)
    }

    pub(crate) fn with_output(mut self, output: impl PackOutput + 'static) -> Self {
        self.pack.objects.output = Some(Box::new(output));
        self
    }

    fn done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// Leave bytes after the response in the caller's buffer. Native helpers
    /// have a separate response-end marker. Direct SSH uses this same framer.
    fn feed(&mut self, mut input: &[u8]) -> Result<usize> {
        let len = input.len();
        while !input.is_empty() && !self.done() {
            object::check_transfer()?;
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
                    1 if matches!(self.phase, Phase::Shallow) => self.phase = Phase::Section,
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
                    1 => self.pack.write(payload)?,
                    2 => (),
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
        if matches!(self.phase, Phase::Section | Phase::Shallow) || self.channel == Some(b'E') {
            crate::transport::check_packet_error(&self.text)?;
        }
        match self.phase {
            Phase::Section => match crate::transport::packet_text(&self.text) {
                b"packfile" => self.phase = Phase::Pack,
                b"shallow-info" => self.phase = Phase::Shallow,
                _ => bail!("invalid Git fetch response section"),
            },
            Phase::Shallow => ensure!(
                self.text.starts_with(b"shallow ") || self.text.starts_with(b"unshallow "),
                "invalid Git shallow section"
            ),
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

struct Pack {
    wants: Vec<Oid>,
    requested: Vec<Oid>,
    allow_omission: bool,
    publish: Publish,
    object_errors: HashMap<Oid, anyhow::Error>,
    bytes: u64,
    pending_input: Vec<u8>,
    objects: PackObjects,
    eager: bool,
    consumed: usize,
    checksum: Checksum,
    progress_started: Option<std::time::Instant>,
    count: Option<usize>,
    entries: usize,
    entry: Option<Entry>,
}

struct Entry {
    kind: Option<ObjectType>,
    base: Option<Base>,
    offset: usize,
    compressed_start: usize,
    size: usize,
    inflater: Decompress,
    hash: Option<Sha1>,
    data: Vec<u8>,
}

impl Pack {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes += bytes.len() as u64;
        self.checksum.update(bytes);
        let mut pending = std::mem::take(&mut self.pending_input);
        let consumed = if pending.is_empty() {
            let consumed = self.inflate_entries(bytes)?;
            pending.try_reserve(bytes.len() - consumed)?;
            pending.extend_from_slice(&bytes[consumed..]);
            consumed
        } else {
            // Join boundary fragments. The normal path decodes directly
            // from the already available transport buffer.
            pending.try_reserve(bytes.len())?;
            pending.extend_from_slice(bytes);
            let consumed = self.inflate_entries(&pending)?;
            pending.drain(..consumed);
            consumed
        };
        self.consumed += consumed;
        self.pending_input = pending;
        if self.count == Some(self.entries) {
            ensure!(
                self.pending_input.len() <= TRAILER,
                "oversized Git pack trailer"
            );
        }
        if let Some(count) = self.count {
            crate::terminal_ui::transfer_progress(
                &mut self.progress_started,
                self.bytes,
                self.entries,
                count,
            );
        }
        Ok(())
    }

    fn inflate_entries(&mut self, input: &[u8]) -> Result<usize> {
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
                position = 12;
                count
            }
        };
        while self.entries < count {
            object::check_transfer()?;
            if self.entry.is_none() {
                let bytes = &input[position..];
                let Some((kind, size, length)) = entry_header(bytes)? else {
                    break;
                };
                let offset = self.consumed + position;
                let (base, base_length) = if kind.is_some() {
                    (None, 0)
                } else {
                    let Some((base, base_length)) =
                        read_base((bytes[0] >> 4) & 7, &bytes[length..], offset)?
                    else {
                        break;
                    };
                    (Some(base), base_length)
                };
                position += length + base_length;
                let mut data = Vec::new();
                data.try_reserve_exact(size)?;
                self.entry = Some(Entry {
                    kind,
                    base,
                    offset,
                    compressed_start: self.consumed + position,
                    size,
                    inflater: Decompress::new(true),
                    hash: kind.map(|kind| object::object_hash(kind, size)),
                    data,
                });
            }
            let entry = self.entry.as_mut().expect("entry initialized");
            let old_size = entry.data.len();
            let old_input = entry.inflater.total_in();
            let output_len = (entry.size - old_size).min(32768);
            let mut overflow = [MaybeUninit::uninit()];
            let output = if output_len == 0 {
                &mut overflow[..]
            } else {
                &mut entry.data.spare_capacity_mut()[..output_len]
            };
            let status = entry.inflater.decompress_uninit(
                &input[position..],
                output,
                FlushDecompress::None,
            )?;
            let consumed = (entry.inflater.total_in() - old_input) as usize;
            let used = (entry.inflater.total_out() as usize) - old_size;
            ensure!(used <= output_len, "oversized Git pack object");
            position += consumed;
            // The inflater has initialized exactly the prefix it reports.
            unsafe { entry.data.set_len(old_size + used) };
            if let Some(hash) = &mut entry.hash {
                hash.update(&entry.data[old_size..]);
            }
            if status == Status::StreamEnd {
                let entry = self.entry.take().expect("entry initialized");
                ensure!(entry.data.len() == entry.size, "truncated Git pack object");
                self.entries += 1;
                let wants = &mut self.wants;
                let publish = &mut self.publish;
                let eager = self.eager;
                let errors = &mut self.object_errors;
                let mut match_want = |object: &VerifiedObject, placement: Placement| {
                    let wanted = wants
                        .iter()
                        .position(|&oid| oid == object.oid)
                        .map(|index| wants.swap_remove(index));
                    // A retry applies to this object. The response still
                    // needs its checksum, framing and transport verdict.
                    if let Placement::Retry(error) = placement {
                        if eager {
                            return Err(error);
                        }
                        if let Some(oid) = wanted {
                            errors.insert(oid, error);
                        }
                        return Ok(false);
                    }
                    if wanted.is_some() {
                        publish(object.clone())?;
                    }
                    Ok(matches!(placement, Placement::InOutput))
                };
                if !self.eager && count == 1 && entry.base.is_none() {
                    let object = entry.finish_full()?;
                    ensure!(
                        self.requested.contains(&object.oid),
                        "single-object pack supplied an unexpected OID"
                    );
                    match_want(&object, self.objects.place(&object, false)?)?;
                } else {
                    let original = if entry.base.is_none() {
                        entry
                            .compressed_start
                            .checked_sub(self.consumed)
                            .and_then(|start| input.get(start..position))
                    } else {
                        None
                    };
                    self.objects.accept(
                        entry,
                        original,
                        |oid| self.eager || self.requested.contains(oid),
                        match_want,
                    )?;
                }
            } else if consumed == 0 && used == 0 {
                break;
            }
        }
        Ok(position)
    }

    fn finish(mut self) -> Result<Received> {
        crate::terminal_ui::transfer_finished(self.progress_started);
        self.checksum.finish()?;
        ensure!(
            self.count == Some(self.entries) && self.pending_input.len() == TRAILER,
            "incomplete Git pack entries"
        );
        ensure!(
            self.objects.pending.is_empty(),
            "Git pack omitted a delta base"
        );
        if !self.eager {
            self.objects
                .objects
                .retain(|oid, _| !self.requested.contains(oid));
        }
        if let Some(oid) = self.wants.first().filter(|_| !self.allow_omission) {
            bail!("remote did not supply requested object {oid}");
        }
        Ok(Received {
            objects: self.objects.objects,
            incomplete: !self.wants.is_empty(),
            bytes: self.bytes,
            object_errors: self.object_errors,
        })
    }
}

impl Entry {
    fn finish_full(self) -> Result<VerifiedObject> {
        Ok(VerifiedObject {
            oid: Oid::from_bytes(&object::finish_sha1(self.hash.expect("full object hash"))?)?,
            kind: self.kind.expect("full object type"),
            data: Arc::new(self.data),
        })
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Base {
    Offset(usize),
    Id(Oid),
}

#[derive(Default)]
struct PackObjects {
    output: Option<Box<dyn PackOutput>>,
    offsets: HashMap<usize, Oid>,
    objects: HashMap<Oid, PackedObject>,
    decoded_base: Option<VerifiedObject>,
    pending: HashMap<Base, Vec<(usize, Vec<u8>)>>,
}

impl PackObjects {
    fn place(&mut self, object: &VerifiedObject, as_base: bool) -> Result<Placement> {
        self.output
            .as_mut()
            .map_or(Ok(Placement::InPack), |output| {
                output.place(object, as_base)
            })
    }

    fn memory_base(&mut self, oid: Oid) -> Result<Option<VerifiedObject>> {
        if let Some(decoded) = self
            .decoded_base
            .as_ref()
            .filter(|object| object.oid == oid)
        {
            return Ok(Some(decoded.clone()));
        }
        if let Some(object) = self.objects.get(&oid) {
            let decoded = object.decode()?;
            if decoded.kind == ObjectType::Blob && decoded.data.capacity() <= ACTIVE_BASE_LIMIT {
                self.decoded_base = Some(decoded.clone());
            }
            return Ok(Some(decoded));
        }
        Ok(None)
    }

    fn apply(&mut self, base: Base, instructions: &[u8]) -> Result<Option<VerifiedObject>> {
        let oid = match base {
            Base::Offset(offset) => {
                let Some(&oid) = self.offsets.get(&offset) else {
                    return Ok(None);
                };
                oid
            }
            Base::Id(oid) => oid,
        };
        if let Some(base) = self.memory_base(oid)? {
            return apply_delta(
                base.kind,
                base.data.len(),
                instructions,
                BaseBytes::Memory(&base.data),
            )
            .map(Some);
        }
        let Some(output) = &mut self.output else {
            return Ok(None);
        };
        output.with_base(oid, &mut |kind, len, read| {
            apply_delta(kind, len, instructions, BaseBytes::External(read))
        })
    }

    fn accept(
        &mut self,
        entry: Entry,
        mut original: Option<&[u8]>,
        selected: impl Fn(&Oid) -> bool,
        mut publish: impl FnMut(&VerifiedObject, Placement) -> Result<bool>,
    ) -> Result<()> {
        let mut offset = entry.offset;
        let mut object = match entry.base {
            None => entry.finish_full()?,
            Some(reference) => {
                let Some(object) = self.apply(reference, &entry.data)? else {
                    self.pending
                        .entry(reference)
                        .or_default()
                        .push((offset, entry.data));
                    return Ok(());
                };
                object
            }
        };
        // Resolve waiting deltas one at a time so fan-out from a shared
        // reference base cannot leave many newly inflated blobs in memory.
        let mut ready = Vec::new();
        loop {
            let oid = object.oid;
            let encoding = original.take();
            let placement = if selected(&oid) {
                self.place(&object, true)?
            } else {
                Placement::InPack
            };
            let placed = publish(&object, placement)?;
            self.offsets.insert(offset, oid);
            if !placed {
                let stored = self.objects.entry(oid);
                if let std::collections::hash_map::Entry::Vacant(entry) = stored {
                    if object.kind == ObjectType::Blob
                        && object.data.capacity() <= ACTIVE_BASE_LIMIT
                    {
                        self.decoded_base = Some(object.clone());
                    }
                    entry.insert(PackedObject::retain(object, encoding)?);
                }
            }
            for reference in [Base::Id(oid), Base::Offset(offset)] {
                if let Some(pending) = self.pending.remove(&reference) {
                    ready.extend(
                        pending
                            .into_iter()
                            .map(|(offset, bytes)| (offset, oid, bytes)),
                    );
                }
            }
            let Some((next_offset, base, instructions)) = ready.pop() else {
                return Ok(());
            };
            offset = next_offset;
            object = self
                .apply(Base::Id(base), &instructions)?
                .context("missing resolved delta base")?;
        }
    }
}

enum BaseBytes<'a, 'b> {
    Memory(&'a [u8]),
    External(&'a mut VisitRange<'b>),
}

fn apply_delta(
    kind: ObjectType,
    base_size: usize,
    instructions: &[u8],
    mut base: BaseBytes<'_, '_>,
) -> Result<VerifiedObject> {
    let delta = delta::Delta::parse(instructions)?;
    let size = delta.target_size();
    let mut data = Vec::new();
    data.try_reserve_exact(size)?;
    let mut hash = object::object_hash(kind, size);
    let mut append = |bytes: &[u8]| {
        hash.update(bytes);
        data.extend_from_slice(bytes);
        Ok(())
    };
    delta.apply(base_size, |part| {
        object::check_transfer()?;
        match part {
            delta::Part::Copy(range) => match &mut base {
                BaseBytes::Memory(bytes) => append(&bytes[range]),
                BaseBytes::External(read) => read(range, &mut append),
            },
            delta::Part::Bytes(bytes) => append(bytes),
        }
    })?;
    Ok(VerifiedObject {
        oid: Oid::from_bytes(&object::finish_sha1(hash)?)?,
        kind,
        data: Arc::new(data),
    })
}

fn read_base(kind: u8, bytes: &[u8], offset: usize) -> Result<Option<(Base, usize)>> {
    if kind == 7 {
        return bytes
            .get(..20)
            .map(|oid| Ok((Base::Id(Oid::from_bytes(oid)?), 20)))
            .transpose();
    }
    let Some((&first, rest)) = bytes.split_first() else {
        return Ok(None);
    };
    let mut distance = usize::from(first & 127);
    let mut continued = first & 128 != 0;
    let mut length = 1;
    while continued {
        let Some(&byte) = rest.get(length - 1) else {
            return Ok(None);
        };
        distance = distance
            .checked_add(1)
            .and_then(|value| value.checked_mul(128))
            .and_then(|value| value.checked_add(usize::from(byte & 127)))
            .context("Git pack delta base offset overflow")?;
        length += 1;
        continued = byte & 128 != 0;
    }
    let base = offset
        .checked_sub(distance)
        .filter(|&base| base >= 12 && base < offset)
        .context("invalid Git pack delta base offset")?;
    Ok(Some((Base::Offset(base), length)))
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

fn entry_header(bytes: &[u8]) -> Result<Option<(Option<ObjectType>, usize, usize)>> {
    let Some((&first, rest)) = bytes.split_first() else {
        return Ok(None);
    };
    let kind = match (first >> 4) & 7 {
        1 => Some(ObjectType::Commit),
        2 => Some(ObjectType::Tree),
        3 => Some(ObjectType::Blob),
        4 => Some(ObjectType::Tag),
        6 | 7 => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_blob::RawBlobs;
    use std::io::Write;

    #[test]
    fn neighboring_deltas_share_one_decoded_base() {
        let data = Arc::new(vec![b'x'; 32]);
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let stored = PackedObject::retain(
            VerifiedObject {
                oid,
                kind: ObjectType::Blob,
                data,
            },
            None,
        )
        .unwrap();
        let mut pack = PackObjects::default();
        pack.objects.insert(oid, stored);
        let first = pack.memory_base(oid).unwrap().expect("packed base");
        let second = pack.memory_base(oid).unwrap().expect("packed base");
        assert!(Arc::ptr_eq(&first.data, &second.data));
    }

    #[test]
    fn streamed_delta_survives_eviction_and_copy_up() {
        const SIZE: usize = 65_537;
        let directory = tempfile::tempdir().unwrap();
        let cache = RawBlobs::new(directory.path()).unwrap();
        let blob = |byte| {
            let data = Arc::new(vec![byte; SIZE]);
            VerifiedObject {
                oid: Oid::hash_object(ObjectType::Blob, &data).unwrap(),
                kind: ObjectType::Blob,
                data,
            }
        };

        let base = blob(b'a');
        let target = Oid::hash_object(ObjectType::Blob, b"aaaaa").unwrap();
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        let base_offset = pack.len();
        let encode = |pack: &mut Vec<u8>, data: &[u8]| {
            let mut output = flate2::write::ZlibEncoder::new(pack, flate2::Compression::default());
            output.write_all(data).unwrap();
            output.finish().unwrap();
        };
        pack.extend_from_slice(&[0xb1, 0x80, 0x20]);
        encode(&mut pack, base.data());
        let distance = u8::try_from(pack.len() - base_offset).unwrap();
        assert!(distance < 128);
        pack.extend_from_slice(&[0x66, distance]);
        encode(&mut pack, &[0x81, 0x80, 0x04, 5, 0x90, 5]);
        let checksum = Sha1::digest(&pack);
        pack.extend_from_slice(&checksum);
        let mut wire = format!("000dpackfile\n{:04x}\x01", pack.len() + 5).into_bytes();
        wire.extend(pack);
        wire.extend_from_slice(b"0000");

        let source_oid = base.oid();
        let source = cache.clone();
        let writable = directory.path().join("writable");
        let mut response = Response::new(
            vec![source_oid, target],
            Box::new(move |object| {
                if object.oid() == source_oid {
                    for byte in b'b'..=b'i' {
                        source.insert_verified_blob(&blob(byte))?;
                    }
                    assert_eq!(source.write_metrics().1, SIZE as u64);
                    source.promote(source_oid, &writable, true, |_| Ok(()))?;
                    std::fs::write(&writable, b"changed")?;
                }
                Ok(())
            }),
        )
        .with_output(cache.pack_output());
        response.write_http(&wire).unwrap();
        assert!(!response.finish_http().unwrap().incomplete);
        assert_eq!(
            cache.get(target).unwrap().unwrap().read_all().unwrap(),
            b"aaaaa"
        );
        assert_eq!(cache.tested_authenticated_bytes(), 65_536);
        assert!(cache.get(source_oid).unwrap().is_none());
    }

    #[test]
    fn streaming_fetch_keeps_server_rejection_diagnostic() {
        let reason = "upload-pack denied the requested object";
        let packet = format!("{:04x}ERR {reason}", 8 + reason.len());
        for prefix in [b"".as_slice(), b"0011shallow-info\n", b"000dpackfile\n"] {
            let mut response = Response::new(
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
