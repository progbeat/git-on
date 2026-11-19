use super::{
    protocol::{Attributes, MAX_NAME},
    server::{Server, LEASE_SECONDS},
    status::*,
    xdr::{Bitmap, Decode, InvalidXdr, Reader, Writer, MAX_DATA},
};
use crate::backend::{FileType, SetAttributes, TimeOrNow};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GET: &[u32] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 16, 17, 18, 19, 20, 21, 22, 23, 26, 27, 28, 29,
    30, 31, 33, 34, 35, 36, 37, 41, 42, 43, 44, 45, 47, 50, 51, 52, 53, 55, 75, 76,
];
const SET: &[u32] = &[4, 33, 36, 37, 48, 50, 54];
const SIZE: u32 = 4;
const USED: u32 = 45;
const RDATTR_ERROR: u32 = 11;

pub(super) fn file_type(kind: FileType) -> u32 {
    match kind {
        FileType::RegularFile => 1,
        FileType::Directory => 2,
        FileType::BlockDevice => 3,
        FileType::CharDevice => 4,
        FileType::Symlink => 5,
        FileType::Socket => 6,
        FileType::NamedPipe => 7,
    }
}

fn time(w: &mut Writer, time: SystemTime) {
    let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => (delta.as_secs() as i64, delta.subsec_nanos()),
        Err(error) => {
            let delta = error.duration();
            if delta.subsec_nanos() == 0 {
                (-(i128::from(delta.as_secs())) as i64, 0)
            } else {
                (
                    (-i128::from(delta.as_secs()) - 1) as i64,
                    1_000_000_000 - delta.subsec_nanos(),
                )
            }
        }
    };
    w.u64(seconds as u64);
    w.u32(nanos);
}

fn decode_time(r: &mut Reader<'_>) -> Decode<SystemTime> {
    let seconds = r.u64()? as i64;
    let nanos = r.u32()?;
    if nanos >= 1_000_000_000 {
        return Err(InvalidXdr);
    }
    let whole = Duration::from_secs(seconds.unsigned_abs());
    let time = if seconds >= 0 {
        UNIX_EPOCH.checked_add(whole)
    } else {
        UNIX_EPOCH.checked_sub(whole)
    };
    time.and_then(|t| t.checked_add(Duration::from_nanos(u64::from(nanos))))
        .ok_or(InvalidXdr)
}

fn set_time(r: &mut Reader<'_>) -> Decode<TimeOrNow> {
    match r.u32()? {
        0 => Ok(TimeOrNow::Now),
        1 => Ok(TimeOrNow::SpecificTime(decode_time(r)?)),
        _ => Err(InvalidXdr),
    }
}

