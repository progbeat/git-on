//! Response data implementation of [`crate::AsyncFilesystem::lookup`] operation to
//! send to the kernel

#![allow(missing_docs, missing_debug_implementations)]

use std::time::Duration;

use crate::FileAttr;
use crate::Generation;
use crate::ll::ResponseStruct;
use crate::ll::ioslice_concat::IosliceConcat;
use crate::ll::reply::Attr;
use crate::ll::reply::Response;

/// Response data from [`crate::AsyncFilesystem::lookup`] operation
pub struct LookupResponse {
    ttl: Duration,
    entry_ttl: Duration,
    attr: FileAttr,
    generation: Generation,
    owner: Option<Box<dyn std::any::Any>>,
}
impl std::fmt::Debug for LookupResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupResponse")
            .field("attr", &self.attr)
            .finish_non_exhaustive()
    }
}

impl LookupResponse {
    /// `ttl` is the time for which this response may be cached
    /// `attr` is the attributes of the file
    /// `generation`
    pub fn new(ttl: Duration, attr: FileAttr, generation: Generation) -> Self {
        Self {
            ttl,
            entry_ttl: ttl,
            attr,
            generation,
            owner: None,
        }
    }

    /// Cache a name binding independently of its inode's attributes.
    pub fn with_entry_ttl(mut self, ttl: Duration) -> Self {
        self.entry_ttl = ttl;
        self
    }

    /// Retain local namespace ownership through reply completion.
    pub fn with_owner(mut self, owner: impl std::any::Any) -> Self {
        self.owner = Some(Box::new(owner));
        self
    }
}

impl Response for LookupResponse {
    fn payload(&self) -> impl IosliceConcat {
        ResponseStruct::new_entry(
            self.attr.ino,
            self.generation,
            &Attr::from(self.attr),
            self.ttl,
            self.entry_ttl,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn name_ttl_is_independent_of_attributes_and_retains_owner() {
        let time = std::time::UNIX_EPOCH;
        let attr = FileAttr {
            ino: crate::INodeNo(7),
            size: 3,
            blocks: 1,
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: crate::FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 1,
            gid: 1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        let owner = std::rc::Rc::new(());
        let response = LookupResponse::new(Duration::ZERO, attr, Generation(0))
            .with_entry_ttl(Duration::from_secs(86400))
            .with_owner(owner.clone());
        response.with_iovec(crate::RequestId(1), |bufs| {
            let bytes: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
            assert_eq!(u64::from_ne_bytes(bytes[32..40].try_into().unwrap()), 86400);
            assert_eq!(u64::from_ne_bytes(bytes[40..48].try_into().unwrap()), 0);
            assert_eq!(std::rc::Rc::strong_count(&owner), 2);
        });
        drop(response);
        assert_eq!(std::rc::Rc::strong_count(&owner), 1);
    }
}
