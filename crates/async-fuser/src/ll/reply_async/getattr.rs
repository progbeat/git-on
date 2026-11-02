//! Response data implementation of [`crate::AsyncFilesystem::getattr`] operation to
//! send to the kernel

use std::time::Duration;

use crate::FileAttr;
use crate::ll::ResponseStruct;
use crate::ll::ioslice_concat::IosliceConcat;
use crate::ll::reply::Attr;
use crate::ll::reply::Response;

/// Response data from [`crate::AsyncFilesystem::getattr`] operation
pub struct GetAttrResponse {
    ttl: Duration,
    attr: FileAttr,
    owner: Option<Box<dyn std::any::Any>>,
}

impl std::fmt::Debug for GetAttrResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetAttrResponse")
            .field("attr", &self.attr)
            .finish_non_exhaustive()
    }
}

impl GetAttrResponse {
    /// Creates a `GetAttrResponse` object
    pub fn new(ttl: Duration, attr: FileAttr) -> Self {
        Self {
            ttl,
            attr,
            owner: None,
        }
    }

    /// Retain local metadata ownership through nonblocking reply completion.
    pub fn with_owner(mut self, owner: impl std::any::Any) -> Self {
        self.owner = Some(Box::new(owner));
        self
    }
}

impl Response for GetAttrResponse {
    fn payload(&self) -> impl IosliceConcat {
        ResponseStruct::new_attr(&self.ttl, &Attr::from(self.attr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attribute_reply_retains_local_owner_through_wire_payload() {
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
        let response =
            GetAttrResponse::new(Duration::from_secs(86400), attr).with_owner(owner.clone());
        response.with_iovec(crate::RequestId(1), |bufs| {
            let bytes: Vec<_> = bufs.iter().flat_map(|buf| buf.iter().copied()).collect();
            assert_eq!(u64::from_ne_bytes(bytes[16..24].try_into().unwrap()), 86400);
            assert_eq!(std::rc::Rc::strong_count(&owner), 2);
        });
        drop(response);
        assert_eq!(std::rc::Rc::strong_count(&owner), 1);
    }
}
