//! Invocation-owned native transports. Protocol v2 permits branch selection
//! and successive exact-object requests on the same channel.
use crate::spool::Spool;
use anyhow::{bail, ensure, Context, Result};
use git2::Oid;
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

fn invalid(message: &str) -> anyhow::Error {
    crate::object::InvalidObject(message.into()).into()
}
type Stop = Option<Arc<AtomicBool>>;

fn check_transfer(stop: &Stop) -> io::Result<()> {
    if stop
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        return Err(io::Error::other(
            anyhow::Error::new(crate::object::OperationCancelled)
                .context("owned HTTPS transport stopped"),
        ));
    }
    crate::object::check_transfer().map_err(io::Error::other)
}

struct Input(UnixStream, Stop);
impl Read for Input {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        loop {
            check_transfer(&self.1)?;
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
struct Output(UnixStream, Stop);
impl Write for Output {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        loop {
            check_transfer(&self.1)?;
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

pub(crate) struct SshSocket {
    pub(crate) path: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl SshSocket {
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode),
            "owned SSH control connection is unavailable; reconnection is forbidden"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Connection {
    command: Arc<Command>,
    helper: bool,
    ssh: Option<Arc<SshSocket>>,
}

impl Connection {
    pub(crate) fn new(command: Command, helper: bool, ssh: Option<SshSocket>) -> Self {
        Self {
            command: Arc::new(command),
            helper,
            ssh: ssh.map(Arc::new),
        }
    }

    pub(crate) fn is_ssh(&self) -> bool {
        self.ssh.is_some()
    }

    pub(crate) fn check(&self) -> Result<()> {
        if let Some(socket) = &self.ssh {
            socket.check()?;
        }
        Ok(())
    }

    pub(crate) fn connect(&self, stop: Stop) -> Result<Option<Session>> {
        self.open(stop, false)
    }

    pub(crate) fn connect_additional(&self, stop: Arc<AtomicBool>) -> Result<Option<Session>> {
        self.open(Some(stop), true)
    }

    pub(crate) fn start_additional(&self) -> Result<StartingSession> {
        StartingSession::spawn(self.command(true)?, self.helper, None)
    }

    fn open(&self, stop: Stop, additional: bool) -> Result<Option<Session>> {
        Session::connect_with_stop(self.command(additional)?, self.helper, stop)
    }

    fn command(&self, additional: bool) -> Result<Command> {
        self.check()?;
        let mut command = copy_command(&self.command);
        if additional && self.is_ssh() {
            command.env("GIT_ON_SSH_ADDITIONAL", "1");
        }
        Ok(command)
    }
}

pub(crate) fn copy_command(template: &Command) -> Command {
    let mut command = Command::new(template.get_program());
    command.args(template.get_args());
    if let Some(directory) = template.get_current_dir() {
        command.current_dir(directory);
    }
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    command
}

pub(crate) struct Session {
    owner: Owner,
    input: Output,
    output: BufReader<Input>,
    errors: File,
    stateless: bool,
}

pub(crate) struct StartingSession(Session);

impl StartingSession {
    pub(crate) fn spawn(mut command: Command, helper: bool, stop: Stop) -> Result<Self> {
        check_transfer(&stop)?;
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
                // Credential helpers must not be able to read the command's
                // terminal through /dev/tty. The session ID also owns a new
                // process group for transport cancellation and reaping.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("start invocation demand transport")?;
        // Command retains its Stdio descriptors after spawn. Close the
        // parent's copies so an exited helper produces EOF during handshake.
        drop(command);
        Ok(Self(Session {
            owner: Owner(Mutex::new(Some(child))),
            input: Output(input, stop.clone()),
            output: BufReader::with_capacity(64 * 1024, Input(output, stop)),
            errors,
            stateless: helper,
        }))
    }