fn identity(r: &mut Reader<'_>) -> Result<u32, Status> {
    let owner = r.opaque(64).map_err(|_| BADXDR)?;
    std::str::from_utf8(owner)
        .ok()
        .filter(|value| value.bytes().all(|c| c.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
        .ok_or(BADOWNER)
}

pub(super) fn decode_set(attrs: Attributes<'_>) -> Result<SetAttributes, Status> {
    if attrs.mask.intersection(Bitmap::from_indices(SET)) != attrs.mask {
        return Err(ATTRNOTSUPP);
    }
    let mut fields = SetAttributes::default();
    let mut r = Reader::new(attrs.data);
    for bit in attrs.mask.iter() {
        match bit {
            4 => {
                let size = r.u64().map_err(|_| BADXDR)?;
                if size > i64::MAX as u64 {
                    return Err(FBIG);
                }
                fields.size = Some(size);
            }
            33 => fields.mode = Some(r.u32().map_err(|_| BADXDR)?),
            36 => fields.owner = Some(identity(&mut r)?),
            37 => fields.group = Some(identity(&mut r)?),
            48 => fields.atime = Some(set_time(&mut r).map_err(|_| BADXDR)?),
            50 => fields.crtime = Some(decode_time(&mut r).map_err(|_| BADXDR)?),
            54 => fields.mtime = Some(set_time(&mut r).map_err(|_| BADXDR)?),
            _ => unreachable!(),
        }
    }
    r.finish().map_err(|_| BADXDR)?;
    Ok(fields)
}

impl Server {
    pub fn get_attributes(
        &mut self,
        ino: u64,
        requested: Bitmap,
        readdir: bool,
        w: &mut Writer,
    ) -> Result<(), Status> {
        if [48, 54].into_iter().any(|bit| requested.contains(bit))
            || (!readdir && requested.contains(RDATTR_ERROR))
        {
            return Err(INVAL);
        }
        let mut mask = requested.intersection(Bitmap::from_indices(GET));
        let has_size = mask.contains(SIZE) || mask.contains(USED);
        let (stat, size_known) = self
            .core
            .nfs_attributes(ino, has_size && !readdir)
            .map_err(errno)?;
        if readdir && !size_known {
            // A remote Git blob has no separately fetchable size. Return the
            // available directory attributes and let GETATTR size a named file.
            mask.clear(SIZE);
            mask.clear(USED);
        }
        let stats = [21, 22, 23, 42, 43, 44]
            .into_iter()
            .any(|bit| mask.contains(bit))
            .then(|| self.core.statistics().map_err(errno))
            .transpose()?;
        let mut values = Writer::default();
        for bit in mask.iter() {
            match bit {
                0 => Bitmap::from_indices(GET)
                    .union(Bitmap::from_indices(SET))
                    .encode(&mut values),
                1 => values.u32(file_type(stat.kind)),
                // Handles remain valid for the lifetime of this filesystem.
                2 => values.u32(0),
                3 => values.u64(self.core.nfs_change(ino).map_err(errno)?),
                SIZE => values.u64(stat.size),
                5 | 6 | 9 | 15 | 17 | 18 | 26 | 34 => values.boolean(true),
                7 | 16 => values.boolean(false),
                8 => values.fixed(&self.run),
                10 => values.u32(LEASE_SECONDS),
                RDATTR_ERROR | 13 | 76 => values.u32(0),
                19 => values.opaque(&self.filehandle(ino)),
                20 | 55 => values.u64(ino),
                21 | 22 => values.u64(stats.as_ref().unwrap().free_files),
                23 => values.u64(stats.as_ref().unwrap().files),
                27 => values.u64(i64::MAX as u64),
                28 => values.u32(u32::MAX),
                29 => values.u32(MAX_NAME as u32),
                30 | 31 => values.u64(MAX_DATA as u64),
                33 => values.u32(u32::from(stat.perm)),
                35 => values.u32(stat.nlink),
                36 => values.opaque(stat.uid.to_string().as_bytes()),
                37 => values.opaque(stat.gid.to_string().as_bytes()),
                41 => {
                    values.u32(stat.rdev >> 24);
                    values.u32(stat.rdev & 0x00ff_ffff);
                }
                42..=44 => {
                    let stats = stats.as_ref().unwrap();
                    let blocks = match bit {
                        42 => stats.available,
                        43 => stats.free,
                        _ => stats.blocks,
                    };
                    values.u64(blocks.saturating_mul(u64::from(stats.fragment_size.max(1))));
                }
                USED => values.u64(stat.blocks.saturating_mul(512)),
                47 => time(&mut values, stat.atime),
                50 => time(&mut values, stat.crtime),
                51 => time(&mut values, UNIX_EPOCH + Duration::from_nanos(1)),
                52 => time(&mut values, stat.ctime),
                53 => time(&mut values, stat.mtime),
                75 => Bitmap::from_indices(SET).encode(&mut values),
                _ => unreachable!(),
            }
        }
        mask.encode(w);
        w.opaque(values.bytes());
        Ok(())
    }

    pub fn verify_attributes(
        &mut self,
        ino: u64,
        attrs: Attributes<'_>,
        negated: bool,
    ) -> Result<(), Status> {
        if attrs.mask.intersection(Bitmap::from_indices(GET)) != attrs.mask {
            return Err(ATTRNOTSUPP);
        }
        let mut actual = Writer::default();
        self.get_attributes(ino, attrs.mask, false, &mut actual)?;
        let mut r = Reader::new(actual.bytes());
        Bitmap::decode(&mut r).map_err(|_| SERVERFAULT)?;
        let actual = r.opaque(MAX_DATA).map_err(|_| SERVERFAULT)?;
        match (actual == attrs.data, negated) {
            (true, true) => Err(SAME),
            (false, false) => Err(NOT_SAME),
            _ => Ok(()),
        }
    }
}

pub(super) fn directory_attribute_error(
    mask: Bitmap,
    status: Status,
    w: &mut Writer,
) -> Result<(), Status> {
    if !mask.contains(RDATTR_ERROR) {
        return Err(status);
    }
    Bitmap::from_indices(&[RDATTR_ERROR]).encode(w);
    w.opaque(&status.to_be_bytes());
    Ok(())
}
