//! Immutable parsed directories retained once per touched Git tree OID.
//! Names are shared between iteration order and the expected-O(1) lookup map.
use anyhow::{Context, Result};
use git2::Oid;
use sha1_checked::{Digest, Sha1};
use std::{
    collections::HashMap,
    ffi::OsStr,
    io::{BufRead, BufReader, Read},
    os::unix::ffi::OsStrExt,
    sync::Arc,
};
fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
struct Node {
    name: Arc<OsStr>,
    mode: u32,
    oid: Oid,
}
pub(crate) struct Directory {
    entries: Vec<Node>,
    names: HashMap<Arc<OsStr>, usize>,
    weight: usize,
}
impl Directory {
    pub(crate) fn verified(oid: Oid, size: usize, input: impl Read) -> Result<Self> {
        let mut input = Checked::new(input, size);
        let directory = Self::parse(BufReader::with_capacity(64 * 1024, &mut input))?;
        // Parsing and hashing consume exactly the same stream. Neither entries
        // nor negative lookup results become observable before full OID trust.
        input.finish(oid)?;
        Ok(directory)
    }
    pub(crate) fn from_verified(object: &crate::pack_stream::VerifiedObject) -> Result<Self> {
        anyhow::ensure!(
            object.kind() == git2::ObjectType::Tree,
            "streamed object is not a tree"
        );
        Self::from_verified_bytes(object.data())
    }
    pub(crate) fn from_odb(oid: Oid, object: &git2::OdbObject<'_>) -> Result<Self> {
        anyhow::ensure!(object.id() == oid, "snapshot tree changed during transfer");
        crate::object::verify_odb_object(object, git2::ObjectType::Tree)?;
        Self::from_verified_bytes(object.data())
    }
    fn from_verified_bytes(data: &[u8]) -> Result<Self> {
        Self::parse(VerifiedBytes {
            remaining: data,
            available: 0,
        })
    }
    fn parse(input: impl BufRead) -> Result<Self> {
        let mut parser = Parser::new(input);
        let mut entries = Vec::new();
        let mut names = HashMap::new();
        let mut name_bytes = 0;
        while let Some(node) = parser.next()? {
            if entries.len().is_multiple_of(256) {
                crate::object::check_transfer()?;
            }
            name_bytes += node.name.len() + 3 * std::mem::size_of::<usize>();
            if names.insert(node.name.clone(), entries.len()).is_some() {
                return Err(invalid("duplicate path in Git tree"));
            }
            entries.push(node);
        }
        let weight = std::mem::size_of::<Self>()
            + entries.capacity() * std::mem::size_of::<Node>()
            + name_bytes
            + 2 * names.capacity() * (std::mem::size_of::<(Arc<OsStr>, usize)>() + 1);
        Ok(Self {
            entries,
            names,
            weight,
        })
    }
    pub(crate) fn weight(&self) -> usize {
        self.weight
    }
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn index(&self, name: &OsStr) -> Option<usize> {
        self.names.get(name).copied()
    }
    pub(crate) fn at(&self, index: usize) -> Option<(&OsStr, u32)> {
        self.entries
            .get(index)
            .map(|node| (node.name.as_ref(), node.mode))
    }
    pub(crate) fn shared_at(&self, index: usize) -> Option<(Arc<OsStr>, u32)> {
        self.entries
            .get(index)
            .map(|node| (node.name.clone(), node.mode))
    }
    /// Resolve metadata without allocating a copy of the already-known name.
    pub(crate) fn lookup(&self, name: &OsStr) -> Option<(u32, Oid)> {
        self.index(name).and_then(|index| self.metadata_at(index))
    }
    pub(crate) fn metadata_at(&self, index: usize) -> Option<(u32, Oid)> {
        self.entries.get(index).map(|node| (node.mode, node.oid))
    }
    pub(crate) fn children(&self) -> impl ExactSizeIterator<Item = (&OsStr, u32, Oid)> {
        self.entries
            .iter()
            .map(|node| (node.name.as_ref(), node.mode, node.oid))
    }
}
// Parse the verified buffer directly while retaining the cancellation cadence
// of the streaming reader, including within a long mode or name.
struct VerifiedBytes<'a> {
    remaining: &'a [u8],
    available: usize,
}
impl Read for VerifiedBytes<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.fill_buf()?.read(output)?;
        self.consume(count);
        Ok(count)
    }
}
impl BufRead for VerifiedBytes<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.available == 0 {
            crate::object::check_transfer().map_err(std::io::Error::other)?;
            self.available = self.remaining.len().min(64 * 1024);
        }
        Ok(&self.remaining[..self.available])
    }
    fn consume(&mut self, count: usize) {
        self.remaining = &self.remaining[count..];
        self.available -= count;
    }
}

