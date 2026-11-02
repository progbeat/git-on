use std::mem::align_of;

use crate::ll::fuse_abi as abi;
use crate::session::MAX_WRITE_SIZE;

/// Size of the buffer for reading a request from the kernel. Since the kernel may send
/// up to `MAX_WRITE_SIZE` bytes in a write request, we use that value plus some extra space.
const BUFFER_SIZE: usize = MAX_WRITE_SIZE + 4096;

/// A buffer that provides an aligned sub-slice for FUSE operations.
///
/// This struct wraps a `Vec<u8>` and provides access to an aligned portion
/// of the buffer, ensuring proper alignment for `fuse_in_header`.
#[derive(Debug)]
pub(crate) struct FuseReadBuf {
    buffer: Vec<u8>,
}

impl FuseReadBuf {
    /// Creates a new `FuseReadBuf` with the default buffer size.
    ///
    /// The actual buffer may be slightly larger to accommodate alignment requirements.
    pub(crate) fn new() -> Self {
        Self::with_capacity(BUFFER_SIZE)
    }

    pub(crate) fn with_capacity(size: usize) -> Self {
        Self {
            // Linux requires this minimum even for the first INIT read.
            buffer: vec![0; size.max(8192) + align_of::<abi::fuse_in_header>()],
        }
    }

    /// Returns a mutable reference to the aligned portion of the buffer.
    pub(crate) fn as_mut(&mut self) -> &mut [u8] {
        let alignment = align_of::<abi::fuse_in_header>();
        let off = alignment - (self.buffer.as_ptr() as usize) % alignment;
        if off == alignment {
            &mut self.buffer
        } else {
            &mut self.buffer[off..]
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn small_negotiated_requests_still_obey_the_linux_minimum() {
        // Linux fuse_dev_do_read rejects a smaller buffer with EINVAL,
        // including the first INIT read before max_write is negotiated.
        for requested in [0, 1, 4096, 8192] {
            assert!(FuseReadBuf::with_capacity(requested).as_mut().len() >= 8192);
        }
    }
}
