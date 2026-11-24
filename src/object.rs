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
    pub(crate) fn record(oid: Oid, bytes: usize) {
        HASH_PROBE.with(|probe| {
            if let Some((tracked, total)) = probe.get() {
                if tracked == oid {
                    probe.set(Some((tracked, total + bytes)));
                }
            }
        });
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

/// Read the next object link from verified commit or tag bytes. Only the
/// headers needed to validate and resolve that link are visited.
pub(crate) fn selected_child(kind: ObjectType, data: &[u8]) -> Result<Oid> {
    let child = match kind {
        ObjectType::Commit => commit_tree(data, |_| {}),
        ObjectType::Tag => tag_target(data),
        _ => None,
    };
    child.ok_or_else(|| InvalidObject(format!("invalid Git {} headers", kind.str())).into())
}

fn header<'a>(fields: &mut &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let end = fields.iter().position(|&byte| byte == b'\n')?;
    let value = fields[..end].strip_prefix(prefix)?;
    *fields = &fields[end + 1..];
    Some(value)
}

fn oid_header(fields: &mut &[u8], prefix: &[u8]) -> Option<Oid> {
    let value = header(fields, prefix)?;
    if value.len() != 40 {
        return None;
    }
    Oid::from_str(std::str::from_utf8(value).ok()?).ok()
}

