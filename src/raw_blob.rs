//! Verified per-invocation bytes, with a disk-resident randomized hash index.
//!
//! Both files are unlinked immediately, like the previous active-reader spool.
//! Payloads survive close/reopen without retaining one FD or RAM entry per OID.
//! Linear probing stays below half occupancy; geometric rehashes give expected
//! amortized O(1) metadata work per object (plus bytes copied/verified).
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    collections::hash_map::RandomState,
    fs::File,
    hash::BuildHasher,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    path::Path,
    sync::Arc,
};

const SLOT_BYTES: u64 = 40;
const INITIAL_SLOTS: u64 = 64;

#[derive(Clone, Copy)]
struct Location {
    offset: u64,
    size: u64,
}
#[derive(Clone)]
pub(crate) struct RawBlob {
    file: Arc<File>,
    location: Location,
}
impl RawBlob {
    pub(crate) fn len(&self) -> u64 {
        self.location.size
    }
    pub(crate) fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        let length = self
            .location
            .size
            .saturating_sub(offset)
            .min(u64::from(size));
        let mut bytes = vec![0; length as usize];
        if !bytes.is_empty() {
            self.file
                .read_exact_at(&mut bytes, self.location.offset + offset)
                .context("read verified run-local blob")?;
        }
        Ok(bytes)
    }
}

pub(crate) struct RawBlobs {
    data: Arc<File>,
    index: File,
    slots: u64,
    hash: RandomState,
    entries: u64,
    bytes: u64,
}
impl RawBlobs {
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        let data = Arc::new(tempfile::tempfile_in(directory)?);
        let index = tempfile::tempfile_in(directory)?;
        index.set_len(INITIAL_SLOTS * SLOT_BYTES)?;
        Ok(Self {
            data,
            index,
            slots: INITIAL_SLOTS,
            hash: RandomState::new(),
            entries: 0,
            bytes: 0,
        })
    }
    fn locate(&self, oid: Oid) -> Result<(u64, Option<Location>)> {
        let mut slot = self.hash.hash_one(oid) & (self.slots - 1);
        for _ in 0..self.slots {
            let record = read_slot(&self.index, slot)?;
            if record[0] == 0 {
                return Ok((slot, None));
            }
            if record[0] != 1 {
                bail!("invalid private raw-blob index");
            }
            if &record[1..21] == oid.as_bytes() {
                return Ok((slot, Some(decode_location(&record))));
            }
            slot = (slot + 1) & (self.slots - 1);
        }
        bail!("private raw-blob index is full")
    }
    pub(crate) fn get(&self, oid: Oid) -> Result<Option<RawBlob>> {
        Ok(self.locate(oid)?.1.map(|location| RawBlob {
            file: self.data.clone(),
            location,
        }))
    }
    fn grow(&mut self, directory: &Path) -> Result<()> {
        let slots = self
            .slots
            .checked_mul(2)
            .context("raw-blob index capacity")?;
        let index = tempfile::tempfile_in(directory)?;
        index.set_len(
            slots
                .checked_mul(SLOT_BYTES)
                .context("raw-blob index size")?,
        )?;
        // Build the replacement completely before publishing it. Failure leaves
        // all existing entries readable, with no crash-recovery database.
        for old_slot in 0..self.slots {
            crate::object::check_transfer()?;
            let record = read_slot(&self.index, old_slot)?;
            if record[0] == 0 {
                continue;
            }
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
    /// The transfer writes each raw payload once and publishes its locator only
    /// after the canonical Git OID has been verified. An interrupted or invalid
    /// transfer cannot replace any previously verified entry.
    pub(crate) fn insert(
        &mut self,
        oid: Oid,
        size: usize,
        input: impl Read,
        directory: &Path,
    ) -> Result<RawBlob> {
        if let Some(blob) = self.get(oid)? {
            return Ok(blob);
        }
        if self.entries >= self.slots / 2 {
            self.grow(directory)?;
        }
        let offset = (&*self.data).seek(SeekFrom::End(0))?;
        if let Err(error) =
            crate::object::copy_verified(input, &*self.data, git2::ObjectType::Blob, size, oid)
        {
            self.data
                .set_len(offset)
                .context("roll back incomplete raw blob")?;
            return Err(error);
        }
        let location = Location {
            offset,
            size: size as u64,
        };
        let mut record = [0; SLOT_BYTES as usize];
        record[0] = 1;
        record[1..21].copy_from_slice(oid.as_bytes());
        record[21..29].copy_from_slice(&location.offset.to_le_bytes());
        record[29..37].copy_from_slice(&location.size.to_le_bytes());
        let (slot, _) = self.locate(oid)?;
        self.index.write_all_at(&record, slot * SLOT_BYTES)?;
        self.entries += 1;
        self.bytes += size as u64;
        Ok(RawBlob {
            file: self.data.clone(),
            location,
        })
    }
    pub(crate) fn entries(&self) -> u64 {
        self.entries
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}
fn read_slot(index: &File, slot: u64) -> Result<[u8; SLOT_BYTES as usize]> {
    let mut record = [0; SLOT_BYTES as usize];
    index.read_exact_at(&mut record, slot * SLOT_BYTES)?;
    Ok(record)
}
fn decode_location(record: &[u8; SLOT_BYTES as usize]) -> Location {
    Location {
        offset: u64::from_le_bytes(record[21..29].try_into().unwrap()),
        size: u64::from_le_bytes(record[29..37].try_into().unwrap()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn growing_index_retains_verified_bytes_without_named_files_or_per_object_fds() {
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
            .next()
            .is_none());
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
