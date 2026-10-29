//! Verified, cancellable transfers. libgit2 streams loose objects; its pack
//! backend currently requires decoded objects (including necessary delta bases).
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Odb, Oid};
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

/// Returns whether the transfer used a streaming backend. A failed streaming
/// open falls back to libgit2 decoding, never to native Git auto-fetch.
pub(crate) fn copy_from_odb(
    odb: &Odb<'_>,
    oid: Oid,
    mut output: impl Write,
) -> Result<(usize, ObjectType, bool)> {
    if let Ok((reader, size, kind)) = odb.reader(oid) {
        copy_verified(reader, &mut output, kind, size, oid)?;
        Ok((size, kind, true))
    } else {
        let object = odb.read(oid).context("decode Git object")?;
        copy_verified(object.data(), &mut output, object.kind(), object.len(), oid)?;
        Ok((object.len(), object.kind(), false))
    }
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
