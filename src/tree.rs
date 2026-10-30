//! Prepared per-invocation directory metadata. Object decoding and index
//! construction happen once; memory eviction never restores a full-tree scan.
use crate::{
    raw_blob::{RawBlob, RawBlobs},
    spool::Spool,
};
use anyhow::{bail, Context, Result};
use git2::Oid;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::{hash_map::RandomState, HashMap},
    ffi::{OsStr, OsString},
    hash::BuildHasher,
    io::{BufRead, BufReader, Read},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
};

const KEY: usize = 80;
const VALUE: usize = 48;
const RECORD: usize = 192;
const SIGNED: usize = RECORD - 32;
type Key = [u8; KEY];
type Value = [u8; VALUE];
type Record = [u8; RECORD];
type Auth = Hmac<Sha256>;
fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}

// A missing root marker requests reconstruction from verified canonical bytes;
// it never authorizes a missing source name. Only sealed name tables do that.
struct Index {
    file: Spool,
    slots: u64,
    count: u64,
    hash: RandomState,
    key: [u8; 32],
    probes: Cell<u64>,
}
impl Index {
    fn new(raw: &RawBlobs, directory: &Path) -> Result<Self> {
        let file = raw.index_file(directory)?;
        file.set_len(64 * RECORD as u64)?;
        let mut key = [0; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Ok(Self {
            file,
            slots: 64,
            count: 0,
            hash: RandomState::new(),
            key,
            probes: Cell::new(0),
        })
    }
    fn auth(&self, record: &Record) -> Auth {
        let mut auth = Auth::new_from_slice(&self.key).expect("fixed key");
        auth.update(b"git-on-tree-index-v1");
        auth.update(&record[..SIGNED]);
        auth
    }
    fn record(&self, file: &Spool, slot: u64) -> Result<Record> {
        let mut record = [0; RECORD];
        file.read_exact_at(&mut record, slot * RECORD as u64)?;
        if record[0] != 0
            && (record[0] != 1 || self.auth(&record).verify_slice(&record[SIGNED..]).is_err())
        {
            return Err(invalid("prepared tree index was modified"));
        }
        Ok(record)
    }
    fn locate(
        &self,
        key: &Key,
        mut matches: impl FnMut(&Value) -> Result<bool>,
    ) -> Result<(u64, Option<Value>)> {
        let mut slot = self.hash.hash_one(key) & (self.slots - 1);
        for _ in 0..self.slots {
            self.probes.set(self.probes.get().saturating_add(1));
            let record = self.record(&self.file, slot)?;
            if record[0] == 0 {
                return Ok((slot, None));
            }
            if &record[1..1 + KEY] == key {
                let value: Value = record[1 + KEY..1 + KEY + VALUE].try_into().unwrap();
                if matches(&value)? {
                    return Ok((slot, Some(value)));
                }
            }
            slot = (slot + 1) & (self.slots - 1);
        }
        bail!("prepared tree index is full")
    }
    fn get(&self, key: &Key, matches: impl FnMut(&Value) -> Result<bool>) -> Result<Option<Value>> {
        Ok(self.locate(key, matches)?.1)
    }
    fn insert(
        &mut self,
        key: Key,
        value: Value,
        matches: impl FnMut(&Value) -> Result<bool>,
        raw: &RawBlobs,
        directory: &Path,
    ) -> Result<bool> {
        if self.count >= self.slots / 2 {
            let slots = self.slots.checked_mul(2).context("tree index capacity")?;
            let file = raw.index_file(directory)?;
            file.set_len(
                slots
                    .checked_mul(RECORD as u64)
                    .context("tree index size")?,
            )?;
            for old in 0..self.slots {
                crate::object::check_transfer()?;
                let record = self.record(&self.file, old)?;
                if record[0] == 0 {
                    continue;
                }
                let key: Key = record[1..1 + KEY].try_into().unwrap();
                let mut slot = self.hash.hash_one(key) & (slots - 1);
                while self.record(&file, slot)?[0] != 0 {
                    slot = (slot + 1) & (slots - 1);
                }
                file.write_all_at(&record, slot * RECORD as u64)?;
            }
            self.file = file;
            self.slots = slots;
        }
        let (slot, previous) = self.locate(&key, matches)?;
        if previous.is_some() {
            return Ok(false);
        }
        let mut record = [0; RECORD];
        record[0] = 1;
        record[1..1 + KEY].copy_from_slice(&key);
        record[1 + KEY..1 + KEY + VALUE].copy_from_slice(&value);
        let signature = self.auth(&record).finalize().into_bytes();
        record[SIGNED..].copy_from_slice(&signature);
        self.file.write_all_at(&record, slot * RECORD as u64)?;
        self.count += 1;
        Ok(true)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TreeItem {
    pub(crate) name: OsString,
    pub(crate) mode: u32,
    pub(crate) oid: Oid,
}
struct Memory {
    entries: Vec<TreeItem>,
    names: HashMap<OsString, usize>,
    weight: usize,
}
pub(crate) struct Directory {
    raw: RawBlob,
    table: Table,
    memory: Option<Memory>,
}
impl Directory {
    pub(crate) fn weight(&self) -> usize {
        std::mem::size_of::<Self>() + self.memory.as_ref().map_or(0, |memory| memory.weight)
    }
    pub(crate) fn children(&self) -> Result<Vec<TreeItem>> {
        if let Some(memory) = &self.memory {
            return Ok(memory.entries.clone());
        }
        let mut entries = Vec::new();
        let mut parser = Parser::new(self.raw.reader());
        while let Some(item) = parser.next()? {
            entries.push(item.owned());
        }
        Ok(entries)
    }
}
pub(crate) struct PreparedTrees {
    index: Index,
    tables: Tables,
    prepared_entries: u64,
}
fn root_key(oid: Oid) -> Key {
    let mut key = [0; KEY];
    key[1..21].copy_from_slice(oid.as_bytes());
    key
}
fn name_matches(raw: &RawBlob, value: &Value, name: &[u8]) -> Result<bool> {
    let offset = u64::from_le_bytes(value[..8].try_into().unwrap());
    let length = u64::from_le_bytes(value[8..16].try_into().unwrap());
    if length != name.len() as u64 {
        return Ok(false);
    }
    Ok(raw.read(offset, name.len().try_into()?)? == name)
}
impl PreparedTrees {
    pub(crate) fn new(raw: &RawBlobs, directory: &Path) -> Result<Self> {
        let index = Index::new(raw, directory)?;
        let tables = Tables {
            file: raw.index_file(directory)?,
        };
        Ok(Self {
            index,
            tables,
            prepared_entries: 0,
        })
    }
    pub(crate) fn open(&self, oid: Oid, raw: RawBlob) -> Result<Option<Directory>> {
        let Some(value) = self.index.get(&root_key(oid), |_| Ok(true))? else {
            return Ok(None);
        };
        Ok(Some(Directory {
            raw,
            table: Table::from_value(value),
            memory: None,
        }))
    }
    pub(crate) fn lookup(
        &self,
        oid: Oid,
        directory: &Directory,
        name: &OsStr,
    ) -> Result<Option<TreeItem>> {
        if let Some(memory) = &directory.memory {
            return Ok(memory
                .names
                .get(name)
                .map(|index| memory.entries[*index].clone()));
        }
        let name = name.as_bytes();
        let (_, value) = self.tables.locate(
            &self.index,
            oid,
            directory.table,
            &directory.raw,
            name,
            true,
        )?;
        let Some(value) = value else { return Ok(None) };
        Ok(Some(TreeItem {
            name: OsString::from_vec(name.to_vec()),
            oid: Oid::from_bytes(&value[16..36])?,
            mode: u32::from_le_bytes(value[36..40].try_into().unwrap()),
        }))
    }
    pub(crate) fn prepare(
        &mut self,
        oid: Oid,
        raw: RawBlob,
        storage: &RawBlobs,
        directory: &Path,
        memory_limit: usize,
    ) -> Result<Directory> {
        let mut table = self.tables.allocate(8)?;
        let mut parser = Parser::new(raw.reader());
        let mut entries = Some(Vec::new());
        let mut estimated = std::mem::size_of::<Directory>();
        let mut count = 0u64;
        while let Some(item) = parser.next()? {
            if count.is_multiple_of(256) {
                crate::object::check_transfer()?;
            }
            if count >= table.slots / 2 {
                table = self.tables.grow(&self.index, oid, table)?;
            }
            let (slot, previous) =
                self.tables
                    .locate(&self.index, oid, table, &raw, item.name, false)?;
            if previous.is_some() {
                return Err(invalid("duplicate path in Git tree"));
            }
            let mut record = [0; NAME_RECORD];
            record[0] = 1;
            record[1..33].copy_from_slice(&Sha256::digest(item.name));
            record[33..41].copy_from_slice(&item.name_offset.to_le_bytes());
            record[41..49].copy_from_slice(&(item.name.len() as u64).to_le_bytes());
            record[49..69].copy_from_slice(item.oid.as_bytes());
            record[69..73].copy_from_slice(&item.mode.to_le_bytes());
            self.tables
                .write(&self.index, oid, table, slot, record, false)?;
            count += 1;
            estimated = estimated.saturating_add(
                4 * std::mem::size_of::<TreeItem>()
                    + 2 * std::mem::size_of::<(OsString, usize)>()
                    + 4 * item.name.len(),
            );
            if estimated > memory_limit {
                entries = None;
            }
            if let Some(entries) = &mut entries {
                entries.push(item.owned());
            }
        }
        let memory = entries.and_then(|entries| {
            let names: HashMap<_, _> = entries
                .iter()
                .enumerate()
                .map(|(index, item)| (item.name.clone(), index))
                .collect();
            let weight = entries.capacity() * std::mem::size_of::<TreeItem>()
                + entries
                    .iter()
                    .map(|entry| entry.name.capacity())
                    .sum::<usize>()
                + 2 * names.capacity() * std::mem::size_of::<(OsString, usize)>()
                + names.keys().map(|name| name.capacity()).sum::<usize>();
            (weight + std::mem::size_of::<Directory>() <= memory_limit).then_some(Memory {
                entries,
                names,
                weight,
            })
        });
        table = self.tables.seal(&self.index, oid, table, count)?;
        self.index.insert(
            root_key(oid),
            table.value(),
            |_| Ok(true),
            storage,
            directory,
        )?;
        self.prepared_entries += count;
        Ok(Directory { raw, table, memory })
    }
    pub(crate) fn entries(&self) -> u64 {
        self.prepared_entries
    }
    pub(crate) fn probes(&self) -> u64 {
        self.index.probes.get()
    }
}

// A directory's name table becomes immutable before publication. Empty-slot
// proofs bind its final generation and slot, so an old empty row cannot erase
// a tracked name. Build generations never sign empties; final cardinality
// checking detects erased rows before the ready marker is published.
const NAME_RECORD: usize = 128;
const NAME_SIGNED: usize = NAME_RECORD - 32;
type NameRecord = [u8; NAME_RECORD];
#[derive(Clone, Copy)]
struct Table {
    offset: u64,
    slots: u64,
    count: u64,
    generation: [u8; 16],
}
impl Table {
    fn value(self) -> Value {
        let mut value = [0; VALUE];
        value[..8].copy_from_slice(&self.offset.to_le_bytes());
        value[8..16].copy_from_slice(&self.slots.to_le_bytes());
        value[16..24].copy_from_slice(&self.count.to_le_bytes());
        value[24..40].copy_from_slice(&self.generation);
        value
    }
    fn from_value(value: Value) -> Self {
        Self {
            offset: u64::from_le_bytes(value[..8].try_into().unwrap()),
            slots: u64::from_le_bytes(value[8..16].try_into().unwrap()),
            count: u64::from_le_bytes(value[16..24].try_into().unwrap()),
            generation: value[24..40].try_into().unwrap(),
        }
    }
}
struct Tables {
    file: Spool,
}
impl Tables {
    fn allocate(&self, slots: u64) -> Result<Table> {
        let offset = self.file.len()?;
        self.file.set_len(
            slots
                .checked_mul(NAME_RECORD as u64)
                .and_then(|size| offset.checked_add(size))
                .context("prepared name table size")?,
        )?;
        Ok(Table {
            offset,
            slots,
            count: 0,
            generation: *uuid::Uuid::new_v4().as_bytes(),
        })
    }
    fn auth(
        index: &Index,
        oid: Oid,
        table: Table,
        slot: u64,
        record: &NameRecord,
        ready: bool,
    ) -> Auth {
        let mut auth = Auth::new_from_slice(&index.key).expect("fixed key");
        auth.update(if ready {
            b"git-on-ready-tree-v1"
        } else {
            b"git-on-build-tree-v1"
        });
        auth.update(oid.as_bytes());
        auth.update(&table.generation);
        auth.update(&table.offset.to_le_bytes());
        auth.update(&slot.to_le_bytes());
        auth.update(&record[..NAME_SIGNED]);
        auth
    }
    fn read(
        &self,
        index: &Index,
        oid: Oid,
        table: Table,
        slot: u64,
        ready: bool,
    ) -> Result<NameRecord> {
        let mut record = [0; NAME_RECORD];
        self.file
            .read_exact_at(&mut record, table.offset + slot * NAME_RECORD as u64)?;
        if record[0] > 1
            || ((ready || record[0] != 0)
                && Self::auth(index, oid, table, slot, &record, ready)
                    .verify_slice(&record[NAME_SIGNED..])
                    .is_err())
        {
            return Err(invalid("prepared directory names were modified"));
        }
        Ok(record)
    }
    fn write(
        &self,
        index: &Index,
        oid: Oid,
        table: Table,
        slot: u64,
        mut record: NameRecord,
        ready: bool,
    ) -> Result<()> {
        let signature = Self::auth(index, oid, table, slot, &record, ready)
            .finalize()
            .into_bytes();
        record[NAME_SIGNED..].copy_from_slice(&signature);
        self.file
            .write_all_at(&record, table.offset + slot * NAME_RECORD as u64)?;
        Ok(())
    }
    fn locate(
        &self,
        index: &Index,
        oid: Oid,
        table: Table,
        raw: &RawBlob,
        name: &[u8],
        ready: bool,
    ) -> Result<(u64, Option<Value>)> {
        let digest: [u8; 32] = Sha256::digest(name).into();
        let mut slot = index.hash.hash_one(digest) & (table.slots - 1);
        for _ in 0..table.slots {
            index.probes.set(index.probes.get().saturating_add(1));
            let record = self.read(index, oid, table, slot, ready)?;
            if record[0] == 0 {
                return Ok((slot, None));
            }
            if record[1..33] == digest {
                let value: Value = record[33..81].try_into().unwrap();
                if name_matches(raw, &value, name)? {
                    return Ok((slot, Some(value)));
                }
            }
            slot = (slot + 1) & (table.slots - 1);
        }
        bail!("prepared directory name table is full")
    }
    fn grow(&self, index: &Index, oid: Oid, old: Table) -> Result<Table> {
        let table = self.allocate(
            old.slots
                .checked_mul(2)
                .context("directory name table capacity")?,
        )?;
        for old_slot in 0..old.slots {
            if old_slot.is_multiple_of(256) {
                crate::object::check_transfer()?;
            }
            let record = self.read(index, oid, old, old_slot, false)?;
            if record[0] == 0 {
                continue;
            }
            let digest: [u8; 32] = record[1..33].try_into().unwrap();
            let mut slot = index.hash.hash_one(digest) & (table.slots - 1);
            while self.read(index, oid, table, slot, false)?[0] != 0 {
                slot = (slot + 1) & (table.slots - 1);
            }
            self.write(index, oid, table, slot, record, false)?;
        }
        Ok(table)
    }
    fn seal(&self, index: &Index, oid: Oid, building: Table, count: u64) -> Result<Table> {
        let ready = Table {
            generation: *uuid::Uuid::new_v4().as_bytes(),
            count,
            ..building
        };
        let mut present = 0;
        for slot in 0..building.slots {
            if slot.is_multiple_of(256) {
                crate::object::check_transfer()?;
            }
            let mut record = self.read(index, oid, building, slot, false)?;
            if record[0] == 0 {
                record.fill(0)
            } else {
                present += 1;
            }
            self.write(index, oid, ready, slot, record, true)?;
        }
        if present != count {
            return Err(invalid(
                "prepared directory entries were erased during construction",
            ));
        }
        Ok(ready)
    }
}

struct Parser<R> {
    input: BufReader<R>,
    position: u64,
    mode: Vec<u8>,
    name: Vec<u8>,
}
struct Item<'a> {
    name: &'a [u8],
    mode: u32,
    oid: Oid,
    name_offset: u64,
}
impl Item<'_> {
    fn owned(&self) -> TreeItem {
        TreeItem {
            name: OsString::from_vec(self.name.to_vec()),
            mode: self.mode,
            oid: self.oid,
        }
    }
}
impl<R: Read> Parser<R> {
    fn new(input: R) -> Self {
        Self {
            input: BufReader::with_capacity(64 * 1024, input),
            position: 0,
            mode: Vec::new(),
            name: Vec::new(),
        }
    }
    fn next(&mut self) -> Result<Option<Item<'_>>> {
        self.mode.clear();
        let count = Read::by_ref(&mut self.input)
            .take(12)
            .read_until(b' ', &mut self.mode)?;
        if count == 0 {
            return Ok(None);
        }
        if count == 1 || self.mode.last() != Some(&b' ') {
            return Err(invalid("invalid Git tree mode"));
        }
        let mut mode = 0u32;
        for byte in &self.mode[..count - 1] {
            if !(b'0'..=b'7').contains(byte) {
                return Err(invalid("invalid Git tree mode"));
            }
            mode = mode
                .checked_mul(8)
                .and_then(|mode| mode.checked_add(u32::from(byte - b'0')))
                .context("Git tree mode overflow")?;
        }
        self.position += count as u64;
        let name_offset = self.position;
        self.name.clear();
        Read::by_ref(&mut self.input)
            .take(libc::NAME_MAX as u64 + 1)
            .read_until(0, &mut self.name)?;
        if self.name.last() != Some(&0) {
            return Err(invalid("invalid or unsupported Git tree name"));
        }
        let length = self.name.len() - 1;
        let name = &self.name[..length];
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            return Err(invalid("invalid name in Git tree"));
        }
        self.position += self.name.len() as u64;
        let mut oid = [0; 20];
        self.input.read_exact(&mut oid)?;
        self.position += 20;
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
            name_offset,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn erased_name_record_must_not_turn_a_tracked_name_into_absence() {
        let root = tempfile::tempdir().unwrap();
        let blob = Oid::hash_object(git2::ObjectType::Blob, b"x").unwrap();
        let mut bytes = b"100644 tracked\0".to_vec();
        bytes.extend(blob.as_bytes());
        let oid = Oid::hash_object(git2::ObjectType::Tree, &bytes).unwrap();
        let mut storage = RawBlobs::new(root.path()).unwrap();
        let raw = storage
            .insert_tree(oid, bytes.len(), bytes.as_slice(), root.path())
            .unwrap();
        let mut trees = PreparedTrees::new(&storage, root.path()).unwrap();
        let directory = trees.prepare(oid, raw, &storage, root.path(), 0).unwrap();
        let (slot, _) = trees
            .tables
            .locate(
                &trees.index,
                oid,
                directory.table,
                &directory.raw,
                b"tracked",
                true,
            )
            .unwrap();
        trees
            .tables
            .file
            .write_all_at(
                &[0; NAME_RECORD],
                directory.table.offset + slot * NAME_RECORD as u64,
            )
            .unwrap();
        assert!(
            trees
                .lookup(oid, &directory, OsStr::new("tracked"))
                .is_err(),
            "tampering forged a missing tracked name"
        );
    }
    #[test]
    fn empty_slot_proofs_cannot_be_replayed_at_an_occupied_slot() {
        let root = tempfile::tempdir().unwrap();
        let blob = Oid::hash_object(git2::ObjectType::Blob, b"x").unwrap();
        let mut bytes = b"100644 tracked\0".to_vec();
        bytes.extend(blob.as_bytes());
        let oid = Oid::hash_object(git2::ObjectType::Tree, &bytes).unwrap();
        let mut storage = RawBlobs::new(root.path()).unwrap();
        let raw = storage
            .insert_tree(oid, bytes.len(), bytes.as_slice(), root.path())
            .unwrap();
        let mut trees = PreparedTrees::new(&storage, root.path()).unwrap();
        let directory = trees.prepare(oid, raw, &storage, root.path(), 0).unwrap();
        assert!(trees
            .lookup(oid, &directory, OsStr::new("absent"))
            .unwrap()
            .is_none());
        let (occupied, _) = trees
            .tables
            .locate(
                &trees.index,
                oid,
                directory.table,
                &directory.raw,
                b"tracked",
                true,
            )
            .unwrap();
        let empty = (0..directory.table.slots)
            .find_map(|slot| {
                let record = trees
                    .tables
                    .read(&trees.index, oid, directory.table, slot, true)
                    .unwrap();
                (record[0] == 0).then_some(record)
            })
            .unwrap();
        trees
            .tables
            .file
            .write_all_at(
                &empty,
                directory.table.offset + occupied * NAME_RECORD as u64,
            )
            .unwrap();
        assert!(trees
            .lookup(oid, &directory, OsStr::new("tracked"))
            .is_err());
    }
    #[test]
    fn erased_build_rows_fail_cardinality_before_ready_publication() {
        let root = tempfile::tempdir().unwrap();
        let storage = RawBlobs::new(root.path()).unwrap();
        let trees = PreparedTrees::new(&storage, root.path()).unwrap();
        let oid = Oid::zero();
        let building = trees.tables.allocate(64).unwrap();
        let mut record = [0; NAME_RECORD];
        record[0] = 1;
        trees
            .tables
            .write(&trees.index, oid, building, 9, record, false)
            .unwrap();
        trees
            .tables
            .file
            .write_all_at(&[0; NAME_RECORD], building.offset + 9 * NAME_RECORD as u64)
            .unwrap();
        assert!(trees.tables.seal(&trees.index, oid, building, 1).is_err());
        assert!(trees
            .index
            .get(&root_key(oid), |_| Ok(true))
            .unwrap()
            .is_none());
    }
    #[test]
    fn malformed_and_duplicate_names_never_publish_ready_metadata() {
        let root = tempfile::tempdir().unwrap();
        let mut storage = RawBlobs::new(root.path()).unwrap();
        let mut trees = PreparedTrees::new(&storage, root.path()).unwrap();
        let blob = Oid::hash_object(git2::ObjectType::Blob, b"x").unwrap();
        let entry = |mode: &[u8], name: &[u8]| {
            let mut bytes = mode.to_vec();
            bytes.push(b' ');
            bytes.extend(name);
            bytes.push(0);
            bytes.extend(blob.as_bytes());
            bytes
        };
        let mut duplicate = entry(b"100644", b"same");
        duplicate.extend(entry(b"100644", b"same"));
        for bytes in [
            entry(b"", b"name"),
            entry(b"100649", b"name"),
            entry(b"100644", b"a/b"),
            entry(b"100644", b".."),
            b"100644 truncated".to_vec(),
            duplicate,
        ] {
            let oid = Oid::hash_object(git2::ObjectType::Tree, &bytes).unwrap();
            let raw = storage
                .insert_tree(oid, bytes.len(), bytes.as_slice(), root.path())
                .unwrap();
            assert!(trees
                .prepare(oid, raw.clone(), &storage, root.path(), 0)
                .is_err());
            assert!(trees.open(oid, raw).unwrap().is_none());
        }
    }
}