struct Checked<R> {
    input: R,
    remaining: usize,
    hash: Sha1,
}
impl<R: Read> Checked<R> {
    fn new(input: R, size: usize) -> Self {
        let mut hash = Sha1::new();
        hash.update(format!("tree {size}\0").as_bytes());
        Self {
            input,
            remaining: size,
            hash,
        }
    }
    fn finish(mut self, oid: Oid) -> Result<()> {
        crate::object::check_transfer()?;
        if self.remaining != 0 {
            return Err(invalid("truncated Git tree"));
        }
        let mut extra = [0];
        if self.input.read(&mut extra)? != 0 {
            return Err(invalid("oversized Git tree"));
        }
        if crate::object::finish_sha1(self.hash)?.as_slice() != oid.as_bytes() {
            return Err(invalid("Git tree hash mismatch"));
        }
        Ok(())
    }
}
impl<R: Read> Read for Checked<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        crate::object::check_transfer().map_err(std::io::Error::other)?;
        let length = bytes.len().min(self.remaining);
        if length == 0 {
            return Ok(0);
        }
        let count = self.input.read(&mut bytes[..length])?;
        self.hash.update(&bytes[..count]);
        self.remaining -= count;
        Ok(count)
    }
}
struct Parser<R> {
    input: R,
    name: Vec<u8>,
}
impl<R: BufRead> Parser<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            name: Vec::new(),
        }
    }
    fn next(&mut self) -> Result<Option<Node>> {
        // libgit2 accepts arbitrarily many leading zeroes but bounds the value
        // to UINT16_MAX before normalizing legacy modes. Parse incrementally so
        // preserving that behavior does not need an unbounded text buffer.
        let mut mode = 0u16;
        let mut digits = false;
        loop {
            let mut byte = [0];
            if self.input.read(&mut byte)? == 0 {
                return if digits {
                    Err(invalid("unterminated Git tree mode"))
                } else {
                    Ok(None)
                };
            }
            if byte[0] == b' ' && digits {
                break;
            }
            if !(b'0'..=b'7').contains(&byte[0]) {
                return Err(invalid("invalid Git tree mode"));
            }
            digits = true;
            mode = mode
                .checked_mul(8)
                .and_then(|mode| mode.checked_add(u16::from(byte[0] - b'0')))
                .context("Git tree mode overflow")?;
        }
        let mode = u32::from(mode);
        let name = self.read_name()?;
        let mut oid = [0; 20];
        self.input.read_exact(&mut oid)?;
        // Preserve libgit2's normalized mode semantics, including legacy modes.
        let kind = mode & 0o170000;
        let mode = if kind == 0o040000 {
            0o040000
        } else if mode & 0o100 != 0 {
            0o100755
        } else if kind == 0o160000 {
            0o160000
        } else if kind == 0o120000 {
            0o120000
        } else {
            0o100644
        };
        Ok(Some(Node {
            name,
            mode,
            oid: Oid::from_bytes(&oid)?,
        }))
    }

    fn read_name(&mut self) -> Result<Arc<OsStr>> {
        self.name.clear();
        loop {
            let bytes = match self.input.fill_buf() {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if bytes.is_empty() {
                return Err(invalid("unterminated Git tree name"));
            }
            if let Some(end) = memchr::memchr(0, bytes) {
                // Copy from the input directly into the shared name when it
                // fits in this chunk. Use scratch only across chunk boundaries.
                let name = if self.name.is_empty() {
                    &bytes[..end]
                } else {
                    self.name.extend_from_slice(&bytes[..end]);
                    self.name.as_slice()
                };
                if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
                    return Err(invalid("invalid name in Git tree"));
                }
                let name = Arc::from(OsStr::from_bytes(name));
                self.input.consume(end + 1);
                return Ok(name);
            }
            let count = bytes.len();
            self.name.extend_from_slice(bytes);
            self.input.consume(count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::ObjectType;

    #[test]
    fn a_name_crossing_the_input_boundary_preserves_raw_bytes_and_the_next_entry() {
        let mode = b"100644 ";
        let mut long = vec![b'a'; 64 * 1024 - mode.len()];
        *long.last_mut().unwrap() = 0xfe;
        let short = b"file-\xff";
        let oid = Oid::hash_object(ObjectType::Blob, b"blob").unwrap();
        let mut bytes = Vec::new();
        for name in [&long, short.as_slice()] {
            bytes.extend_from_slice(mode);
            bytes.extend_from_slice(name);
            bytes.push(0);
            bytes.extend_from_slice(oid.as_bytes());
        }
        let tree_oid = Oid::hash_object(ObjectType::Tree, &bytes).unwrap();
        for tree in [
            Directory::verified(tree_oid, bytes.len(), bytes.as_slice()).unwrap(),
            Directory::from_verified_bytes(&bytes).unwrap(),
        ] {
            assert_eq!(tree.len(), 2);
            assert_eq!(tree.lookup(OsStr::from_bytes(&long)), Some((0o100644, oid)));
            assert_eq!(tree.at(1), Some((OsStr::from_bytes(short), 0o100644)));
        }
    }
}
