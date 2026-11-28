//! Bounds-checked XDR primitives for the local ONC RPC service.
use crate::raw_blob::ReadData;

pub(super) const MAX_RECORD: usize = 2 * 1024 * 1024;
pub(super) const MAX_DATA: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct InvalidXdr;
pub(super) type Decode<T> = Result<T, InvalidXdr>;

#[derive(Clone)]
pub(super) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(super) fn new(rest: &'a [u8]) -> Self {
        Self { rest }
    }

    pub(super) fn take(&mut self, len: usize) -> Decode<&'a [u8]> {
        let (value, rest) = self.rest.split_at_checked(len).ok_or(InvalidXdr)?;
        self.rest = rest;
        Ok(value)
    }

    pub(super) fn fixed<const N: usize>(&mut self) -> Decode<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| InvalidXdr)
    }

    pub(super) fn u32(&mut self) -> Decode<u32> {
        Ok(u32::from_be_bytes(self.fixed()?))
    }

    pub(super) fn u64(&mut self) -> Decode<u64> {
        Ok(u64::from_be_bytes(self.fixed()?))
    }

    pub(super) fn boolean(&mut self) -> Decode<bool> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(InvalidXdr),
        }
    }

    pub(super) fn count(&mut self, max: usize) -> Decode<usize> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(InvalidXdr);
        }
        Ok(len)
    }

    pub(super) fn opaque(&mut self, max: usize) -> Decode<&'a [u8]> {
        let len = self.count(max)?;
        let padded = len.checked_next_multiple_of(4).ok_or(InvalidXdr)?;
        let data = self.take(padded)?;
        Ok(&data[..len])
    }

    pub(super) fn remaining(&self) -> &'a [u8] {
        self.rest
    }

    pub(super) fn finish(&self) -> Decode<()> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(InvalidXdr)
        }
    }
}

#[derive(Default)]
pub(super) struct Writer {
    bytes: Vec<u8>,
    payload: Option<(usize, ReadData)>,
    extra_payloads: Vec<(usize, ReadData)>,
    payload_len: usize,
}

impl Writer {
    pub(super) fn len(&self) -> usize {
        self.bytes.len() + self.payload_len
    }

    pub(super) fn bytes(&self) -> &[u8] {
        debug_assert!(self.payload.is_none());
        &self.bytes
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        debug_assert!(self.payload.is_none());
        self.bytes
    }

    pub(super) fn truncate(&mut self, len: usize) {
        self.bytes.truncate(self.inline_offset(len));
        while self
            .extra_payloads
            .last()
            .is_some_and(|(start, _)| *start >= len)
        {
            self.payload_len -= self.extra_payloads.pop().unwrap().1.len();
        }
        if self
            .payload
            .as_ref()
            .is_some_and(|(start, _)| *start >= len)
        {
            self.payload_len -= self.payload.take().unwrap().1.len();
        }
    }

    fn inline_offset(&self, offset: usize) -> usize {
        let mut inline = offset;
        for (start, data) in self.payloads() {
            if offset < *start + data.len() {
                debug_assert!(offset <= *start, "XDR writer cannot patch attached data");
                break;
            }
            inline -= data.len();
        }
        inline
    }

    pub(super) fn fixed(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.fixed(&value.to_be_bytes());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.fixed(&value.to_be_bytes());
    }

    pub(super) fn boolean(&mut self, value: bool) {
        self.u32(u32::from(value));
    }

    pub(super) fn opaque(&mut self, value: &[u8]) {
        self.u32(value.len().try_into().expect("bounded XDR output"));
        self.fixed(value);
        self.pad();
    }

    pub(super) fn opaque_data(&mut self, value: ReadData) {
        self.u32(value.len().try_into().expect("bounded XDR output"));
        if !value.is_empty() {
            let start = self.len();
            self.payload_len += value.len();
            if self.payload.is_none() {
                self.payload = Some((start, value));
            } else {
                self.extra_payloads.push((start, value));
            }
        }
        self.pad();
    }

    fn pad(&mut self) {
        let len = self.len();
        self.bytes
            .resize(self.bytes.len() + len.next_multiple_of(4) - len, 0);
    }

    fn payloads(&self) -> impl Iterator<Item = &(usize, ReadData)> {
        self.payload.iter().chain(&self.extra_payloads)
    }

    pub(super) fn segments(&self) -> impl Iterator<Item = &[u8]> {
        let suffix = self
            .extra_payloads
            .last()
            .or(self.payload.as_ref())
            .map_or(0, |(start, data)| *start + data.len() - self.payload_len);
        let mut inline = 0;
        let mut attached = 0;
        self.payloads()
            .flat_map(move |(start, data)| {
                let split = *start - attached;
                let header = &self.bytes[inline..split];
                attached += data.len();
                inline = split;
                [header, data.as_ref()]
            })
            .chain(std::iter::once(&self.bytes[suffix..]))
    }

