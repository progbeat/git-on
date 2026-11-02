//! Response data implementation for operations that return an arbitrary byte
//! buffer.

use std::io::IoSlice;

use crate::ll::{ioslice_concat::IosliceConcat, reply::Response};

/// Response data from async operations that return a byte buffer.
#[derive(Debug)]
pub struct DataResponse {
    data: super::read::ReadResponse,
}

impl DataResponse {
    /// Creates a new [`DataResponse`] with the provided bytes.
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: super::read::ReadResponse::new(data),
        }
    }

    /// Return shared immutable local bytes without copying their payload.
    pub fn from_local_slice(data: std::rc::Rc<Vec<u8>>, offset: usize, size: usize) -> Self {
        Self {
            data: super::read::ReadResponse::from_local_slice(data, offset, size),
        }
    }

    fn bytes(&self) -> &[u8] {
        self.data.bytes()
    }
}

impl Response for DataResponse {
    fn payload(&self) -> impl IosliceConcat {
        [IoSlice::new(self.bytes())]
    }
}
