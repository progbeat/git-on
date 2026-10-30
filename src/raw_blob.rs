//! Authenticated per-invocation bytes and a disk-resident randomized index.
//! Three logical files retain constant descriptors and index RAM. Bounded parts
//! respect per-file limits; block tags authenticate later reads from disk.
use crate::spool::{Files, Spool};
use anyhow::{bail, Context, Result};
use git2::Oid;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{
    collections::hash_map::RandomState,
    hash::BuildHasher,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
};

const SLOT_BYTES: u64 = 96;
const INITIAL_SLOTS: u64 = 64;
const BLOCK: u64 = 4096;
const TAG_BYTES: u64 = 32;
type Auth = Hmac<Sha256>;
type Key = Arc<[u8; 32]>;

fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
fn authenticator(key: &[u8], domain: &[u8]) -> Auth {
    let mut auth = Auth::new_from_slice(key).expect("fixed HMAC key");
    auth.update(domain);
    auth
}
fn block_auth(key: &[u8], oid: Oid, size: u64, generation: &[u8; 16], block: u64) -> Auth {
    let mut auth = authenticator(key, b"git-on-raw-block-v1");
    auth.update(oid.as_bytes());
    auth.update(&size.to_le_bytes());
    auth.update(generation);
    auth.update(&block.to_le_bytes());
    auth
}
#[derive(Clone, Copy)]
struct Location {
    offset: u64,
    size: u64,
    tags: u64,
    generation: [u8; 16],
    tree: bool,
}
#[derive(Clone)]
pub(crate) struct RawBlob {
    file: Arc<Spool>,
    tags: Arc<Spool>,
    key: Key,
    oid: Oid,
    location: Location,
}
impl RawBlob {
    pub(crate) fn kind(&self) -> git2::ObjectType {
        if self.location.tree {
            git2::ObjectType::Tree
        } else {
            git2::ObjectType::Blob
        }
    }
    pub(crate) fn len(&self) -> u64 {
        self.location.size
    }
    pub(crate) fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.range(
            offset,
            self.len().saturating_sub(offset).min(u64::from(size)),
        )
    }
    pub(crate) fn read_all(&self) -> Result<Vec<u8>> {
        self.range(0, self.len())
    }
    pub(crate) fn reader(&self) -> impl Read {
        VerifiedReader {
            blob: self.clone(),
            position: 0,
        }
    }
    fn range(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let first = offset / BLOCK;
        let last = (offset + length - 1) / BLOCK;
        let start = first * BLOCK;
        let end = (last + 1).saturating_mul(BLOCK).min(self.len());
        let mut data = vec![0; usize::try_from(end - start).context("raw read size")?];
        let mut tags = vec![0; usize::try_from((last - first + 1) * TAG_BYTES)?];
        self.file
            .read_exact_at(&mut data, self.location.offset + start)?;
        self.tags
            .read_exact_at(&mut tags, self.location.tags + first * TAG_BYTES)?;
        for (i, chunk) in data.chunks(BLOCK as usize).enumerate() {
            crate::object::check_transfer()?;
            let mut auth = block_auth(
                self.key.as_ref(),
                self.oid,
                self.len(),
                &self.location.generation,
                first + i as u64,
            );
            auth.update(chunk);
            auth.verify_slice(&tags[i * 32..(i + 1) * 32])
                .map_err(|_| invalid("verified raw blob bytes were modified"))?;
        }
        // Return precisely the bytes checked above, with no second disk read
        // between authentication and exposure to the filesystem caller.
        let prefix = (offset - start) as usize;
        let length = usize::try_from(length)?;
        data.copy_within(prefix..prefix + length, 0);
        data.truncate(length);
        Ok(data)
    }
}
struct VerifiedReader {
    blob: RawBlob,
    position: u64,
}
impl Read for VerifiedReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let data = self
            .blob
            .read(self.position, bytes.len().min(64 * 1024) as u32)
            .map_err(std::io::Error::other)?;
        bytes[..data.len()].copy_from_slice(&data);
        self.position += data.len() as u64;
        Ok(data.len())
    }
}
/// Only staged, not-yet-trusted native stdout uses this unchecked reader.
struct StagedReader {
    file: Arc<Spool>,
    start: u64,
    size: u64,
    position: u64,
}
impl Read for StagedReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let length = self
            .size
            .saturating_sub(self.position)
            .min(bytes.len() as u64) as usize;
        let count = self
            .file
            .read_at(&mut bytes[..length], self.start + self.position)?;
        self.position += count as u64;
        Ok(count)
    }
}
struct Tags<'a> {
    output: BufWriter<&'a Spool>,
    key: &'a [u8],
    oid: Oid,
    size: u64,
    generation: [u8; 16],
    block: u64,
    used: usize,
    auth: Auth,
}
impl<'a> Tags<'a> {
    fn new(output: &'a Spool, key: &'a [u8], oid: Oid, size: u64, generation: [u8; 16]) -> Self {
        Self {
            output: BufWriter::with_capacity(64 * 1024, output),
            key,
            oid,
            size,
            generation,
            block: 0,
            used: 0,
            auth: block_auth(key, oid, size, &generation, 0),
        }
    }
    fn tag(&mut self) -> std::io::Result<()> {
        self.output
            .write_all(&self.auth.clone().finalize().into_bytes())?;
        self.block += 1;
        self.used = 0;
        self.auth = block_auth(self.key, self.oid, self.size, &self.generation, self.block);
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        if self.used != 0 {
            self.tag()?;
        }
        self.output.flush()?;
        Ok(())
    }
}
impl Write for Tags<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let length = bytes.len();
        while !bytes.is_empty() {
            let count = bytes.len().min(BLOCK as usize - self.used);
            self.auth.update(&bytes[..count]);
            self.used += count;
            bytes = &bytes[count..];
            if self.used == BLOCK as usize {
                self.tag()?;
            }
        }
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
struct StoreAndTag<'a> {
    data: &'a Spool,
    tags: Tags<'a>,
}
impl Write for StoreAndTag<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.data.write_all(bytes)?;
        self.tags.write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tags.flush()
    }
}

