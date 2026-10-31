//! Verified, cancellable transfers. libgit2 streams loose objects; its pack
//! backend currently requires decoded objects (including necessary delta bases).
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid};
use sha1_checked::{Digest, Sha1};
use std::{
    io::{Read, Write},
    sync::atomic::{AtomicBool, Ordering},
};

// Enabled only by the single-threaded CLI startup policy, and only for the
// audited vendored backend. An embedding that does not establish that policy
// keeps the original Rust verification path.
static NATIVE_OID_VERIFICATION: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static HASH_PROBE: std::cell::Cell<Option<(Oid, usize)>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
pub(crate) struct HashProbe;
#[cfg(test)]
impl HashProbe {
    pub(crate) fn start(oid: Oid) -> Self {
        HASH_PROBE.with(|probe| {
            assert!(probe.replace(Some((oid, 0))).is_none());
        });
        Self
    }
    pub(crate) fn bytes(&self) -> usize {
        HASH_PROBE.with(|probe| probe.get().expect("active hash probe").1)
    }
}
#[cfg(test)]
impl Drop for HashProbe {
    fn drop(&mut self) {
        HASH_PROBE.with(|probe| probe.set(None));
    }
}

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

pub(crate) fn finish_sha1(hash: Sha1) -> Result<[u8; 20]> {
    let result = hash.try_finalize();
    if result.has_collision() {
        return Err(InvalidObject("Git SHA-1 collision attack detected".into()).into());
    }
    Ok(result
        .hash()
        .as_slice()
        .try_into()
        .expect("SHA-1 output size"))
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
    #[cfg(feature = "test-metrics")]
    let _profile = crate::profile::span("object_verify_copy", size as u64);
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
        #[cfg(test)]
        HASH_PROBE.with(|probe| {
            if let Some((tracked, bytes)) = probe.get() {
                if tracked == oid {
                    probe.set(Some((tracked, bytes + count)));
                }
            }
        });
        output
            .write_all(&buffer[..count])
            .context("write Git object stream")?;
        remaining -= count;
    }
    check_transfer()?;
    if input.read(&mut buffer[..1])? != 0 {
        return Err(InvalidObject(format!("oversized Git object {oid}")).into());
    }
    if finish_sha1(hash)?.as_slice() != oid.as_bytes() {
        return Err(InvalidObject(format!("Git object hash mismatch for {oid}")).into());
    }
    Ok(())
}

/// The owned ODB buffer was verified before libgit2 returned it. Only the
/// audited startup policy establishes that fact; arbitrary streams never use
/// this route. Block digests are still formed from these same copied bytes.
pub(crate) fn copy_odb_blob(object: &git2::OdbObject<'_>, mut output: impl Write) -> Result<()> {
    anyhow::ensure!(
        object.kind() == ObjectType::Blob,
        "ODB object is not a blob"
    );
    if !NATIVE_OID_VERIFICATION.load(Ordering::Acquire) {
        return copy_verified(
            object.data(),
            output,
            ObjectType::Blob,
            object.len(),
            object.id(),
        );
    }
    #[cfg(feature = "test-metrics")]
    let _profile = crate::profile::span("object_preverified_copy", object.len() as u64);
    for bytes in object.data().chunks(64 * 1024) {
        check_transfer()?;
        output
            .write_all(bytes)
            .context("copy verified ODB buffer")?;
    }
    check_transfer()
}

/// # Safety
/// Call once at process startup, before spawning threads or using libgit2.
/// These options modify libgit2 C globals without synchronization.
/// Do not disable strict hash verification afterwards. Embeddings calling
/// this initializer must preserve its policy; without it the importer keeps
/// independent Rust verification.
pub unsafe fn configure_pack_descriptors() -> Result<usize> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read descriptor limit");
    }
    // Retained pack descriptors use at most a quarter of the soft limit, capped
    // at 32. The remaining descriptors serve Git startup, mounts, active files,
    // cache transfers and caller-inherited descriptors. No OS limit is changed.
    let budget = (unsafe { limit.assume_init() }.rlim_cur / 4).clamp(1, 32) as usize;
    unsafe {
        git2::opts::set_mwindow_file_limit(budget)?;
        // A long-lived source repository otherwise keeps 1 GiB windows under
        // libgit2's 8 GiB mapped-byte default (on 64-bit hosts), independently
        // of our decoded-data LRU. Size each window as well as the soft total:
        // one in-use window may exceed the latter, and active delta decoding
        // still has its own necessary working set. No object-size limit is set.
        git2::opts::set_mwindow_size(8 * 1024 * 1024)?;
        git2::opts::set_mwindow_mapped_limit(16 * 1024 * 1024)?;
    }
    // git_odb_read verifies the complete canonical object before publication
    // (libgit2 1.9.7 odb.c:1375). The pinned vendored build uses SHA1DC and
    // rejects detected collisions. Never infer the same guarantee from an
    // unknown system build or future dependency version.
    git2::opts::strict_hash_verification(true);
    let version = git2::Version::get();
    NATIVE_OID_VERIFICATION.store(
        version.vendored()
            && version.libgit2_version() == (1, 9, 7)
            && version.crate_version() == "0.20.4",
        Ordering::Release,
    );
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

#[cfg(test)]
mod collision_tests {
    use super::*;
    #[test]
    fn known_collision_vector_is_rejected_before_trusting_its_digest() {
        let mut hash = Sha1::new();
        hash.update(include_bytes!("../tests/fixtures/security/sha-mbles-1.bin"));
        assert!(finish_sha1(hash).is_err());
    }
}