    pub(super) fn shrink_inline(&mut self, budget: usize) {
        if self.bytes.capacity() > budget {
            self.bytes.shrink_to_fit();
        }
    }

    pub(super) fn compact_for_replay(&self, budget: usize) -> Option<Self> {
        self.payload.as_ref()?;
        let mut retained = self.bytes.capacity();
        for (index, (_, data)) in self.payloads().enumerate() {
            if !self
                .payloads()
                .take(index)
                .any(|(_, previous)| previous.backing_ptr() == data.backing_ptr())
            {
                retained = retained.saturating_add(data.backing_capacity());
            }
        }
        if retained <= budget {
            return None;
        }
        let mut bytes = Vec::with_capacity(self.len());
        for slice in self.segments() {
            bytes.extend_from_slice(slice);
        }
        Some(Self {
            bytes,
            ..Self::default()
        })
    }

    pub(super) fn patch_u32(&mut self, offset: usize, value: u32) {
        let offset = self.inline_offset(offset);
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
#[test]
fn opaque_read_uses_the_original_range_with_padding_and_following_fields() {
    use std::sync::Arc;

    let mut data = b"--reads--".to_vec();
    data.resize(64, 0);
    let blob = Arc::new(data);
    let mut writer = Writer::default();
    writer.u32(42);
    writer.opaque_data(ReadData::Cached {
        bytes: blob.clone().into(),
        range: 2..7,
    });
    let following = writer.len();
    writer.u32(0);
    writer.patch_u32(following, 43);
    let parts: Vec<_> = writer.segments().collect();
    let &[prefix, payload, suffix] = parts.as_slice() else {
        panic!("expected one attached range");
    };
    assert_eq!(prefix, [0, 0, 0, 42, 0, 0, 0, 5]);
    assert_eq!(payload, b"reads");
    assert_eq!(payload.as_ptr(), blob[2..].as_ptr());
    assert_eq!(suffix, [0, 0, 0, 0, 0, 0, 43]);
    assert_eq!(writer.len(), 20);
    let saved = writer.compact_for_replay(20).unwrap();
    assert_eq!(saved.bytes(), parts.concat());
    let end = writer.len();
    writer.opaque_data(ReadData::Cached {
        bytes: blob.clone().into(),
        range: 3..7,
    });
    assert!(writer.compact_for_replay(128).is_none());
    writer.truncate(end);
    assert_eq!(
        writer.segments().collect::<Vec<_>>().concat(),
        saved.bytes()
    );
    writer.truncate(following);
    assert_eq!(writer.len(), 16);
    writer.truncate(4);
    assert_eq!(writer.bytes(), [0, 0, 0, 42]);
    assert_eq!(Arc::strong_count(&blob), 1);
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Bitmap([u32; 4]);

impl Bitmap {
    pub(super) fn decode(r: &mut Reader<'_>) -> Decode<Self> {
        let mut bits = Self::default();
        let count = r.count(bits.0.len())?;
        for word in &mut bits.0[..count] {
            *word = r.u32()?;
        }
        Ok(bits)
    }

    pub(super) fn encode(self, w: &mut Writer) {
        let count = self
            .0
            .iter()
            .rposition(|word| *word != 0)
            .map_or(0, |i| i + 1);
        w.u32(count as u32);
        for word in &self.0[..count] {
            w.u32(*word);
        }
    }

    pub(super) fn from_indices(indices: &[u32]) -> Self {
        let mut result = Self::default();
        for &bit in indices {
            result.set(bit);
        }
        result
    }

    pub(super) fn set(&mut self, bit: u32) {
        self.0[(bit / 32) as usize] |= 1 << (bit % 32);
    }

    pub(super) fn clear(&mut self, bit: u32) {
        self.0[(bit / 32) as usize] &= !(1 << (bit % 32));
    }

    pub(super) fn contains(self, bit: u32) -> bool {
        self.0[(bit / 32) as usize] & (1 << (bit % 32)) != 0
    }

    pub(super) fn union(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] | other.0[i]))
    }

    pub(super) fn intersection(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] & other.0[i]))
    }

    pub(super) fn iter(self) -> impl Iterator<Item = u32> {
        (0..128).filter(move |&i| self.contains(i))
    }
}

pub(super) fn fragment(raw: [u8; 4], so_far: usize) -> Decode<(usize, bool)> {
    let header = u32::from_be_bytes(raw);
    let len = (header & 0x7fff_ffff) as usize;
    if len > MAX_RECORD.checked_sub(so_far).ok_or(InvalidXdr)? {
        return Err(InvalidXdr);
    }
    Ok((len, header >> 31 != 0))
}
