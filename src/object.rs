//! Verified, cancellable transfers. libgit2 streams loose objects; its pack
//! backend currently requires decoded objects (including necessary delta bases).
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};

#[derive(Debug)]
pub(crate) struct InvalidObject(pub String);
impl std::fmt::Display for InvalidObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for InvalidObject {}

pub(crate) fn check_transfer() -> Result<()> {
    if let Some(signal) = crate::git::cancelled_signal() {
        bail!("object transfer cancelled by signal {signal}");
    }
    if crate::git::operations_stopped() {
        bail!("object transfer stopped after command completion");
    }
    crate::git::check_request_interruption()
}

/// The destination is staging storage: callers must not expose it until this
/// returns successfully. The hash includes Git's canonical type/size header.
pub(crate) fn copy_verified(
    mut input: impl Read,
    mut output: impl Write,
    kind: ObjectType,
    size: usize,
    oid: Oid,
) -> Result<()> {
    let kind = kind.str();
    let mut hash = Sha1::new();
    hash.update(format!("{kind} {size}\0").as_bytes());
    let mut buffer = [0; 64 * 1024];
    let mut remaining = size;
    while remaining != 0 {
        check_transfer()?;
        let length = remaining.min(buffer.len());
        let count = input
            .read(&mut buffer[..length])
            .context("read Git object stream")?;
        if count == 0 {
            return Err(InvalidObject(format!("truncated Git object {oid}")).into());
        }
        hash.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .context("write Git object stream")?;
        remaining -= count;
    }
    check_transfer()?;
    if input.read(&mut buffer[..1])? != 0 {
        return Err(InvalidObject(format!("oversized Git object {oid}")).into());
    }
    if hash.finalize().as_slice() != oid.as_bytes() {
        return Err(InvalidObject(format!("Git object hash mismatch for {oid}")).into());
    }
    Ok(())
}

/// # Safety
/// Call once at process startup, before spawning threads or using libgit2.
/// The option modifies a libgit2 C global without synchronization.
pub unsafe fn configure_pack_descriptors() -> Result<usize> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read descriptor limit");
    }
    // Retained pack descriptors use at most a quarter of the soft limit, capped
    // at 32. The remaining descriptors serve Git startup, mounts, active files,
    // cache transfers and caller-inherited descriptors. No OS limit is changed.
    let budget = (unsafe { limit.assume_init() }.rlim_cur / 4).clamp(1, 32) as usize;
    unsafe { git2::opts::set_mwindow_file_limit(budget) }?;
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_stops_after_command_completion_without_a_signal() {
        if std::env::var_os("GIT_ON_TRANSFER_STOP_CHILD").is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "object::tests::streaming_stops_after_command_completion_without_a_signal",
                    "--nocapture",
                ])
                .env("GIT_ON_TRANSFER_STOP_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        struct StopOnRead {
            remaining: usize,
            reads: usize,
        }
        impl Read for StopOnRead {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                let size = self.remaining.min(buffer.len());
                buffer[..size].fill(0);
                self.remaining -= size;
                crate::git::stop_operations();
                Ok(size)
            }
        }
        let size = 256 * 1024;
        let id = Oid::hash_object(ObjectType::Blob, &vec![0; size]).unwrap();
        let mut input = StopOnRead {
            remaining: size,
            reads: 0,
        };
        let error =
            copy_verified(&mut input, std::io::sink(), ObjectType::Blob, size, id).unwrap_err();
        assert!(error.to_string().contains("stopped"), "{error:#}");
        assert_eq!(crate::git::cancelled_signal(), None);
        assert_eq!(
            input.reads, 1,
            "shutdown consumed more than the active chunk"
        );
    }
    #[test]
    fn streaming_checks_exact_length_type_hash_and_io_errors() {
        let bytes = vec![0xff; 200_000];
        let id = Oid::hash_object(ObjectType::Blob, &bytes).unwrap();
        let mut output = Vec::new();
        copy_verified(&bytes[..], &mut output, ObjectType::Blob, bytes.len(), id).unwrap();
        assert_eq!(output, bytes);
        for (kind, size) in [
            (ObjectType::Tree, bytes.len()),
            (ObjectType::Blob, bytes.len() + 1),
            (ObjectType::Blob, bytes.len() - 1),
        ] {
            assert!(copy_verified(&bytes[..], std::io::sink(), kind, size, id)
                .unwrap_err()
                .is::<InvalidObject>());
        }
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("interrupted fixture transfer"))
            }
        }
        let error =
            copy_verified(Broken, std::io::sink(), ObjectType::Blob, bytes.len(), id).unwrap_err();
        assert!(!error.is::<InvalidObject>());
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }
}
