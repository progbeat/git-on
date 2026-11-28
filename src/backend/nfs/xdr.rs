//! Bounds-checked XDR primitives for the local ONC RPC service.
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
}

impl Writer {
    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(super) fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
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
        self.bytes.resize(self.len().next_multiple_of(4), 0);
    }

    pub(super) fn patch_u32(&mut self, offset: usize, value: u32) {
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
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