pub(crate) struct RawBlobs {
    files: Files,
    data: Arc<Spool>,
    tags: Arc<Spool>,
    index: Spool,
    key: Key,
    slots: u64,
    hash: RandomState,
    indexed_entries: u64,
    blob_entries: u64,
    blob_bytes: u64,
    tree_bytes: u64,
}
impl RawBlobs {
    pub(crate) fn index_file(&self, directory: &Path) -> Result<Spool> {
        Ok(self.files.file(directory)?)
    }
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        let files = Files::new(directory)?;
        Self::with_files(directory, files)
    }
    fn with_files(directory: &Path, files: Files) -> Result<Self> {
        let data = Arc::new(files.file(directory)?);
        let tags = Arc::new(files.file(directory)?);
        let index = files.file(directory)?;
        index.set_len(INITIAL_SLOTS * SLOT_BYTES)?;
        // UUID v4 uses the existing OS-random source; neither key nor tags are
        // reused across invocations or exposed through the metrics channel.
        let mut key = [0; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Ok(Self {
            files,
            data,
            tags,
            index,
            key: Arc::new(key),
            slots: INITIAL_SLOTS,
            hash: RandomState::new(),
            indexed_entries: 0,
            blob_entries: 0,
            blob_bytes: 0,
            tree_bytes: 0,
        })
    }
    fn seal(&self, record: &mut [u8; SLOT_BYTES as usize]) {
        let mut auth = authenticator(self.key.as_ref(), b"git-on-raw-index-v1");
        auth.update(&record[..64]);
        record[64..].copy_from_slice(&auth.finalize().into_bytes());
    }
    fn verify(&self, record: &[u8; SLOT_BYTES as usize]) -> Result<()> {
        if record[0] != 1 || record[62] > 1 {
            return Err(invalid("invalid private raw-blob index"));
        }
        let mut auth = authenticator(self.key.as_ref(), b"git-on-raw-index-v1");
        auth.update(&record[..64]);
        auth.verify_slice(&record[64..])
            .map_err(|_| invalid("private raw-blob index was modified"))?;
        Ok(())
    }
    fn locate(&self, oid: Oid) -> Result<(u64, Option<Location>)> {
        let mut slot = self.hash.hash_one(oid) & (self.slots - 1);
        for _ in 0..self.slots {
            let record = read_slot(&self.index, slot)?;
            if record[0] == 0 {
                return Ok((slot, None));
            }
            self.verify(&record)?;
            if &record[1..21] == oid.as_bytes() {
                return Ok((slot, Some(decode_location(&record))));
            }
            slot = (slot + 1) & (self.slots - 1);
        }
        bail!("private raw-blob index is full")
    }
    fn blob(&self, oid: Oid, location: Location) -> RawBlob {
        RawBlob {
            file: self.data.clone(),
            tags: self.tags.clone(),
            key: self.key.clone(),
            oid,
            location,
        }
    }
    pub(crate) fn get(&self, oid: Oid) -> Result<Option<RawBlob>> {
        Ok(self.locate(oid)?.1.map(|location| self.blob(oid, location)))
    }
    pub(crate) fn for_read(&mut self, oid: Oid) -> Result<Option<(RawBlob, bool)>> {
        let (slot, location) = self.locate(oid)?;
        if location.is_none() {
            return Ok(None);
        }
        let mut record = read_slot(&self.index, slot)?;
        self.verify(&record)?;
        if &record[1..21] != oid.as_bytes() {
            return Err(invalid("private raw-blob locator changed"));
        }
        let location = decode_location(&record);
        anyhow::ensure!(!location.tree, "snapshot file does not reference a blob");
        let first = record[45] == 0;
        if first {
            record[45] = 1;
            self.seal(&mut record);
            self.index.write_all_at(&record, slot * SLOT_BYTES)?;
        }
        Ok(Some((self.blob(oid, location), first)))
    }
    pub(crate) fn append_output(&self) -> Result<(RawOutput, u64)> {
        let start = (&*self.data).seek(SeekFrom::End(0))?;
        Ok((
            RawOutput {
                file: self.data.clone(),
            },
            start,
        ))
    }
    pub(crate) fn output_reader(&self, start: u64) -> Result<impl Read> {
        let size = self
            .data
            .len()?
            .checked_sub(start)
            .context("invalid raw output range")?;
        Ok(StagedReader {
            file: self.data.clone(),
            start,
            size,
            position: 0,
        })
    }
    pub(crate) fn import_blob(
        &mut self,
        oid: Oid,
        offset: u64,
        size: usize,
        input: impl Read,
        directory: &Path,
    ) -> Result<()> {
        self.import(
            Incoming {
                oid,
                offset,
                size,
                tree: false,
            },
            input,
            std::io::sink(),
            directory,
        )
    }
    pub(crate) fn import_tree(
        &mut self,
        oid: Oid,
        offset: u64,
        size: usize,
        input: impl Read,
        metadata: impl Write,
        directory: &Path,
    ) -> Result<()> {
        self.import(
            Incoming {
                oid,
                offset,
                size,
                tree: true,
            },
            input,
            metadata,
            directory,
        )
    }
    fn import(
        &mut self,
        object: Incoming,
        input: impl Read,
        mut metadata: impl Write,
        directory: &Path,
    ) -> Result<()> {
        let Incoming {
            oid,
            offset,
            size,
            tree,
        } = object;
        let tags = (&*self.tags).seek(SeekFrom::End(0))?;
        let generation = *uuid::Uuid::new_v4().as_bytes();
        let mut output = Tags::new(&self.tags, self.key.as_ref(), oid, size as u64, generation);
        let kind = if tree {
            git2::ObjectType::Tree
        } else {
            git2::ObjectType::Blob
        };
        crate::object::copy_verified(input, Tee(&mut output, &mut metadata), kind, size, oid)?;
        output.finish()?;
        self.publish(
            oid,
            Location {
                offset,
                size: size as u64,
                tags,
                generation,
                tree,
            },
            false,
            directory,
        )
    }
    fn grow(&mut self, directory: &Path) -> Result<()> {
        let slots = self
            .slots
            .checked_mul(2)
            .context("raw-blob index capacity")?;
        let index = self.files.file(directory)?;
        index.set_len(
            slots
                .checked_mul(SLOT_BYTES)
                .context("raw-blob index size")?,
        )?;
        for old_slot in 0..self.slots {
            crate::object::check_transfer()?;
            let record = read_slot(&self.index, old_slot)?;
            if record[0] == 0 {
                continue;
            }
            self.verify(&record)?;
            let oid = Oid::from_bytes(&record[1..21])?;
            let mut slot = self.hash.hash_one(oid) & (slots - 1);
            while read_slot(&index, slot)?[0] != 0 {
                slot = (slot + 1) & (slots - 1);
            }
            index.write_all_at(&record, slot * SLOT_BYTES)?;
        }
        self.index = index;
        self.slots = slots;
        Ok(())
    }
    pub(crate) fn insert(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        directory: &Path,
    ) -> Result<RawBlob> {
        self.insert_kind(oid, size, input, directory, git2::ObjectType::Blob)
    }
    pub(crate) fn insert_tree(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        directory: &Path,
    ) -> Result<RawBlob> {
        self.insert_kind(oid, size, input, directory, git2::ObjectType::Tree)
    }
    fn insert_kind(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        directory: &Path,
        kind: git2::ObjectType,
    ) -> Result<RawBlob> {
        if let Some(blob) = self.get(oid)? {
            return Ok(blob);
        }
        let offset = (&*self.data).seek(SeekFrom::End(0))?;
        let tags = (&*self.tags).seek(SeekFrom::End(0))?;
        let generation = *uuid::Uuid::new_v4().as_bytes();
        let outcome = {
            let mut output = StoreAndTag {
                data: &self.data,
                tags: Tags::new(&self.tags, self.key.as_ref(), oid, size as u64, generation),
            };
            crate::object::copy_verified(input, &mut output, kind, size, oid)
                .and_then(|()| output.tags.finish())
        };
        if let Err(error) = outcome {
            self.data
                .set_len(offset)
                .context("roll back incomplete raw blob")?;
            self.tags
                .set_len(tags)
                .context("roll back incomplete raw tags")?;
            return Err(error);
        }
        let location = Location {
            offset,
            size: size as u64,
            tags,
            generation,
            tree: kind == git2::ObjectType::Tree,
        };
        self.publish(oid, location, true, directory)?;
        Ok(self.blob(oid, location))
    }
    fn publish(
        &mut self,
        oid: Oid,
        location: Location,
        used: bool,
        directory: &Path,
    ) -> Result<()> {
        if self.get(oid)?.is_some() {
            return Ok(());
        }
        if self.indexed_entries >= self.slots / 2 {
            self.grow(directory)?;
        }
        let mut record = [0; SLOT_BYTES as usize];
        record[0] = 1;
        record[1..21].copy_from_slice(oid.as_bytes());
        record[21..29].copy_from_slice(&location.offset.to_le_bytes());
        record[29..37].copy_from_slice(&location.size.to_le_bytes());
        record[37..45].copy_from_slice(&location.tags.to_le_bytes());
        record[45] = u8::from(used);
        record[46..62].copy_from_slice(&location.generation);
        record[62] = u8::from(location.tree);
        self.seal(&mut record);
        let (slot, _) = self.locate(oid)?;
        self.index.write_all_at(&record, slot * SLOT_BYTES)?;
        self.indexed_entries += 1;
        if location.tree {
            self.tree_bytes += location.size;
        } else {
            self.blob_entries += 1;
            self.blob_bytes += location.size;
        }
        Ok(())
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn oids(&self) -> Result<Vec<Oid>> {
        let mut oids = Vec::new();
        for slot in 0..self.slots {
            let record = read_slot(&self.index, slot)?;
            if record[0] != 0 {
                self.verify(&record)?;
                oids.push(Oid::from_bytes(&record[1..21])?);
            }
        }
        Ok(oids)
    }
    pub(crate) fn entries(&self) -> u64 {
        self.blob_entries
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.blob_bytes
    }
    pub(crate) fn tree_bytes(&self) -> u64 {
        self.tree_bytes
    }
}
struct Incoming {
    oid: Oid,
    offset: u64,
    size: usize,
    tree: bool,
}
struct Tee<A, B>(A, B);
impl<A: Write, B: Write> Write for Tee<A, B> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(bytes)?;
        self.1.write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()?;
        self.1.flush()
    }
}
pub(crate) struct RawOutput {
    file: Arc<Spool>,
}
impl Write for RawOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (&*self.file).write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn read_slot(index: &Spool, slot: u64) -> Result<[u8; SLOT_BYTES as usize]> {
    let mut record = [0; SLOT_BYTES as usize];
    index.read_exact_at(&mut record, slot * SLOT_BYTES)?;
    Ok(record)
}
fn decode_location(record: &[u8; SLOT_BYTES as usize]) -> Location {
    Location {
        offset: u64::from_le_bytes(record[21..29].try_into().unwrap()),
        size: u64::from_le_bytes(record[29..37].try_into().unwrap()),
        tags: u64::from_le_bytes(record[37..45].try_into().unwrap()),
        generation: record[46..62].try_into().unwrap(),
        tree: record[62] != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn authenticated_ranges_cross_finite_parts_and_outlive_the_store() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::with_limit(root.path(), Some(97)).unwrap();
        let mut store = RawBlobs::with_files(root.path(), files).unwrap();
        let bytes: Vec<_> = (0..10000).map(|i| (i % 251) as u8).collect();
        let oid = Oid::hash_object(git2::ObjectType::Blob, &bytes).unwrap();
        let reader = store
            .insert(oid, bytes.len(), bytes.as_slice(), root.path())
            .unwrap();
        assert_eq!(reader.read(4091, 800).unwrap(), bytes[4091..4891]);
        drop(store);
        assert_eq!(reader.read(96, 700).unwrap(), bytes[96..796]);
        drop(reader);
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }
    #[test]
    fn modified_prepared_bytes_are_rejected_before_a_range_is_returned() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let oid = Oid::hash_object(git2::ObjectType::Blob, b"good").unwrap();
        let reader = store
            .insert(oid, 4, b"good".as_slice(), directory.path())
            .unwrap();
        store
            .data
            .write_all_at(b"evil", reader.location.offset)
            .unwrap();
        assert!(
            reader.read(0, 1).is_err(),
            "prepared storage mutation exposed unverified bytes"
        );
    }
    #[test]
    fn tags_from_failed_preparation_cannot_authenticate_a_later_generation() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let good = vec![b'g'; BLOCK as usize];
        let evil = vec![b'e'; BLOCK as usize];
        let oid = Oid::hash_object(git2::ObjectType::Blob, &good).unwrap();
        let (mut output, start) = store.append_output().unwrap();
        output.write_all(&evil).unwrap();
        drop(output);
        let input = store.output_reader(start).unwrap();
        assert!(store
            .import_blob(oid, start, evil.len(), input, directory.path())
            .is_err());
        let mut failed_tag = [0; 32];
        store.tags.read_exact_at(&mut failed_tag, 0).unwrap();
        let reader = store
            .insert(oid, good.len(), good.as_slice(), directory.path())
            .unwrap();
        store
            .data
            .write_all_at(&evil, reader.location.offset)
            .unwrap();
        store
            .tags
            .write_all_at(&failed_tag, reader.location.tags)
            .unwrap();
        assert!(reader.read(0, 1).is_err());
    }
    #[test]
    fn modified_index_and_tags_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let oid = Oid::hash_object(git2::ObjectType::Blob, b"good").unwrap();
        let reader = store
            .insert(oid, 4, b"good".as_slice(), directory.path())
            .unwrap();
        store
            .tags
            .write_all_at(&[0; 32], reader.location.tags)
            .unwrap();
        assert!(reader.read_all().is_err());
        let (slot, _) = store.locate(oid).unwrap();
        store
            .index
            .write_all_at(&[255], slot * SLOT_BYTES + 21)
            .unwrap();
        assert!(store.get(oid).is_err());
        assert!(store.for_read(oid).is_err());
    }
    #[test]
    fn native_output_is_verified_in_place_and_read_marking_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let (mut output, start) = store.append_output().unwrap();
        output.write_all(b"headergood\n").unwrap();
        drop(output);
        let oid = Oid::hash_object(git2::ObjectType::Blob, b"good").unwrap();
        let input = store.output_reader(start + 6).unwrap().take(4);
        store
            .import_blob(oid, start + 6, 4, input, directory.path())
            .unwrap();
        let (first, prepared) = store.for_read(oid).unwrap().unwrap();
        assert!(prepared);
        assert_eq!(first.read_all().unwrap(), b"good");
        drop(first);
        let (again, prepared) = store.for_read(oid).unwrap().unwrap();
        assert!(!prepared);
        assert_eq!(again.read_all().unwrap(), b"good");
        let wrong = Oid::hash_object(git2::ObjectType::Blob, b"evil").unwrap();
        let input = store.output_reader(start + 6).unwrap().take(4);
        assert!(store
            .import_blob(wrong, start + 6, 4, input, directory.path())
            .is_err());
        assert!(store.get(wrong).unwrap().is_none());
        assert_eq!(store.entries(), 1);
    }
    #[test]
    fn growing_index_retains_verified_bytes_without_per_object_fds() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        for i in 0..257 {
            let bytes = format!("blob-{i}");
            let oid = Oid::hash_object(git2::ObjectType::Blob, bytes.as_bytes()).unwrap();
            store
                .insert(oid, bytes.len(), bytes.as_bytes(), directory.path())
                .unwrap();
        }
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| entry.unwrap().file_type().unwrap().is_dir()));
        assert_eq!(store.entries(), 257);
        for i in (0..257).rev() {
            let bytes = format!("blob-{i}");
            let oid = Oid::hash_object(git2::ObjectType::Blob, bytes.as_bytes()).unwrap();
            let blob = store.get(oid).unwrap().unwrap();
            assert_eq!(blob.read(0, u32::MAX).unwrap(), bytes.as_bytes());
            assert_eq!(blob.read(u64::MAX, 1).unwrap(), b"");
            assert_eq!(
                blob.read(blob.len() - 1, 100).unwrap(),
                &bytes.as_bytes()[bytes.len() - 1..]
            );
        }
        drop(store);
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none());
    }
    #[test]
    fn invalid_and_truncated_transfers_do_not_publish_or_damage_previous_data() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = RawBlobs::new(directory.path()).unwrap();
        let oid = Oid::hash_object(git2::ObjectType::Blob, b"good").unwrap();
        let good = store
            .insert(oid, 4, b"good".as_slice(), directory.path())
            .unwrap();
        let invalid = Oid::hash_object(git2::ObjectType::Blob, b"next").unwrap();
        for data in [b"evil".as_slice(), b"n", b"next-extra"] {
            assert!(store.insert(invalid, 4, data, directory.path()).is_err());
            assert!(store.get(invalid).unwrap().is_none());
            assert_eq!(good.read(0, 100).unwrap(), b"good");
            assert_eq!(store.bytes(), 4);
        }
        let next = store
            .insert(invalid, 4, b"next".as_slice(), directory.path())
            .unwrap();
        assert_eq!(next.read(0, 4).unwrap(), b"next");
    }
}
