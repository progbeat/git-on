//! Response data implementation of [`crate::AsyncFilesystem::read`] operation to
//! send to the kernel

use std::io::IoSlice;
use std::sync::Arc;

use crate::ll::{ioslice_concat::IosliceConcat, reply::Response};

/// Internal representation of the response data, which
/// can be either owned or shared.
#[derive(Debug)]
enum ReadResponseData {
    Owned(Vec<u8>),
    Shared {
        data: Arc<[u8]>,
        start: usize,
        end: usize,
    },
    Local {
        data: std::rc::Rc<Vec<u8>>,
        start: usize,
        end: usize,
    },
}

/// Response data from [`crate::AsyncFilesystem::read`] operation
pub struct ReadResponse {
    data: ReadResponseData,
    owner: Option<Box<dyn std::any::Any>>,
}
impl std::fmt::Debug for ReadResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadResponse")
            .field("data", &self.data)
            .finish_non_exhaustive()
    }
}

impl ReadResponse {
    /// Creates a new [`ReadResponse`] with a specified buffer.
    pub fn new(data: Vec<u8>) -> ReadResponse {
        ReadResponse {
            data: ReadResponseData::Owned(data),
            owner: None,
        }
    }

    /// Creates a [`ReadResponse`] backed by a slice of shared data without copying it.
    ///
    /// The requested range is clamped to the bounds of `data`.
    pub fn from_shared_slice(data: Arc<[u8]>, offset: usize, size: usize) -> ReadResponse {
        let start = offset.min(data.len());
        let end = start.saturating_add(size).min(data.len());
        ReadResponse {
            data: ReadResponseData::Shared { data, start, end },
            owner: None,
        }
    }

    /// Keep immutable event-loop-owned contents alive through reply completion.
    /// The range is clamped in the same way as the thread-safe shared form.
    pub fn from_local_slice(
        data: std::rc::Rc<Vec<u8>>,
        offset: usize,
        size: usize,
    ) -> ReadResponse {
        let start = offset.min(data.len());
        let end = start.saturating_add(size).min(data.len());
        ReadResponse {
            data: ReadResponseData::Local { data, start, end },
            owner: None,
        }
    }

    /// Retain local operation ownership until the response has been sent or
    /// dropped, including while a nonblocking reply transport is waiting.
    pub fn with_owner(mut self, owner: impl std::any::Any) -> Self {
        self.owner = Some(Box::new(owner));
        self
    }

    pub(super) fn bytes(&self) -> &[u8] {
        match &self.data {
            ReadResponseData::Owned(data) => data.as_slice(),
            ReadResponseData::Shared { data, start, end } => &data[*start..*end],
            ReadResponseData::Local { data, start, end } => &data[*start..*end],
        }
    }
}

impl Response for ReadResponse {
    fn payload(&self) -> impl IosliceConcat {
        [IoSlice::new(self.bytes())]
    }
}

#[cfg(test)]
mod tests {
    use super::ReadResponse;
    use std::sync::Arc;

    #[test]
    fn response_owns_local_guard_through_payload_borrow() {
        let owner = std::rc::Rc::new(());
        let response = ReadResponse::new(b"data".to_vec()).with_owner(owner.clone());
        assert_eq!(std::rc::Rc::strong_count(&owner), 2);
        assert_eq!(response.bytes(), b"data");
        drop(response);
        assert_eq!(std::rc::Rc::strong_count(&owner), 1);
    }

    #[test]
    fn shared_slice_clamps_to_bounds() {
        let response = ReadResponse::from_shared_slice(Arc::from(&b"abcdef"[..]), 2, 99);
        assert_eq!(response.bytes(), b"cdef");
    }

    #[test]
    fn shared_slice_handles_offset_past_end() {
        let response = ReadResponse::from_shared_slice(Arc::from(&b"abcdef"[..]), 99, 4);
        assert_eq!(response.bytes(), b"");
    }

    #[test]
    fn local_slice_keeps_original_allocation_alive_without_copying() {
        let data = std::rc::Rc::new(b"abcdef".to_vec());
        let ptr = data.as_ptr();
        let response = ReadResponse::from_local_slice(data.clone(), 2, usize::MAX);
        assert_eq!(std::rc::Rc::strong_count(&data), 2);
        drop(data);
        assert_eq!(response.bytes(), b"cdef");
        assert_eq!(response.bytes().as_ptr(), unsafe { ptr.add(2) });
    }
}