fn identity_header(fields: &mut &[u8], prefix: &[u8]) -> Option<()> {
    let value = header(fields, prefix)?;
    let email_start = value.iter().rposition(|&byte| byte == b'<')?;
    let email_end = value.iter().rposition(|&byte| byte == b'>')?;
    if email_end <= email_start {
        return None;
    }
    // Match libgit2's acceptance of a missing timestamp and trailing
    // timezone text. The timestamp itself must fit in a signed 64-bit value.
    if let Some(timestamp) = value.get(email_end + 2..).filter(|value| !value.is_empty()) {
        let start = timestamp
            .iter()
            .position(|&byte| !matches!(byte, b' ' | b'\t'..=b'\r'))?;
        let timestamp = &timestamp[start..];
        let sign = usize::from(matches!(timestamp[0], b'+' | b'-'));
        let digits = timestamp[sign..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        std::str::from_utf8(&timestamp[..sign + digits])
            .ok()?
            .parse::<i64>()
            .ok()?;
    }
    Some(())
}

pub(crate) fn commit_links(data: &[u8], parent: impl FnMut(Oid)) -> Result<Oid> {
    commit_tree(data, parent)
        .ok_or_else(|| InvalidObject("invalid Git commit headers".into()).into())
}

fn commit_tree(mut fields: &[u8], mut parent: impl FnMut(Oid)) -> Option<Oid> {
    let tree = oid_header(&mut fields, b"tree ")?;
    while fields.starts_with(b"parent ") {
        parent(oid_header(&mut fields, b"parent ")?);
    }
    identity_header(&mut fields, b"author ")?;
    while fields.starts_with(b"author ") {
        identity_header(&mut fields, b"author ")?;
    }
    identity_header(&mut fields, b"committer ")?;
    Some(tree)
}

fn tag_target(mut fields: &[u8]) -> Option<Oid> {
    let target = oid_header(&mut fields, b"object ")?;
    match header(&mut fields, b"type ")? {
        b"commit" | b"tree" | b"blob" | b"tag" => {}
        _ => return None,
    }
    header(&mut fields, b"tag ")?;
    if !fields.is_empty() && !fields.starts_with(b"\n") {
        identity_header(&mut fields, b"tagger ")?;
    }
    if !fields.is_empty()
        && !fields.starts_with(b"\n")
        && !fields.windows(2).any(|bytes| bytes == b"\n\n")
    {
        return None;
    }
    Some(target)
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
        #[cfg(test)]
        HashProbe::record(self.oid, count);
        self.remaining -= count;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// The owned ODB buffer was verified before libgit2 returned it. Only the
/// audited startup policy establishes that fact; arbitrary streams never use
/// this route. Embedders without that policy verify this same borrowed buffer.
pub(crate) fn copy_odb_blob(
    object: &git2::OdbObject<'_>,
    output: impl Write,
    validate: impl FnOnce(&[u8]) -> Result<()>,
) -> Result<()> {
    verify_odb_object(object, ObjectType::Blob)?;
    validate(object.data())?;
    write_verified_buffer(object.data(), output)
}

pub(crate) fn verify_odb_object(object: &git2::OdbObject<'_>, kind: ObjectType) -> Result<()> {
    anyhow::ensure!(object.kind() == kind, "ODB object is not a {}", kind.str());
    if !NATIVE_OID_VERIFICATION.load(Ordering::Acquire) {
        let mut hash = VerifiedWriter::new(std::io::sink(), kind, object.len(), object.id());
        write_verified_buffer(object.data(), &mut hash)?;
        hash.finish()?;
    }
    check_transfer()
}

pub(crate) fn write_verified_buffer(bytes: &[u8], mut output: impl Write) -> Result<()> {
    for chunk in bytes.chunks(64 * 1024) {
        check_transfer()?;
        output
            .write_all(chunk)
            .context("write verified Git object")?;
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
    // Pack reuse must not make the descriptor cache grow with repository size.
    const PACK_DESCRIPTORS: usize = 32;
    unsafe {
        git2::opts::set_mwindow_file_limit(PACK_DESCRIPTORS)?;
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
    Ok(PACK_DESCRIPTORS)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn borrowed_commit_and_tag_headers_match_native_object_selection() {
        let odb = git2::Odb::new().unwrap();
        odb.add_new_mempack_backend(1).unwrap();
        let repo = git2::Repository::from_odb(odb).unwrap();
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let author = "author Someone <a@example.test> 123 +0000\n";
        let committer = "committer Someone <a@example.test> 123 +0000\n";
        let commit = format!("tree {oid}\nparent {oid}\n{author}");
        let tag = format!("object {oid}\ntype commit\ntag release\n");
        let cases = [
            (
                ObjectType::Commit,
                format!("{commit}{committer}\nmessage\n"),
            ),
            (ObjectType::Commit, format!("{commit}{author}{committer}")),
            (
                ObjectType::Commit,
                format!("{commit}committer Someone <a@example.test>\n"),
            ),
            (
                ObjectType::Commit,
                format!(
                    "{commit}committer Someone <a@example.test> -9223372036854775808 unknown\n"
                ),
            ),
            (
                ObjectType::Commit,
                format!("{commit}committer Someone <a@example.test> 9223372036854775808 +0000\n"),
            ),
            (
                ObjectType::Commit,
                format!("tree {oid}\nparent abbreviated\n{author}{committer}"),
            ),
            (
                ObjectType::Commit,
                format!("{commit}committer malformed> <\n"),
            ),
            (
                ObjectType::Tag,
                format!("{tag}tagger Someone <a@example.test> 123 +0000\n\nmessage\n"),
            ),
            (ObjectType::Tag, tag.clone()),
            (
                ObjectType::Tag,
                format!("{tag}tagger Someone <a@example.test>\nmissing separator"),
            ),
            (
                ObjectType::Tag,
                format!("{tag}tagger Someone <a@example.test>\nextra header\n\nmessage"),
            ),
            (
                ObjectType::Tag,
                format!("object {oid}\ntype invalid\ntag release\n"),
            ),
        ];
        for (kind, data) in cases {
            let stored = repo.odb().unwrap().write(kind, data.as_bytes()).unwrap();
            let expected = match kind {
                ObjectType::Commit => repo.find_commit(stored).map(|commit| commit.tree_id()),
                ObjectType::Tag => repo.find_tag(stored).map(|tag| tag.target_id()),
                _ => unreachable!(),
            };
            assert_eq!(
                selected_child(kind, data.as_bytes()).ok(),
                expected.ok(),
                "{kind:?} {data:?}"
            );
        }
    }

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
