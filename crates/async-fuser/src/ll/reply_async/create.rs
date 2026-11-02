//! Response data implementation of [`crate::AsyncFilesystem::create`] operation.

use std::time::Duration;

use crate::FileAttr;
use crate::FopenFlags;
use crate::Generation;
use crate::ll::FileHandle;
use crate::ll::ResponseStruct;
use crate::ll::ioslice_concat::IosliceConcat;
use crate::ll::reply::Attr;
use crate::ll::reply::Response;

/// Response data from [`crate::AsyncFilesystem::create`] operation.
#[derive(Debug)]
pub struct CreateResponse {
    ttl: Duration,
    attr: FileAttr,
    generation: Generation,
    fh: FileHandle,
    flags: FopenFlags,
    backing: Option<std::sync::Arc<crate::AsyncBackingId>>,
}

impl CreateResponse {
    /// Creates a new [`CreateResponse`].
    pub fn new(
        ttl: Duration,
        attr: FileAttr,
        generation: Generation,
        fh: FileHandle,
        flags: FopenFlags,
    ) -> Self {
        Self {
            ttl,
            attr,
            generation,
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

impl Response for CreateResponse {
    fn payload(&self) -> impl IosliceConcat {
        ResponseStruct::new_create(
            &self.ttl,
            &Attr::from(self.attr),
            self.generation,
            self.fh,
            self.flags,
            self.backing.as_ref().map_or(0, |b| b.id),
        )
    }
}
