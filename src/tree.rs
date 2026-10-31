//! Immutable parsed directories retained once per touched Git tree OID.
//! Names are shared between iteration order and the expected-O(1) lookup map.
use anyhow::{Context, Result};
use git2::Oid;
use sha1_checked::{Digest, Sha1};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::{BufRead, BufReader, Read},
    os::unix::ffi::OsStringExt,
    sync::Arc,
};
fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
#[derive(Clone, Debug)]
pub(crate) struct TreeItem {
    pub(crate) name: OsString,
    pub(crate) mode: u32,
    pub(crate) oid: Oid,
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
        let mut parser = Parser::new(&mut input);
        let mut entries = Vec::new();
        let mut names = HashMap::new();
        while let Some(item) = parser.next()? {
            if entries.len().is_multiple_of(256) {
                crate::object::check_transfer()?;
            }
            let name: Arc<OsStr> = Arc::from(OsString::from_vec(item.name.to_vec()));
            if names.insert(name.clone(), entries.len()).is_some() {
                return Err(invalid("duplicate path in Git tree"));
            }
            entries.push(Node {
                name,
                mode: item.mode,
                oid: item.oid,
            });
        }
        drop(parser);
        // Parsing and hashing consume exactly the same stream. Neither entries
        // nor negative lookup results become observable before full OID trust.
        input.finish(oid)?;
        let weight = std::mem::size_of::<Self>()
            + entries.capacity() * std::mem::size_of::<Node>()
            + entries
                .iter()
                .map(|item| item.name.len() + 3 * std::mem::size_of::<usize>())
                .sum::<usize>()
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
    fn owned(node: &Node) -> TreeItem {
        TreeItem {
            name: node.name.to_os_string(),
            mode: node.mode,
            oid: node.oid,
        }
    }
    pub(crate) fn lookup(&self, name: &OsStr) -> Option<TreeItem> {
        self.names
            .get(name)
            .map(|index| Self::owned(&self.entries[*index]))
    }
    pub(crate) fn children(&self) -> Result<Vec<TreeItem>> {
        Ok(self.entries.iter().map(Self::owned).collect())
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
        crate::object::check_transfer().map_err(crate::git::transfer_io_error)?;
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
    input: BufReader<R>,
    name: Vec<u8>,
}
struct Item<'a> {
    name: &'a [u8],
    mode: u32,
    oid: Oid,
}
impl<R: Read> Parser<R> {
    fn new(input: R) -> Self {
        Self {
            input: BufReader::with_capacity(64 * 1024, input),
            name: Vec::new(),
        }
    }
    fn next(&mut self) -> Result<Option<Item<'_>>> {
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
        self.name.clear();
        // Git tree names have no platform NAME_MAX field. Keep one active name
        // rather than imposing a parser limit from the current host filesystem;
        // a materializer/kernel reports its actual representation limitation.
        self.input.read_until(0, &mut self.name)?;
        if self.name.last() != Some(&0) {
            return Err(invalid("unterminated Git tree name"));
        }
        let length = self.name.len() - 1;
        let name = &self.name[..length];
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            return Err(invalid("invalid name in Git tree"));
        }
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
        Ok(Some(Item {
            name,
            mode,
            oid: Oid::from_bytes(&oid)?,
        }))
    }
}
