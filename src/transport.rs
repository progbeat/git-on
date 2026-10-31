//! Invocation-owned native transports. Git still resolves the source/refspec;
//! protocol v2 permits successive exact-object requests on an existing channel.
use crate::spool::Spool;
use anyhow::{bail, ensure, Context, Result};
use git2::Oid;
use sha1_checked::{Digest, Sha1};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::Mutex,
};

fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
struct Input(UnixStream);
impl Read for Input {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        loop {
            crate::object::check_transfer().map_err(crate::git::transfer_io_error)?;
            let mut poll = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut poll, 1, 10) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => continue,
                _ => return self.0.read(data),
            }
        }
    }
}
struct Output(UnixStream);
impl Write for Output {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        loop {
            crate::object::check_transfer().map_err(crate::git::transfer_io_error)?;
            let mut poll = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            match unsafe { libc::poll(&mut poll, 1, 10) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => continue,
                _ => match self.0.write(data) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => return result,
                },
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
// A child is never reaped before its final group stop, so its PID cannot be
// reused as an unrelated process-group ID between checking and killing it.
struct Owner(Mutex<Option<Child>>);
impl Owner {
    fn stop(&self) -> Result<()> {
        let mut child = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        if let Some(mut child) = child.take() {
            let killed = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let error = (killed != 0).then(io::Error::last_os_error);
            child.wait().context("wait for owned demand transport")?;
            if let Some(error) = error.filter(|error| error.raw_os_error() != Some(libc::ESRCH)) {
                return Err(error).context("stop owned demand transport group");
            }
        }
        Ok(())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(crate) struct Session {
    owner: Owner,
    input: Output,
    output: BufReader<Input>,
    errors: File,
    stateless: bool,
}
#[derive(Debug, PartialEq)]
enum Packet {
    Flush,
    Delimiter,
    End,
    Data(Vec<u8>),
}
// Git text pkt-lines accept an optional terminal LF. Never apply this to
// sideband pack payloads, which remain byte-exact binary data.
fn packet_text(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}
impl Session {
    /// A missing protocol/helper capability is an optional optimization miss,
    /// before any fetch. Errors after a fetch request must never retry it.
    pub(crate) fn connect(mut command: Command, helper: bool) -> Result<Option<Self>> {
        crate::object::check_transfer()?;
        let (input, to_child) = UnixStream::pair()?;
        input.set_nonblocking(true)?;
        let (output, from_child) = UnixStream::pair()?;
        let errors = tempfile::tempfile()?;
        command
            .stdin(Stdio::from(OwnedFd::from(to_child)))
            .stdout(Stdio::from(OwnedFd::from(from_child)))
            .stderr(errors.try_clone()?);
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("start invocation demand transport")?;
        let mut session = Self {
            owner: Owner(Mutex::new(Some(child))),
            input: Output(input),
            output: BufReader::with_capacity(64 * 1024, Input(output)),
            errors,
            stateless: helper,
        };
        match session.handshake(helper) {
            Ok(true) => Ok(Some(session)),
            Ok(false) => Ok(None),
            Err(error) => {
                let diagnostic = session.error_text();
                Err(error)
                    .with_context(|| format!("open native demand channel: {}", diagnostic.trim()))
            }
        }
    }
    fn handshake(&mut self, helper: bool) -> Result<bool> {
        if helper {
            self.input.write_all(b"capabilities\n")?;
            let mut available = false;
            loop {
                let line = self.line()?;
                if line.is_empty() {
                    break;
                }
                available |= line == b"stateless-connect";
            }
            if !available {
                return Ok(false);
            }
            self.input
                .write_all(b"stateless-connect git-upload-pack\n")?;
            let response = self.line()?;
            if response == b"fallback" {
                return Ok(false);
            }
            ensure!(
                response.is_empty(),
                "native helper rejected protocol connection"
            );
        }
        if !matches!(self.packet()?, Packet::Data(version) if packet_text(&version) == b"version 2")
        {
            return Ok(false);
        }
        let mut fetch = false;
        let mut filter = false;
        loop {
            match self.packet()? {
                Packet::Flush => break,
                Packet::Data(capability) => {
                    if let Some(values) = capability.strip_prefix(b"fetch=") {
                        fetch = true;
                        filter |= values
                            .split(u8::is_ascii_whitespace)
                            .any(|value| value == b"filter");
                    }
                    if let Some(format) = capability.strip_prefix(b"object-format=") {
                        ensure!(
                            packet_text(format) == b"sha1",
                            "SHA-256 repositories unsupported"
                        );
                    }
                }
                _ => bail!("invalid demand capability advertisement"),
            }
        }
        ensure!(fetch && filter, "demand transport lacks object filtering");
        Ok(true)
    }
    fn line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        // Native helper replies are small fixed protocol lines; packet replies
        // have the Git protocol's own 65520-byte framing bound below.
        Read::by_ref(&mut self.output)
            .take(65521)
            .read_until(b'\n', &mut line)?;
        ensure!(
            line.last() == Some(&b'\n'),
            "demand helper closed its reply stream"
        );
        line.pop();
        Ok(line)
    }
    fn packet(&mut self) -> Result<Packet> {
        let mut header = [0; 4];
        self.output
            .read_exact(&mut header)
            .context("read demand packet header")?;
        ensure!(
            header.iter().all(u8::is_ascii_hexdigit),
            "invalid demand packet length"
        );
        let length = u16::from_str_radix(std::str::from_utf8(&header)?, 16)?;
        Ok(match length {
            0 => Packet::Flush,
            1 => Packet::Delimiter,
            2 => Packet::End,
            4..=65520 => {
                let mut bytes = vec![0; usize::from(length) - 4];
                self.output.read_exact(&mut bytes)?;
                if let Some(error) = bytes.strip_prefix(b"ERR ") {
                    bail!(
                        "remote exact-object request rejected: {}",
                        String::from_utf8_lossy(error)
                    );
                }
                Packet::Data(bytes)
            }
            _ => return Err(invalid("invalid demand packet length")),
        })
    }
    fn request_packet(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
        write!(output, "{:04x}", bytes.len() + 4)?;
        output.extend_from_slice(bytes);
        Ok(())
    }
    pub(crate) fn fetch(&mut self, oid: Oid, pack: &Spool) -> Result<()> {
        #[cfg(feature = "test-metrics")]
        let _profile = crate::profile::span("session_fetch", 0);
        // The completed previous response has no diagnostics to retain. A
        // shared regular-file offset is reset only between transactions.
        self.errors.set_len(0)?;
        self.errors.rewind()?;
        // One small exact-demand request, not one syscall/signal-status read
        // for every pkt-line header and body. No future objects are prefetched.
        let mut request = Vec::with_capacity(256);
        Self::request_packet(&mut request, b"command=fetch\n")?;
        request.extend_from_slice(b"0001");
        Self::request_packet(&mut request, format!("want {oid}\n").as_bytes())?;
        for value in [
            b"deepen 1\n".as_slice(),
            b"filter tree:0\n",
            b"no-progress\n",
            b"done\n",
        ] {
            Self::request_packet(&mut request, value)?;
        }
        request.extend_from_slice(b"0000");
        self.input.write_all(&request)?;
        loop {
            match self.packet()? {
                Packet::Data(section) if packet_text(&section) == b"packfile" => break,
                Packet::Data(section) if packet_text(&section) == b"shallow-info" => loop {
                    match self.packet()? {
                        Packet::Delimiter => break,
                        Packet::Data(value)
                            if value.starts_with(b"shallow ")
                                || value.starts_with(b"unshallow ") => {}
                        _ => return Err(invalid("invalid demand shallow section")),
                    }
                },
                _ => return Err(invalid("invalid demand response section")),
            }
        }
        let mut output = PackWriter::new(pack);
        loop {
            match self.packet()? {
                Packet::Flush => break,
                Packet::Data(data) => match data.split_first() {
                    Some((1, payload)) => output.write_all(payload)?,
                    Some((2, _)) => {} // optional progress is not application data
                    Some((3, error)) => {
                        bail!("remote pack failed: {}", String::from_utf8_lossy(error))
                    }
                    _ => return Err(invalid("invalid demand pack sideband")),
                },
                _ => return Err(invalid("invalid demand pack termination")),
            }
        }
        if self.stateless {
            ensure!(self.packet()? == Packet::End, "missing helper response end");
        }
        output.finish()?;
        Ok(())
    }
    pub(crate) fn stop(&self) -> Result<()> {
        self.owner.stop()
    }
    pub(crate) fn error_text(&mut self) -> String {
        let _ = self.owner.stop();
        let mut bytes = Vec::new();
        let length = self.errors.metadata().map_or(0, |metadata| metadata.len());
        let _ = self
            .errors
            .seek(io::SeekFrom::Start(length.saturating_sub(65536)));
        let _ = Read::by_ref(&mut self.errors)
            .take(65536)
            .read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}
// Keep the trailer out of the checksum without retaining the pack in RAM.
struct PackWriter<'a> {
    output: BufWriter<&'a Spool>,
    hash: Sha1,
    tail: Vec<u8>,
}
impl<'a> PackWriter<'a> {
    fn new(file: &'a Spool) -> Self {
        Self {
            output: BufWriter::with_capacity(64 * 1024, file),
            hash: Sha1::new(),
            tail: Vec::with_capacity(20),
        }
    }
    fn finish(mut self) -> Result<()> {
        self.output.flush()?;
        ensure!(
            self.tail.len() == 20 && crate::object::finish_sha1(self.hash)?.as_slice() == self.tail,
            "received pack checksum mismatch"
        );
        Ok(())
    }
}
impl Write for PackWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write_all(bytes)?;
        if bytes.len() >= 20 {
            self.hash.update(&self.tail);
            self.hash.update(&bytes[..bytes.len() - 20]);
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - 20..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(20);
            self.hash.update(&self.tail[..excess]);
            self.tail.drain(..excess);
            self.tail.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

pub(crate) struct ObjectReader<'a> {
    file: &'a Spool,
    position: u64,
    end: u64,
    input: Box<[u8; 65536]>,
    from: usize,
    to: usize,
    decoder: flate2::Decompress,
    finished: bool,
}
impl<'a> ObjectReader<'a> {
    /// Complex/multi-object packs remain on the existing Git/libgit2 decoder.
    pub(crate) fn single(file: &'a Spool) -> Result<Option<(Self, usize, git2::ObjectType)>> {
        let length = file.len()?;
        ensure!(length >= 32, "truncated received pack");
        let mut header = [0; 12];
        file.read_exact_at(&mut header, 0)?;
        ensure!(
            &header[..4] == b"PACK"
                && matches!(u32::from_be_bytes(header[4..8].try_into()?), 2 | 3),
            "invalid pack header/version"
        );
        if u32::from_be_bytes(header[8..12].try_into()?) != 1 {
            return Ok(None);
        }
        let mut position = 12;
        let mut byte = [0];
        file.read_exact_at(&mut byte, position)?;
        position += 1;
        let kind = match (byte[0] >> 4) & 7 {
            1 => git2::ObjectType::Commit,
            2 => git2::ObjectType::Tree,
            3 => git2::ObjectType::Blob,
            4 => git2::ObjectType::Tag,
            6 | 7 => return Ok(None),
            _ => return Err(invalid("invalid pack object type")),
        };
        let mut size = usize::from(byte[0] & 15);
        let mut shift = 4;
        while byte[0] & 128 != 0 {
            ensure!(position < length - 20, "truncated pack object header");
            file.read_exact_at(&mut byte, position)?;
            position += 1;
            let value = usize::from(byte[0] & 127);
            ensure!(
                shift < usize::BITS && value <= usize::MAX >> shift,
                "pack object size overflow"
            );
            size |= value << shift;
            shift += 7;
        }
        ensure!(position < length - 20, "missing pack object data");
        Ok(Some((
            Self {
                file,
                position,
                end: length - 20,
                input: Box::new([0; 65536]),
                from: 0,
                to: 0,
                decoder: flate2::Decompress::new(true),
                finished: false,
            },
            size,
            kind,
        )))
    }
}
impl Read for ObjectReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.finished {
            return Ok(0);
        }
        loop {
            crate::object::check_transfer().map_err(crate::git::transfer_io_error)?;
            if self.from == self.to && self.position < self.end {
                let count = (self.end - self.position).min(self.input.len() as u64) as usize;
                self.file
                    .read_exact_at(&mut self.input[..count], self.position)?;
                self.position += count as u64;
                self.from = 0;
                self.to = count;
            }
            let before = (self.decoder.total_in(), self.decoder.total_out());
            let status = self.decoder.decompress(
                &self.input[self.from..self.to],
                output,
                if self.position == self.end {
                    flate2::FlushDecompress::Finish
                } else {
                    flate2::FlushDecompress::None
                },
            )?;
            let read = (self.decoder.total_in() - before.0) as usize;
            let wrote = (self.decoder.total_out() - before.1) as usize;
            self.from += read;
            if status == flate2::Status::StreamEnd {
                if self.from != self.to || self.position != self.end {
                    return Err(io::Error::other("trailing bytes after pack object"));
                }
                self.finished = true;
            }
            if wrote != 0 || self.finished {
                return Ok(wrote);
            }
            if read == 0 && self.position == self.end {
                return Err(io::Error::other("truncated pack zlib stream"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    fn pack(payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
        let mut size = payload.len();
        let mut first = (3 << 4) | (size & 15) as u8;
        size >>= 4;
        if size != 0 {
            first |= 128;
        }
        bytes.push(first);
        while size != 0 {
            let mut byte = (size & 127) as u8;
            size >>= 7;
            if size != 0 {
                byte |= 128;
            }
            bytes.push(byte);
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(payload).unwrap();
        bytes.extend(encoder.finish().unwrap());
        checksum(bytes)
    }
    fn checksum(mut bytes: Vec<u8>) -> Vec<u8> {
        let hash = Sha1::digest(&bytes);
        bytes.extend(hash);
        bytes
    }
    fn spool(root: &std::path::Path, bytes: &[u8]) -> Spool {
        let files = crate::spool::Files::with_limit(root, Some(97)).unwrap();
        let file = files.file(root).unwrap();
        (&file).write_all(bytes).unwrap();
        file
    }
    fn peer_response(bytes: &[u8]) -> (Session, UnixStream) {
        let (input, peer) = UnixStream::pair().unwrap();
        let (output, mut source) = UnixStream::pair().unwrap();
        source.write_all(bytes).unwrap();
        source.shutdown(std::net::Shutdown::Write).unwrap();
        let session = Session {
            owner: Owner(Mutex::new(None)),
            input: Output(input),
            output: BufReader::new(Input(output)),
            errors: tempfile::tempfile().unwrap(),
            stateless: false,
        };
        (session, peer)
    }
    fn text_packet(output: &mut Vec<u8>, text: &[u8], lf: bool) {
        write!(output, "{:04x}", text.len() + usize::from(lf) + 4).unwrap();
        output.extend_from_slice(text);
        if lf {
            output.push(b'\n');
        }
    }
    #[test]
    fn text_handshake_accepts_optional_lf() {
        for version_lf in [false, true] {
            for format_lf in [false, true] {
                let mut bytes = Vec::new();
                text_packet(&mut bytes, b"version 2", version_lf);
                text_packet(&mut bytes, b"fetch=shallow filter", format_lf);
                text_packet(&mut bytes, b"object-format=sha1", format_lf);
                bytes.extend_from_slice(b"0000");
                let (mut session, _request_peer) = peer_response(&bytes);
                assert!(session.handshake(false).unwrap());
            }
        }
        for lf in [false, true] {
            let mut bytes = Vec::new();
            text_packet(&mut bytes, b"version 2", true);
            text_packet(&mut bytes, b"fetch=shallow filter", true);
            text_packet(&mut bytes, b"object-format=sha256", lf);
            bytes.extend_from_slice(b"0000");
            let (mut session, _request_peer) = peer_response(&bytes);
            assert!(session.handshake(false).is_err());
        }
    }
    #[test]
    fn fetch_sections_accept_optional_lf_without_changing_binary() {
        let root = tempfile::tempdir().unwrap();
        // Include a binary trailing LF: only text labels may be normalized.
        let good = (0..65536)
            .map(|index| pack(format!("binary payload {index}").as_bytes()))
            .find(|bytes| bytes.last() == Some(&b'\n'))
            .unwrap();
        for shallow_lf in [false, true] {
            for pack_lf in [false, true] {
                let mut bytes = Vec::new();
                text_packet(&mut bytes, b"shallow-info", shallow_lf);
                text_packet(
                    &mut bytes,
                    format!("shallow {}", Oid::zero()).as_bytes(),
                    shallow_lf,
                );
                bytes.extend_from_slice(b"0001");
                text_packet(&mut bytes, b"packfile", pack_lf);
                write!(bytes, "{:04x}", good.len() + 5).unwrap();
                bytes.push(1);
                bytes.extend_from_slice(&good);
                bytes.extend_from_slice(b"0000");
                let file = spool(root.path(), &[]);
                let (mut session, _request_peer) = peer_response(&bytes);
                session.fetch(Oid::zero(), &file).unwrap();
                let mut actual = vec![0; good.len()];
                file.read_exact_at(&mut actual, 0).unwrap();
                assert_eq!(actual, good);
            }
        }
    }
    #[test]
    fn pack_trailer_is_checked_across_all_short_chunk_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let good = pack(b"payload");
        for step in 1..=good.len() {
            let file = spool(root.path(), &[]);
            let mut writer = PackWriter::new(&file);
            for bytes in good.chunks(step) {
                writer.write_all(bytes).unwrap();
            }
            writer.finish().unwrap();
            let mut actual = vec![0; good.len()];
            file.read_exact_at(&mut actual, 0).unwrap();
            assert_eq!(actual, good);
        }
        let file = spool(root.path(), &[]);
        let mut writer = PackWriter::new(&file);
        let mut bad = good.clone();
        bad[15] ^= 1;
        writer.write_all(&bad).unwrap();
        assert!(writer.finish().is_err());
        let file = spool(root.path(), &[]);
        let mut writer = PackWriter::new(&file);
        writer.write_all(&good[..19]).unwrap();
        assert!(writer.finish().is_err());
    }
    #[test]
    fn direct_pack_bytes_require_oid_zlib_end_and_no_hidden_tail() {
        let root = tempfile::tempdir().unwrap();
        let data = vec![b'x'; 1024 * 1024];
        let good = pack(&data);
        let oid = Oid::hash_object(git2::ObjectType::Blob, &data).unwrap();
        let file = spool(root.path(), &good);
        let (reader, size, kind) = ObjectReader::single(&file).unwrap().unwrap();
        assert_eq!(size, data.len());
        assert_eq!(kind, git2::ObjectType::Blob);
        let mut raw = crate::raw_blob::RawBlobs::new(root.path()).unwrap();
        let blob = raw.insert_fetched(oid, size, reader, root.path()).unwrap();
        assert_eq!(blob.read(999999, 17).unwrap(), vec![b'x'; 17]);
        assert!(raw.for_read(oid).unwrap().unwrap().1);
        assert!(!raw.for_read(oid).unwrap().unwrap().1);
        for mutation in [0, 1, 2] {
            let mut bytes = good[..good.len() - 20].to_vec();
            if mutation == 0 {
                bytes.truncate(bytes.len() - 2);
            }
            if mutation == 1 {
                bytes.extend(b"trailing");
            }
            let file = spool(root.path(), &checksum(bytes));
            let (reader, size, _) = ObjectReader::single(&file).unwrap().unwrap();
            let mut raw = crate::raw_blob::RawBlobs::new(root.path()).unwrap();
            let wanted = if mutation == 2 { Oid::zero() } else { oid };
            assert!(raw
                .insert_fetched(wanted, size, reader, root.path())
                .is_err());
            assert!(
                raw.get(wanted).unwrap().is_none(),
                "invalid object was published"
            );
        }
    }
    #[test]
    fn malformed_packet_and_pack_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        for response in [
            b"-001".to_vec(),
            b"ffff".to_vec(),
            b"0003".to_vec(),
            b"00".to_vec(),
        ] {
            let (input, _peer) = UnixStream::pair().unwrap();
            let (output, mut source) = UnixStream::pair().unwrap();
            source.write_all(&response).unwrap();
            source.shutdown(std::net::Shutdown::Write).unwrap();
            let mut session = Session {
                owner: Owner(Mutex::new(None)),
                input: Output(input),
                output: BufReader::new(Input(output)),
                errors: tempfile::tempfile().unwrap(),
                stateless: false,
            };
            let file = spool(root.path(), &[]);
            assert!(session.fetch(Oid::zero(), &file).is_err());
            assert_eq!(file.len().unwrap(), 0);
        }
    }
    #[test]
    fn dropping_idle_session_reaps_its_owned_native_process() {
        let root = tempfile::tempdir().unwrap();
        let capability = root.path().join("capabilities");
        let mut bytes = Vec::new();
        for value in [b"version 2\n".as_slice(), b"fetch=shallow filter\n"] {
            write!(bytes, "{:04x}", value.len() + 4).unwrap();
            bytes.extend(value);
        }
        bytes.extend(b"0000");
        std::fs::write(&capability, bytes).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "cat \"$1\"; exec sleep 30", "fixture"])
            .arg(&capability);
        let session = Session::connect(command, false).unwrap().unwrap();
        let pid = session.owner.0.lock().unwrap().as_ref().unwrap().id();
        drop(session);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[test]
    fn native_error_tail_has_bounded_memory() {
        let (input, _input_peer) = UnixStream::pair().unwrap();
        let (output, _output_peer) = UnixStream::pair().unwrap();
        let mut errors = tempfile::tempfile().unwrap();
        errors.write_all(&vec![b'x'; 256 * 1024]).unwrap();
        errors.write_all(b"actual failure").unwrap();
        let mut session = Session {
            owner: Owner(Mutex::new(None)),
            input: Output(input),
            output: BufReader::new(Input(output)),
            errors,
            stateless: false,
        };
        let message = session.error_text();
        assert!(
            message.len() <= 65536,
            "unbounded diagnostic retained {} bytes",
            message.len()
        );
        assert!(message.ends_with("actual failure"));
    }
    #[test]
    fn empty_blob_stream_and_complex_pack_classification() {
        let root = tempfile::tempdir().unwrap();
        let bytes = pack(b"");
        let file = spool(root.path(), &bytes);
        let (mut reader, size, _) = ObjectReader::single(&file).unwrap().unwrap();
        assert_eq!(size, 0);
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        for mutation in [0, 1] {
            let mut bytes = pack(b"payload");
            if mutation == 0 {
                bytes[11] = 2;
            } else {
                bytes[12] = 6 << 4;
            }
            let file = spool(root.path(), &bytes);
            assert!(ObjectReader::single(&file).unwrap().is_none());
        }
    }
}