    pub(crate) fn wait_for_advertisement(sessions: &[Self]) -> io::Result<Option<usize>> {
        check_transfer(&None)?;
        let mut sockets: Vec<_> = sessions
            .iter()
            .map(|session| libc::pollfd {
                fd: session.0.output.get_ref().0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if unsafe { libc::poll(sockets.as_mut_ptr(), sockets.len() as libc::nfds_t, 10) } == -1 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            return Ok(None);
        }
        Ok(sockets.iter().position(|socket| socket.revents != 0))
    }

    pub(crate) fn advertised_branch(mut self, query: &BranchQuery) -> Result<Option<Oid>> {
        let selected = read_selected_branch(&mut self.0.output, Some(query), true)?;
        let _ = self.0.input.write_all(b"0000");
        Ok(selected)
    }

    /// An optional spare must have a complete handshake before it can take
    /// over from an in-flight ref query on another channel.
    pub(crate) fn filtered_v2_ready(&self) -> bool {
        let mut preview = [0; 4096];
        let read = unsafe {
            libc::recv(
                self.0.output.get_ref().0.as_raw_fd(),
                preview.as_mut_ptr().cast(),
                preview.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        read > 0 && read_v2_capabilities(&mut &preview[..read as usize]).unwrap_or(false)
    }

    pub(crate) fn negotiate(mut self) -> Result<Option<Session>> {
        let session = &mut self.0;
        match session.handshake(session.stateless) {
            Ok(true) => Ok(Some(self.0)),
            Ok(false) => Ok(None),
            Err(error) => {
                let diagnostic = session.error_text();
                Err(error)
                    .with_context(|| format!("open native demand channel: {}", diagnostic.trim()))
            }
        }
    }

    pub(crate) fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.0.set_stop(stop);
    }
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
pub(crate) fn packet_text(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}
impl Session {
    /// A missing protocol/helper capability is an optional optimization miss,
    /// before any fetch. Errors after a fetch request must never retry it.
    #[cfg(test)]
    pub(crate) fn connect(command: Command, helper: bool) -> Result<Option<Self>> {
        Self::connect_with_stop(command, helper, None)
    }
    pub(crate) fn connect_with_stop(
        command: Command,
        helper: bool,
        stop: Stop,
    ) -> Result<Option<Self>> {
        StartingSession::spawn(command, helper, stop)?.negotiate()
    }
    pub(crate) fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.input.1 = Some(stop.clone());
        self.output.get_mut().1 = Some(stop);
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
        read_v2_capabilities(&mut self.output)
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
    pub(crate) fn select_branch(&mut self, query: &BranchQuery) -> Result<Option<Oid>> {
        self.request_branch(query)?;
        self.read_branch(query)
    }

    pub(crate) fn request_branch(&mut self, query: &BranchQuery) -> Result<()> {
        self.input.write_all(&reference_request(Some(query))?)?;
        Ok(())
    }

    pub(crate) fn branch_ready(&self) -> io::Result<bool> {
        check_transfer(&self.output.get_ref().1)?;
        if !self.output.buffer().is_empty() {
            return Ok(true);
        }
        let mut socket = libc::pollfd {
            fd: self.output.get_ref().0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe { libc::poll(&mut socket, 1, 5) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => Ok(false),
            -1 => Err(io::Error::last_os_error()),
            ready => Ok(ready != 0),
        }
    }

    pub(crate) fn read_branch(&mut self, query: &BranchQuery) -> Result<Option<Oid>> {
        read_selected_branch(&mut self.output, Some(query), false)
    }
    pub(crate) fn fetch_stream(
        &mut self,
        oids: &[Oid],
        mut response: crate::pack_stream::Response,
    ) -> Result<crate::pack_stream::Received> {
        response.set_stop(self.input.1.clone());
        let result = (|| {
            self.errors.set_len(0)?;
            self.errors.rewind()?;
            self.input.write_all(&exact_request(oids)?)?;
            let pack = response.read(&mut self.output)?;
            self.check_helper_end()?;
            Ok(pack)
        })();
        self.with_fetch_context(oids, result)
    }

    pub(crate) fn fetch_exact(&mut self, oids: &[Oid], pack: &Spool) -> Result<()> {
        let result = self.fetch(oids, pack);
        self.with_fetch_context(oids, result)
    }

    fn with_fetch_context<T>(&mut self, oids: &[Oid], result: Result<T>) -> Result<T> {
        result.with_context(|| {
            let diagnostic = self.error_text();
            let wanted = if let [oid] = oids {
                format!("object {oid}")
            } else {
                format!("objects ({} requests, first {})", oids.len(), oids[0])
            };
            format!(
                "remote fetch of exact {wanted} failed: {}",
                diagnostic.trim()
            )
        })
    }
    pub(crate) fn fetch(&mut self, oids: &[Oid], pack: &Spool) -> Result<()> {
        // The completed previous response has no diagnostics to retain. A
        // shared regular-file offset is reset only between transactions.
        self.errors.set_len(0)?;
        self.errors.rewind()?;
        self.input.write_all(&exact_request(oids)?)?;
        read_pack(&mut self.output, pack)?;
        self.check_helper_end()
    }

    fn check_helper_end(&mut self) -> Result<()> {
        if self.stateless {
            ensure!(
                read_packet(&mut self.output)? == Packet::End,
                "missing helper response end"
            );
        }
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
pub(crate) fn packet_length(header: &[u8; 4]) -> Result<usize> {
    ensure!(
        header.iter().all(u8::is_ascii_hexdigit),
        "invalid demand packet length"
    );
    Ok(usize::from_str_radix(std::str::from_utf8(header)?, 16)?)
}

fn read_packet(input: &mut impl Read) -> Result<Packet> {
    let mut header = [0; 4];
    input
        .read_exact(&mut header)
        .context("read demand packet header")?;
    Ok(match packet_length(&header)? {
        0 => Packet::Flush,
        1 => Packet::Delimiter,
        2 => Packet::End,
        length @ 4..=65520 => {
            let mut bytes = vec![0; length - 4];
            input.read_exact(&mut bytes)?;
            check_packet_error(&bytes)?;
            Packet::Data(bytes)
        }
        _ => return Err(invalid("invalid demand packet length")),
    })
}
pub(crate) fn check_packet_error(bytes: &[u8]) -> Result<()> {
    if let Some(error) = bytes.strip_prefix(b"ERR ") {
        bail!(
            "remote exact-object request rejected: {}",
            String::from_utf8_lossy(error)
        );
    }
    Ok(())
}

fn request_packet(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    write!(output, "{:04x}", bytes.len() + 4)?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn read_v2_capabilities(input: &mut impl Read) -> Result<bool> {
    if !matches!(read_packet(input)?, Packet::Data(version) if packet_text(&version) == b"version 2")
    {
        return Ok(false);
    }
    let mut fetch = false;
    let mut filter = false;
    loop {
        match read_packet(input)? {
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

#[derive(Clone)]
pub(crate) struct BranchQuery {
    wanted: String,
    alternatives: Vec<String>,
}

impl BranchQuery {
    pub(crate) fn new(revision: &std::ffi::OsStr) -> Option<Self> {
        let revision = revision.to_str()?;
        if revision.is_empty()
            || revision.len() > 1024
            || revision.starts_with('-')
            || revision.starts_with('+')
            || revision == "HEAD"
        {
            return None;
        }
        let wanted = if revision.starts_with("refs/heads/") {
            revision.to_owned()
        } else {
            if revision.starts_with("refs/") {
                return None;
            }
            format!("refs/heads/{revision}")
        };
        git2::Reference::is_valid_name(&wanted).then(|| {
            // Match Git's DWIM rules conservatively. Any competing ref leaves
            // the selection to native Git.
            let mut alternatives = vec![
                revision.to_owned(),
                format!("refs/{revision}"),
                format!("refs/heads/{revision}"),
                format!("refs/tags/{revision}"),
                format!("refs/remotes/{revision}"),
                format!("refs/remotes/{revision}/HEAD"),
            ];
            alternatives.retain(|other| *other != wanted);
            Self {
                wanted,
                alternatives,
            }
        })
    }

    fn prefixes(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.wanted.as_str()).chain(self.alternatives.iter().map(String::as_str))
    }
}

pub(crate) fn reference_request(query: Option<&BranchQuery>) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request_packet(&mut request, b"command=ls-refs\n")?;
    request.extend_from_slice(b"0001");
    if let Some(query) = query {
        for prefix in query.prefixes() {
            request_packet(&mut request, format!("ref-prefix {prefix}\n").as_bytes())?;
        }
    } else {
        request_packet(&mut request, b"ref-prefix refs/git-on/probe/\n")?;
    }
    request.extend_from_slice(b"0000");
    Ok(request)
}

pub(crate) fn read_http_branch(
    mut input: &[u8],
    query: Option<&BranchQuery>,
) -> Result<Option<Oid>> {
    let selected = read_selected_branch(&mut input, query, false)?;
    check_http_end(input)?;
    Ok(selected)
}

fn read_selected_branch(
    input: &mut impl Read,
    query: Option<&BranchQuery>,
    full_advertisement: bool,
) -> Result<Option<Oid>> {
    let mut selected = None;
    let mut ambiguous = false;
    let mut advertised_size = 0;
    loop {
        match read_packet(input)? {
            Packet::Data(line) => {
                if let Some(query) = query {
                    let mut reference = line.as_slice();
                    if full_advertisement {
                        let first = advertised_size == 0;
                        advertised_size += 4 + line.len();
                        if advertised_size > 128 * 1024 {
                            return Ok(None);
                        }
                        if first {
                            reference = reference.split(|&byte| byte == 0).next().unwrap();
                        }
                    }
                    let mut fields = packet_text(reference).split(|&byte| byte == b' ');
                    let id = fields.next().unwrap_or_default();
                    let name = fields.next().unwrap_or_default();
                    if name == query.wanted.as_bytes() {
                        ambiguous |= selected.is_some();
                        ensure!(
                            id.len() == 40 && id.iter().all(u8::is_ascii_hexdigit),
                            "invalid Git reference object ID"
                        );
                        selected = Some(Oid::from_str(std::str::from_utf8(id)?)?);
                    } else if query
                        .alternatives
                        .iter()
                        .any(|other| other.as_bytes() == name)
                    {
                        ambiguous = true;
                    }
                }
            }
            Packet::Flush => return Ok(selected.filter(|_| !ambiguous)),
            _ => return Err(invalid("invalid Git reference response")),
        }
    }
}

pub(crate) fn read_http_capabilities(mut input: &[u8]) -> Result<bool> {
    let mut body = input;
    if matches!(
        read_packet(&mut body)?,
        Packet::Data(service) if packet_text(&service) == b"# service=git-upload-pack"
    ) {
        ensure!(
            read_packet(&mut body)? == Packet::Flush,
            "invalid Git HTTP service announcement"
        );
        input = body;
    }
    read_v2_capabilities(&mut input)
}

pub(crate) fn exact_request(oids: &[Oid]) -> Result<Vec<u8>> {
    // One small exact-demand request, not one syscall/signal-status read
    // for every pkt-line header and body. Every wanted OID is explicit.
    let mut request = Vec::with_capacity(256);
    request_packet(&mut request, b"command=fetch\n")?;
    request.extend_from_slice(b"0001");
    for oid in oids {
        request_packet(&mut request, format!("want {oid}\n").as_bytes())?;
    }
    for value in [
        b"deepen 1\n".as_slice(),
        b"filter tree:0\n",
        b"no-progress\n",
        b"done\n",
    ] {
        request_packet(&mut request, value)?;
    }
    request.extend_from_slice(b"0000");
    Ok(request)
}

fn read_pack(input: &mut impl Read, pack: &Spool) -> Result<()> {
    loop {
        crate::object::check_transfer()?;
        match read_packet(input)? {
            Packet::Data(section) if packet_text(&section) == b"packfile" => break,
            Packet::Data(section) if packet_text(&section) == b"shallow-info" => loop {
                match read_packet(input)? {
                    Packet::Delimiter => break,
                    Packet::Data(value)
                        if value.starts_with(b"shallow ") || value.starts_with(b"unshallow ") => {}
                    _ => return Err(invalid("invalid demand shallow section")),
                }
            },
            _ => return Err(invalid("invalid demand response section")),
        }
    }
    let mut output = PackWriter::new(pack);
    loop {
        crate::object::check_transfer()?;
        match read_packet(input)? {
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
    output.finish()
}

fn check_http_end(input: impl Read) -> Result<()> {
    let mut trailer = Vec::new();
    input.take(5).read_to_end(&mut trailer)?;
    ensure!(
        trailer.is_empty() || trailer == b"0002",
        "unexpected data after the Git HTTP response"
    );
    Ok(())
}

// Keep the trailer out of the checksum without retaining the pack in RAM.
struct PackWriter<'a> {
    output: BufWriter<&'a Spool>,
    checksum: crate::pack_stream::Checksum,
}
impl<'a> PackWriter<'a> {
    fn new(file: &'a Spool) -> Self {
        Self {
            output: BufWriter::with_capacity(64 * 1024, file),
            checksum: crate::pack_stream::Checksum::default(),
        }
    }
    fn finish(mut self) -> Result<()> {
        self.output.flush()?;
        self.checksum.finish()
    }
}
impl Write for PackWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write_all(bytes)?;
        self.checksum.update(bytes);
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
            crate::object::check_transfer().map_err(std::io::Error::other)?;
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
    use sha1_checked::{Digest, Sha1};
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
            input: Output(input, None),
            output: BufReader::new(Input(output, None)),
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
    fn branch_selection_ignores_longer_prefixes_and_leaves_ambiguities_to_git() {
        let query = BranchQuery::new(std::ffi::OsStr::new("release/candidate")).unwrap();
        let mut response = Vec::new();
        let selected = Oid::from_bytes(&[3; 20]).unwrap();
        let other = Oid::from_bytes(&[4; 20]).unwrap();
        for (oid, name) in [
            (selected, "refs/heads/release/candidate"),
            (other, "refs/heads/release/candidate-next"),
        ] {
            text_packet(&mut response, format!("{oid} {name}").as_bytes(), true);
        }
        assert_eq!(
            read_http_branch(&[response.as_slice(), b"00000002"].concat(), Some(&query)).unwrap(),
            Some(selected)
        );
        text_packet(
            &mut response,
            format!("{other} refs/tags/release/candidate").as_bytes(),
            true,
        );
        response.extend_from_slice(b"0000");
        let (mut session, _peer) = peer_response(&response);
        assert_eq!(session.select_branch(&query).unwrap(), None);
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
                session.fetch(&[Oid::zero()], &file).unwrap();
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
        let blob = raw.insert_fetched(oid, size, reader).unwrap();
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
            assert!(raw.insert_fetched(wanted, size, reader).is_err());
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
                input: Output(input, None),
                output: BufReader::new(Input(output, None)),
                errors: tempfile::tempfile().unwrap(),
                stateless: false,
            };
            let file = spool(root.path(), &[]);
            assert!(session.fetch(&[Oid::zero()], &file).is_err());
            assert_eq!(file.len().unwrap(), 0);
        }
    }
    #[test]
    fn helper_exit_before_first_reply_reports_eof() {
        for helper in [false, true] {
            let (send, receive) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "echo early-helper-exit >&2; exit 7"]);
                let error = Session::connect(command, helper)
                    .err()
                    .map(|error| format!("{error:#}"));
                let _ = send.send(error);
            });
            let error = receive
                .recv_timeout(std::time::Duration::from_secs(3))
                .expect("exited helper did not close the handshake stream")
                .expect("early helper exit was accepted");
            assert!(error.contains("early-helper-exit"), "{error}");
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
            input: Output(input, None),
            output: BufReader::new(Input(output, None)),
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
#[test]
fn http_advertisement_accepts_the_version_two_service_preamble() {
    let mut capabilities = Vec::new();
    request_packet(&mut capabilities, b"version 2\n").unwrap();
    request_packet(&mut capabilities, b"fetch=shallow filter\n").unwrap();
    capabilities.extend_from_slice(b"0000");
    assert!(read_http_capabilities(&capabilities).unwrap());

    let mut announced = Vec::new();
    request_packet(&mut announced, b"# service=git-upload-pack\n").unwrap();
    announced.extend_from_slice(b"0000");
    announced.extend_from_slice(&capabilities);
    assert!(read_http_capabilities(&announced).unwrap());
}
