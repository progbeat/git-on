//! Verified, cancellable transfers. libgit2 streams loose objects; its pack
//! backend currently requires decoded objects (including necessary delta bases).
use anyhow::{Context, Result};
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

pub(crate) fn revision_target(kind: ObjectType, bytes: &[u8]) -> Result<Oid> {
    let prefix = match kind {
        ObjectType::Commit => b"tree ".as_slice(),
        ObjectType::Tag => b"object ".as_slice(),
        _ => anyhow::bail!("Git revision metadata must be a commit or tag"),
    };
    let line = bytes
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    let target = line
        .strip_prefix(prefix)
        .filter(|value| value.len() == 40)
        .context("invalid Git revision target")?;
    Ok(Oid::from_str(std::str::from_utf8(target)?)?)
}

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

#[derive(Debug)]
pub(crate) struct OperationCancelled;
impl std::fmt::Display for OperationCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("owned operation cancelled")
    }
}
impl std::error::Error for OperationCancelled {}
pub(crate) fn is_cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<OperationCancelled>())
}
pub(crate) fn check_transfer() -> Result<()> {
    if let Some(signal) = crate::git::cancelled_signal() {
        return Err(OperationCancelled)
            .context(format!("object transfer cancelled by signal {signal}"));
    }
    if crate::git::operations_stopped() {
        return Err(OperationCancelled).context("object transfer stopped after command completion");
    }
    Ok(())
}

pub(crate) fn object_hash(kind: ObjectType, size: usize) -> Sha1 {
    let mut hash = Sha1::new();
    hash.update(format!("{} {size}\0", kind.str()).as_bytes());
    hash
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
    let mut output = VerifiedWriter::new(&mut output, kind, size, oid);
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
    output.finish()
}

/// Authenticate the same buffers written to an unpublished destination. Native
/// decoders can stream through this writer without a second decoded spool.
pub(crate) struct VerifiedWriter<W> {
    output: W,
    hash: Sha1,
    remaining: usize,
    oid: Oid,
}
impl<W: Write> VerifiedWriter<W> {
    pub(crate) fn new(output: W, kind: ObjectType, size: usize, oid: Oid) -> Self {
        Self {
            output,
            hash: object_hash(kind, size),
            remaining: size,
            oid,
        }
    }
    pub(crate) fn finish(self) -> Result<()> {
        anyhow::ensure!(
            self.remaining == 0,
            InvalidObject(format!("truncated Git object {}", self.oid))
        );
        if finish_sha1(self.hash)?.as_slice() != self.oid.as_bytes() {
            return Err(InvalidObject(format!("Git object hash mismatch for {}", self.oid)).into());
        }
        Ok(())
    }
}
impl<W: Write> Write for VerifiedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(InvalidObject(format!(
                "oversized Git object {}",
                self.oid
            ))));
        }
        let count = self.output.write(bytes)?;
        self.hash.update(&bytes[..count]);
        self.remaining -= count;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// The owned ODB buffer was verified before libgit2 returned it. Only the
/// audited startup policy establishes that fact; arbitrary streams never use
/// this route.
pub(crate) fn odb_is_verified() -> bool {
    NATIVE_OID_VERIFICATION.load(Ordering::Acquire)
}

pub(crate) fn copy_odb_blob(object: &git2::OdbObject<'_>, mut output: impl Write) -> Result<()> {
    anyhow::ensure!(
        object.kind() == ObjectType::Blob,
        "ODB object is not a blob"
    );
    if !odb_is_verified() {
        return copy_verified(
            object.data(),
            output,
            ObjectType::Blob,
            object.len(),
            object.id(),
        );
    }
    for bytes in object.data().chunks(64 * 1024) {
        check_transfer()?;
        output
            .write_all(bytes)
            .context("copy verified ODB buffer")?;
    }
    check_transfer()
}

pub(crate) struct OdbBuffer(git2::OdbObject<'static>);

// SAFETY: On the audited libgit2 version, the ODB object owns its immutable
// allocation and releases it through an atomic reference count.
unsafe impl Send for OdbBuffer {}
// SAFETY: The only shared access exposes the immutable allocation as a slice.
unsafe impl Sync for OdbBuffer {}

impl OdbBuffer {
    pub(crate) fn take<'a>(
        object: git2::OdbObject<'a>,
    ) -> std::result::Result<Self, git2::OdbObject<'a>> {
        if !odb_is_verified() {
            return Err(object);
        }
        // SAFETY: The version check in configure_pack_descriptors also pins
        // the audited ownership model. libgit2 1.9.7 odb_object__alloc puts
        // the decoded buffer and metadata in its own refcounted allocation,
        // without an ODB reference. git2 only ties the Rust handle to the ODB
        // through PhantomData. data() and Drop need only the owned allocation.
        Ok(Self(unsafe {
            std::mem::transmute::<git2::OdbObject<'a>, git2::OdbObject<'static>>(object)
        }))
    }

    pub(crate) fn data(&self) -> &[u8] {
        self.0.data()
    }
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
    fn cancellation_identity_survives_io_wrapping_without_hiding_real_faults() {
        let cancelled = anyhow::Error::new(std::io::Error::other(
            anyhow::Error::new(OperationCancelled).context("socket read"),
        ))
        .context("fetch remote object");
        assert!(is_cancelled(&cancelled));
        for error in [
            anyhow::Error::new(InvalidObject("hash mismatch during cancellation".into())),
            std::io::Error::from_raw_os_error(libc::EIO).into(),
            anyhow::anyhow!("remote peer cancelled its request"),
        ] {
            assert!(!is_cancelled(&error));
        }
    }
    #[test]
    fn verified_writer_authenticates_partial_writes_and_rejects_invalid_lengths() {
        struct Short(Vec<u8>);
        impl Write for Short {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let count = bytes.len().min(2);
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = b"verified";
        let oid = Oid::hash_object(ObjectType::Blob, bytes).unwrap();
        let mut destination = Short(Vec::new());
        let mut writer = VerifiedWriter::new(&mut destination, ObjectType::Blob, bytes.len(), oid);
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap();
        assert_eq!(destination.0, bytes);
        for invalid in [&b"short"[..], &b"modified"[..]] {
            let mut writer =
                VerifiedWriter::new(std::io::sink(), ObjectType::Blob, bytes.len(), oid);
            writer.write_all(invalid).unwrap();
            assert!(writer.finish().unwrap_err().is::<InvalidObject>());
        }
        let mut writer = VerifiedWriter::new(std::io::sink(), ObjectType::Blob, bytes.len(), oid);
        assert!(writer.write_all(b"oversized").is_err());
    }
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
