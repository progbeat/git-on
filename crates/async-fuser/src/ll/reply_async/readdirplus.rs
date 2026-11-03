//! Response data implementation of [`crate::AsyncFilesystem::readdirplus`] operation to
//! send to the kernel

use std::ffi::OsStr;
use std::io::IoSlice;
use std::time::Duration;

use crate::FileAttr;
use crate::Generation;
use crate::INodeNo;
use crate::ll::ioslice_concat::IosliceConcat;
use crate::ll::reply::DirEntOffset;
use crate::ll::reply::DirEntPlusList;
use crate::ll::reply::DirEntryPlus;
use crate::ll::reply::Response;

/// Response data from [`crate::AsyncFilesystem::readdirplus`] operation
pub struct DirectoryPlusResponse {
    data: DirEntPlusList,
    owner: Option<Box<dyn std::any::Any>>,
}
impl std::fmt::Debug for DirectoryPlusResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryPlusResponse")
            .field("data", &self.data)
            .finish_non_exhaustive()
    }
}

impl DirectoryPlusResponse {
    /// Creates a new [`DirectoryPlusResponse`] with a specified buffer size.
    pub fn new(size: usize) -> DirectoryPlusResponse {
        DirectoryPlusResponse {
            data: DirEntPlusList::new(size),
            owner: None,
        }
    }

    /// Encoded, padded record length, for planning bounded attribute demands.
    pub fn entry_size(name: &OsStr) -> usize {
        DirEntPlusList::entry_size(name)
    }

    /// Keep namespace/attribute ownership until the reply finishes or drops.
    pub fn with_owner(mut self, owner: impl std::any::Any) -> Self {
        self.owner = Some(Box::new(owner));
        self
    }

    /// Add an entry to the directory reply buffer. Returns true if the buffer is full.
    #[must_use]
    pub fn add<T: AsRef<OsStr>>(
        &mut self,
        ino: INodeNo,
        offset: u64,
        name: T,
        ttl: &Duration,
        attr: &FileAttr,
        generation: Generation,
    ) -> bool {
        self.add_with_ttls(ino, offset, name, (*ttl, *ttl), attr, generation)
    }

    /// Add an entry with independent (name binding, attribute) lifetimes.
    /// A zero node ID lists the name without adding a lookup reference;
    /// the ordinary directory inode number still comes from `attr.ino`.
    #[must_use]
    pub fn add_with_ttls<T: AsRef<OsStr>>(
        &mut self,
        ino: INodeNo,
        offset: u64,
        name: T,
        ttls: (Duration, Duration),
        attr: &FileAttr,
        generation: Generation,
    ) -> bool {
        let name = name.as_ref();
        self.data.push(&DirEntryPlus::new(
            ino,
            generation,
            DirEntOffset(offset),
            name,
            ttls.0,
            attr.into(),
            ttls.1,
        ))
    }
}

impl Response for DirectoryPlusResponse {
    fn payload(&self) -> impl IosliceConcat {
        [IoSlice::new(self.data.as_bytes())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zero_nodeid_lists_a_name_without_adding_a_lookup_reference() {
        let time = std::time::UNIX_EPOCH;
        let attr = FileAttr {
            ino: INodeNo(7),
            size: 3,
            blocks: 1,
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: crate::FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: 1,
            gid: 1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        let mut response = DirectoryPlusResponse::new(4096);
        assert!(!response.add(INodeNo(0), 3, "file", &Duration::ZERO, &attr, Generation(0)));
        response.with_iovec(crate::RequestId(1), |bufs| {
            let bytes: Vec<_> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
            assert_eq!(u64::from_ne_bytes(bytes[16..24].try_into().unwrap()), 0);
            let directory = 16 + std::mem::size_of::<crate::ll::fuse_abi::fuse_entry_out>();
            assert_eq!(
                u64::from_ne_bytes(bytes[directory..directory + 8].try_into().unwrap()),
                7
            );
        });
    }
    #[test]
    fn planned_capacity_independent_ttls_and_owner_match_the_wire_reply() {
        use std::os::unix::ffi::OsStrExt;
        let time = std::time::UNIX_EPOCH;
        let attr = FileAttr {
            ino: INodeNo(7),
            size: 3,
            blocks: 1,
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: crate::FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: 1,
            gid: 1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        for bytes in [vec![b'a'], vec![b'b'; 7], vec![b'c'; 8], vec![255; 255]] {
            let name = OsStr::from_bytes(&bytes);
            let size = DirectoryPlusResponse::entry_size(name);
            let mut short = DirectoryPlusResponse::new(size - 1);
            assert!(short.add(INodeNo(7), 3, name, &Duration::ZERO, &attr, Generation(0)));
            let owner = std::rc::Rc::new(());
            let mut reply = DirectoryPlusResponse::new(size).with_owner(owner.clone());
            assert!(!reply.add_with_ttls(
                INodeNo(7),
                3,
                name,
                (Duration::from_secs(11), Duration::ZERO),
                &attr,
                Generation(0)
            ));
            reply.with_iovec(crate::RequestId(1), |bufs| {
                let wire: Vec<_> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
                assert_eq!(wire.len(), size + 16);
                assert_eq!(u64::from_ne_bytes(wire[32..40].try_into().unwrap()), 11);
                assert_eq!(u64::from_ne_bytes(wire[40..48].try_into().unwrap()), 0);
                assert_eq!(std::rc::Rc::strong_count(&owner), 2);
            });
            drop(reply);
            assert_eq!(std::rc::Rc::strong_count(&owner), 1);
        }
    }
}
