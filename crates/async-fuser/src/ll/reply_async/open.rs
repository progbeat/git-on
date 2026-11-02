//! Response data implementation of async open-style operations.

use crate::FopenFlags;
use crate::ll::FileHandle;
use crate::ll::ResponseStruct;
use crate::ll::ioslice_concat::IosliceConcat;
use crate::ll::reply::Response;

/// Response data from async open-style operations.
#[derive(Debug)]
pub struct OpenResponse {
    fh: FileHandle,
    flags: FopenFlags,
    backing: Option<std::sync::Arc<crate::AsyncBackingId>>,
}

impl OpenResponse {
    /// Creates a new [`OpenResponse`].
    pub fn new(fh: FileHandle, flags: FopenFlags) -> Self {
        Self {
            fh,
            flags,
            backing: None,
        }
    }
    /// Attach a backing registration and retain it through reply transmission.
    pub fn with_backing(mut self, id: std::sync::Arc<crate::AsyncBackingId>) -> Self {
        self.flags |= FopenFlags::FOPEN_PASSTHROUGH;
        self.backing = Some(id);
        self
    }
}

impl Response for OpenResponse {
    fn payload(&self) -> impl IosliceConcat {
        ResponseStruct::new_open(
            self.fh,
            self.flags,
            self.backing.as_ref().map_or(0, |b| b.id),
        )
    }
}
