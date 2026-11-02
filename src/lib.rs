pub mod backend;
#[cfg(target_os = "linux")]
mod backing_async;
mod directory_view;
#[cfg(target_os = "linux")]
mod file_async;
// The mount engine is tested on macOS, but 1.3.0 ships Linux mounts only.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub mod filesystem;
pub mod git;
pub mod materialize;
pub mod object;
pub mod platform;
pub mod policy;
mod progress;
mod raw_blob;
#[cfg(target_os = "linux")]
mod raw_blob_async;
pub mod remote;
pub mod snapshot;
#[cfg(target_os = "linux")]
mod snapshot_async;
#[cfg(target_os = "linux")]
mod source_async;
mod spool;
mod transport;
mod tree;
mod wildmatch;
