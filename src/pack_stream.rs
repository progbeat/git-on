//! Incremental pkt-line and PACK decoding for the reusable fetch transports.
use crate::{object, raw_blob::RawBlobs, spool::Spool};
use anyhow::{bail, ensure, Result};
use flate2::{Decompress, FlushDecompress, Status};
use git2::{ObjectType, Oid};
use sha1_checked::{Digest, Sha1};
use std::{
    io::{BufRead, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MEMORY_PACK: usize = 1024 * 1024;
const TRAILER: usize = 20;

pub(crate) struct VerifiedObject {
    oid: Oid,
    kind: ObjectType,
    data: Vec<u8>,
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

    pub(crate) fn into_data(self) -> Vec<u8> {
        self.data
    }
}

pub(crate) type Publish = Box<dyn FnMut(VerifiedObject) -> Result<()> + Send>;
type Required = Box<dyn Fn(&[Oid]) -> bool + Send>;

pub(crate) struct Received {
    pub(crate) fallback: Option<Spool>,
    pub(crate) bytes: u64,
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
    pub(crate) fn new(scratch: RawBlobs, wants: Vec<Oid>, publish: Publish) -> Self {
        Self {
            pack: Pack {
                scratch,
                stop: None,
                wants,
                publish,
                required: None,
                bytes: 0,
                compressed: Vec::new(),
                spool: None,
                native_fallback: false,
                checksum: Checksum::default(),
                position: 0,
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

    pub(crate) fn set_stop(&mut self, stop: Option<Arc<AtomicBool>>) {
        self.pack.stop = stop;
    }

    pub(crate) fn set_required(&mut self, required: impl Fn(&[Oid]) -> bool + Send + 'static) {
        self.pack.required = Some(Box::new(required));
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

pub(crate) fn index(odb: &git2::Odb<'_>, pack: &Spool) -> Result<()> {
    let mut failure = None;
    let mut writer = odb.packwriter()?;
    writer.progress(|_| match object::check_transfer() {
        Ok(()) => true,
        Err(error) => {
            failure = Some(error);
            false
        }
    });
    let result = (|| {
        let mut offset = 0;
        let mut bytes = [0; 65536];
        let size = pack.len()?;
        while offset < size {
            object::check_transfer()?;
            let count = (size - offset).min(bytes.len() as u64) as usize;
            pack.read_exact_at(&mut bytes[..count], offset)?;
            writer.write_all(&bytes[..count])?;
            offset += count as u64;
        }
        writer.commit()?;
        Ok(())
    })();
    drop(writer);
    failure.map_or(result, Err)
}

struct Pack {
    scratch: RawBlobs,
    stop: Option<Arc<AtomicBool>>,
    wants: Vec<Oid>,
    publish: Publish,
    required: Option<Required>,
    bytes: u64,
    compressed: Vec<u8>,
    spool: Option<Spool>,
    native_fallback: bool,
    checksum: Checksum,
    position: usize,
    count: Option<usize>,
    entries: usize,
    entry: Option<Entry>,
}

struct Entry {
    kind: ObjectType,
    size: usize,
    inflater: Decompress,
    hash: Sha1,
    data: Vec<u8>,
    _decode: Option<crate::raw_blob::PackDecode>,
}

impl Pack {
    fn check_transfer(&self) -> Result<()> {
        if self
            .stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
        {
            return Err(object::OperationCancelled.into());
        }
        object::check_transfer()
    }

    fn spool_pack(&mut self) -> Result<()> {
        if self.spool.is_none() {
            let file = self.scratch.scratch_file()?;
            (&file).write_all(&self.compressed)?;
            self.spool = Some(file);
        }
        Ok(())
    }

    fn use_fallback(&mut self) -> Result<()> {
        self.spool_pack()?;
        self.compressed = Vec::new();
        self.entry = None;
        self.native_fallback = true;
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.native_fallback && self.count == Some(self.entries) {
            let unread = self.compressed.len() - self.position;
            ensure!(
                bytes.len() <= TRAILER - unread,
                "oversized Git pack trailer"
            );
        }
        self.bytes += bytes.len() as u64;
        self.checksum.update(bytes);
        if self.spool.is_none() && bytes.len() > MEMORY_PACK - self.compressed.len() {
            self.spool_pack()?;
        }
        if let Some(file) = &self.spool {
            (&*file).write_all(bytes)?;
            if self.native_fallback {
                return Ok(());
            }
            // Keep the full compressed pack available for delta fallback.
            // After spilling the prefix, only unread bytes feed the inflater.
            self.compressed.drain(..self.position);
            self.position = 0;
        }
        self.compressed.extend_from_slice(bytes);
        self.inflate_entries()
    }

    fn inflate_entries(&mut self) -> Result<()> {
        let count = match self.count {
            Some(count) => count,
            None if self.compressed.len() < 12 => return Ok(()),
            None => {
                let header = &self.compressed[..12];
                ensure!(
                    &header[..4] == b"PACK"
                        && matches!(u32::from_be_bytes(header[4..8].try_into()?), 2 | 3),
                    "invalid Git pack header"
                );
                let count = u32::from_be_bytes(header[8..].try_into()?) as usize;
                self.count = Some(count);
                self.position = 12;
                count
            }
        };
        let mut output = [0; 32768];
        while self.entries < count {
            self.check_transfer()?;
            if self.entry.is_none() {
                let Some((kind, size, length)) = entry_header(&self.compressed[self.position..])?
                else {
                    break;
                };
                let Some(kind) = kind else {
                    return self.use_fallback();
                };
                let decode = (kind == ObjectType::Blob && size > crate::raw_blob::BLOCK)
                    .then(|| {
                        self.scratch.reserve_pack_decode(
                            || {
                                self.required
                                    .as_ref()
                                    .is_none_or(|required| required(&self.wants))
                            },
                            || self.check_transfer(),
                        )
                    })
                    .transpose()?;
                self.position += length;
                self.entry = Some(Entry {
                    kind,
                    size,
                    inflater: Decompress::new(true),
                    hash: object::object_hash(kind, size),
                    data: Vec::new(),
                    _decode: decode,
                });
            }
            let entry = self.entry.as_mut().expect("entry initialized");
            let before_in = entry.inflater.total_in();
            let before_out = entry.inflater.total_out();
            let status = entry.inflater.decompress(
                &self.compressed[self.position..],
                &mut output,
                FlushDecompress::None,
            )?;
            let consumed = (entry.inflater.total_in() - before_in) as usize;
            let used = (entry.inflater.total_out() - before_out) as usize;
            self.position += consumed;
            ensure!(
                used <= entry.size - entry.data.len(),
                "oversized Git pack object"
            );
            entry.data.try_reserve(used)?;
            entry.hash.update(&output[..used]);
            entry.data.extend_from_slice(&output[..used]);
            if status == Status::StreamEnd {
                let entry = self.entry.take().expect("entry initialized");
                ensure!(entry.data.len() == entry.size, "truncated Git pack object");
                self.entries += 1;
                let oid = Oid::from_bytes(&object::finish_sha1(entry.hash)?)?;
                let Some(want) = self.wants.iter().position(|&item| item == oid) else {
                    // Preserve bases or other extra entries through the pack
                    // decoder instead of discarding a received object.
                    return self.use_fallback();
                };
                self.wants.swap_remove(want);
                (self.publish)(VerifiedObject {
                    oid,
                    kind: entry.kind,
                    data: entry.data,
                })?;
            } else if consumed == 0 && used == 0 {
                break;
            }
        }
        ensure!(
            self.entries < count || self.compressed.len() - self.position <= TRAILER,
            "oversized Git pack trailer"
        );
        Ok(())
    }

    fn finish(self) -> Result<Received> {
        self.checksum.finish()?;
        if !self.native_fallback {
            ensure!(
                self.count == Some(self.entries)
                    && self.compressed.len() == self.position + TRAILER,
                "incomplete Git pack entries"
            );
            if let Some(oid) = self.wants.first() {
                bail!("remote did not supply requested object {oid}");
            }
        }
        Ok(Received {
            fallback: if self.native_fallback {
                self.spool
            } else {
                None
            },
            bytes: self.bytes,
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

    #[test]
    fn large_full_object_is_published_before_the_staged_pack_trailer() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let data = vec![3; MEMORY_PACK + 1024];
        let oid = Oid::hash_object(ObjectType::Blob, &data).unwrap();
        let mut packet = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
        let mut remaining = data.len() >> 4;
        let mut byte = 0x30 | (data.len() & 15) as u8;
        while remaining != 0 {
            packet.push(byte | 0x80);
            byte = (remaining & 0x7f) as u8;
            remaining >>= 7;
        }
        packet.push(byte);
        let mut encoder = flate2::write::ZlibEncoder::new(packet, flate2::Compression::none());
        encoder.write_all(&data).unwrap();
        let packet = encoder.finish().unwrap();
        assert!(packet.len() > MEMORY_PACK);
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
        for chunk in packet.chunks(4096) {
            response.pack.write(chunk).unwrap();
        }
        assert!(delivered.load(std::sync::atomic::Ordering::Relaxed));
        response.pack.write(&trailer).unwrap();
        assert!(response.pack.finish().unwrap().fallback.is_none());
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
