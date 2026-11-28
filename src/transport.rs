//! Invocation-owned native subprocesses and Git protocol v2 channels.
use crate::demand::ResponseIdle;
use anyhow::{bail, ensure, Context, Result};
use git2::Oid;
use std::{
    fs,
    io::{self, BufReader, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
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
                .context("owned transport stopped"),
        ));
    }
    crate::object::check_transfer().map_err(io::Error::other)
}

struct Input {
    socket: UnixStream,
    stop: Stop,
    errors: crate::git::Capture,
    idle: Option<ResponseIdle>,
}
impl Read for Input {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        if let Some(idle) = &mut self.idle {
            // Also check when the previous read contained only keepalives.
            idle.check()?;
        }
        loop {
            check_transfer(&self.stop)?;
            self.errors.pump()?;
            let mut poll = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut poll, 1, 10) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => {
                    if let Some(idle) = &mut self.idle {
                        idle.check()?;
                    }
                }
                _ => return self.socket.read(data),
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
    fn stop(&self) -> Result<Option<ExitStatus>> {
        let mut child = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        if let Some(mut child) = child.take() {
            let group = -(child.id() as i32);
            let killed = unsafe { libc::kill(group, libc::SIGKILL) };
            let error = (killed != 0).then(io::Error::last_os_error);
            let status =
                crate::terminal_ui::reap(&mut child).context("wait for owned demand transport")?;
            if let Some(error) = error.filter(|error| error.raw_os_error() != Some(libc::ESRCH)) {
                if error.raw_os_error() == Some(libc::EPERM)
                    && unsafe { libc::kill(group, 0) } != 0
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return Ok(Some(status));
                }
                return Err(error).context("stop owned demand transport group");
            }
            return Ok(Some(status));
        }
        Ok(None)
    }

    fn exited(&self) -> Result<bool> {
        let owned = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        let child = owned.as_ref().context("missing owned SSH process")?;
        Ok(crate::terminal_ui::has_exited(child)?)
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
    ssh: Arc<SshSocket>,
}

impl Connection {
    pub(crate) fn new(command: Command, ssh: SshSocket) -> Self {
        Self {
            command: Arc::new(command),
            ssh: Arc::new(ssh),
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        self.ssh.check()
    }

    pub(crate) fn connect_additional(&self, stop: Arc<AtomicBool>) -> Result<Option<Session>> {
        StartingSession::spawn(self.command()?, Some(stop), false)?.negotiate()
    }

    pub(crate) fn start_additional(&self) -> Result<StartingSession> {
        StartingSession::spawn(self.command()?, None, false)
    }

    fn command(&self) -> Result<Command> {
        self.check()?;
        Ok(copy_command(&self.command))
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

pub(crate) struct NativeProcess {
    owner: Owner,
    input: Output,
    output: BufReader<Input>,
    _native: Option<crate::terminal_ui::NativeOperation>,
}

impl NativeProcess {
    pub(crate) fn spawn(mut command: Command, stop: Stop, initial: bool) -> Result<Self> {
        check_transfer(&stop)?;
        if initial {
            crate::terminal_ui::configure(&mut command);
        }
        let (input, to_child) = UnixStream::pair()?;
        input.set_nonblocking(true)?;
        let (output, from_child) = UnixStream::pair()?;
        let (errors, stderr) = crate::git::Capture::new(Some(65536))?;
        command
            .stdin(Stdio::from(OwnedFd::from(to_child)))
            .stdout(Stdio::from(OwnedFd::from(from_child)))
            .stderr(stderr);
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
        let child = command.spawn().context("start invocation helper")?;
        let native = initial.then(|| crate::terminal_ui::NativeOperation::new(child.id()));
        // Command retains its Stdio descriptors after spawn. Close the
        // parent's copies so an exited helper produces EOF during handshake.
        drop(command);
        Ok(Self {
            owner: Owner(Mutex::new(Some(child))),
            input: Output(input, stop.clone()),
            output: BufReader::with_capacity(
                64 * 1024,
                Input {
                    socket: output,
                    stop,
                    errors,
                    idle: None,
                },
            ),
            _native: native,
        })
    }

    pub(crate) fn pipes(&mut self) -> (&mut impl Write, &mut impl io::BufRead) {
        (&mut self.input, &mut self.output)
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.owner.stop().map(|_| ())
    }

    fn finish_ssh(&mut self) -> Result<()> {
        ensure!(
            self.output.read(&mut [0])? == 0,
            "unexpected data after the Git SSH pack"
        );
        let Input { stop, errors, .. } = self.output.get_mut();
        while !self.owner.exited()? {
            check_transfer(stop)?;
            errors.pump()?;
            std::thread::sleep(Duration::from_millis(5));
        }
        // Keep the exited leader unreaped until its process group has been
        // stopped. The detached, recorded OpenSSH master is owned separately.
        let status = self.owner.stop()?.context("missing SSH exit status")?;
        ensure!(status.success(), "OpenSSH upload-pack failed with {status}");
        Ok(())
    }

    pub(crate) fn error_text(&mut self) -> String {
        let _ = self.stop();
        let errors = &mut self.output.get_mut().errors;
        let _ = errors.drain_available();
        String::from_utf8_lossy(&errors.bytes).into_owned()
    }
}

pub(crate) struct Session {
    process: NativeProcess,
    ref_requested: bool,
    early_fetch: Option<(Oid, bool)>,
}
pub(crate) struct StartingSession(Session);

impl StartingSession {
    pub(crate) fn spawn_foreground(command: Command) -> Result<Self> {
        Self::spawn(command, None, true)
    }

    fn spawn(command: Command, stop: Stop, foreground: bool) -> Result<Self> {
        Ok(Self(Session {
            process: NativeProcess::spawn(command, stop, foreground)?,
            ref_requested: false,
            early_fetch: None,
        }))
    }

    pub(crate) fn wait_for_advertisement(sessions: &mut [Self]) -> io::Result<Option<usize>> {
        check_transfer(&None)?;
        for session in sessions.iter_mut() {
            session.0.process.output.get_mut().errors.pump()?;
        }
        let mut sockets: Vec<_> = sessions
            .iter()
            .map(|session| libc::pollfd {
                fd: session.0.process.output.get_ref().socket.as_raw_fd(),
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

    pub(crate) fn queue_selection(
        &mut self,
        exact: Option<Oid>,
        branch: Option<&BranchQuery>,
        full: bool,
    ) -> Result<()> {
        (|| -> Result<()> {
            if let Some(oid) = exact {
                self.0
                    .process
                    .input
                    .write_all(&fetch_request(&[oid], full)?)?;
                self.0.early_fetch = Some((oid, full));
            } else if let Some(branch) = branch {
                self.0.request_branch(branch)?;
            }
            Ok(())
        })()
        .with_context(|| {
            let diagnostic = self.0.error_text();
            format!("open native demand channel: {}", diagnostic.trim())
        })
    }

    pub(crate) fn has_queued_selection(&self) -> bool {
        self.0.ref_requested || self.0.early_fetch.is_some()
    }

    pub(crate) fn negotiate(mut self) -> Result<Option<Session>> {
        let session = &mut self.0;
        match read_v2_capabilities(&mut session.process.output) {
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
    #[cfg(test)]
    fn connect(command: Command) -> Result<Option<Self>> {
        StartingSession::spawn(command, None, false)?.negotiate()
    }
    pub(crate) fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.process.input.1 = Some(stop.clone());
        self.process.output.get_mut().stop = Some(stop);
    }

    pub(crate) fn set_receive_idle(&mut self, idle: ResponseIdle) {
        self.process.output.get_mut().idle = Some(idle);
    }
    fn request_branch(&mut self, query: &BranchQuery) -> Result<()> {
        self.process.input.write_all(&reference_request(query)?)?;
        self.ref_requested = true;
        Ok(())
    }

    pub(crate) fn select_branch(&mut self, query: &BranchQuery) -> Result<Option<Oid>> {
        if !self.ref_requested {
            self.request_branch(query)?;
        }
        let chosen = read_selected_branch(&mut self.process.output, query)?;
        self.ref_requested = false;
        Ok(chosen)
    }

    pub(crate) fn fetch_stream(
        &mut self,
        oids: &[Oid],
        response: crate::pack_stream::Response,
    ) -> Result<crate::pack_stream::Received> {
        self.fetch_response(oids, response, false)
    }

    pub(crate) fn fetch_eager(
        &mut self,
        oid: Oid,
        response: crate::pack_stream::Response,
    ) -> Result<crate::pack_stream::Received> {
        self.fetch_response(&[oid], response, true)
    }

    fn fetch_response(
        &mut self,
        oids: &[Oid],
        response: crate::pack_stream::Response,
        full: bool,
    ) -> Result<crate::pack_stream::Received> {
        let result = (|| {
            if let Some((oid, queued_full)) = self.early_fetch.take() {
                ensure!(
                    oids == [oid] && full == queued_full,
                    "unexpected pipelined Git request"
                );
            } else {
                let errors = &mut self.process.output.get_mut().errors;
                errors.pump()?;
                errors.bytes.clear();
                self.process.input.write_all(&fetch_request(oids, full)?)?;
            }
            if full {
                match self.process.input.0.shutdown(Shutdown::Write) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotConnected => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let pack = response.read(&mut self.process.output)?;
            if full {
                self.process.finish_ssh()?;
            }
            Ok(pack)
        })();
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
    pub(crate) fn stop(&self) -> Result<()> {
        self.process.stop()
    }
    pub(crate) fn error_text(&mut self) -> String {
        self.process.error_text()
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
    names: Vec<String>,
}

impl BranchQuery {
    pub(crate) fn new(revision: &std::ffi::OsStr) -> Option<Self> {
        let revision = revision.to_str()?;
        if revision == "HEAD" || revision.starts_with("refs/") {
            return (revision == "HEAD" || git2::Reference::is_valid_name(revision)).then(|| {
                Self {
                    names: vec![revision.to_owned()],
                }
            });
        }
        if revision.is_empty() || revision.len() > 1024 || revision.starts_with('-') {
            return None;
        }
        let names: Vec<_> = [
            format!("refs/{revision}"),
            format!("refs/heads/{revision}"),
            format!("refs/tags/{revision}"),
            format!("refs/remotes/{revision}"),
            format!("refs/remotes/{revision}/HEAD"),
        ]
        .into_iter()
        .filter(|name| git2::Reference::is_valid_name(name))
        .collect();
        (!names.is_empty()).then_some(Self { names })
    }

    fn prefixes(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }
}

pub(crate) fn reference_request(query: &BranchQuery) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request_packet(&mut request, b"command=ls-refs\n")?;
    request.extend_from_slice(b"0001");
    for prefix in query.prefixes() {
        request_packet(&mut request, format!("ref-prefix {prefix}\n").as_bytes())?;
    }
    request.extend_from_slice(b"0000");
    Ok(request)
}

pub(crate) fn read_http_branch(mut input: &[u8], query: &BranchQuery) -> Result<Option<Oid>> {
    let selected = read_selected_branch(&mut input, query)?;
    ensure!(
        input.is_empty() || input == b"0002",
        "unexpected data after the Git HTTP response"
    );
    Ok(selected)
}

fn read_selected_branch(input: &mut impl Read, query: &BranchQuery) -> Result<Option<Oid>> {
    let mut selected = None;
    let mut ambiguous = false;
    loop {
        match read_packet(input)? {
            Packet::Data(line) => {
                let mut fields = packet_text(&line).split(|&byte| byte == b' ');
                let id = fields.next().unwrap_or_default();
                let name = fields.next().unwrap_or_default();
                if query
                    .names
                    .iter()
                    .any(|candidate| candidate.as_bytes() == name)
                {
                    ensure!(
                        id.len() == 40 && id.iter().all(u8::is_ascii_hexdigit),
                        "invalid Git reference object ID"
                    );
                    let oid = Oid::from_str(std::str::from_utf8(id)?)?;
                    ambiguous |= selected.is_some_and(|previous| previous != oid);
                    selected = Some(oid);
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
    fetch_request(oids, false)
}

pub(crate) fn fetch_request(oids: &[Oid], full: bool) -> Result<Vec<u8>> {
    // One small exact-demand request, not one syscall/signal-status read
    // for every pkt-line header and body. Every wanted OID is explicit.
    let mut request = Vec::with_capacity(256);
    request_packet(&mut request, b"command=fetch\n")?;
    request.extend_from_slice(b"0001");
    for oid in oids {
        request_packet(&mut request, format!("want {oid}\n").as_bytes())?;
    }
    request_packet(&mut request, b"deepen 1\n")?;
    let selection = if full {
        b"ofs-delta\n".as_slice()
    } else {
        b"filter tree:0\n"
    };
    request_packet(&mut request, selection)?;
    for value in [b"no-progress\n".as_slice(), b"done\n"] {
        request_packet(&mut request, value)?;
    }
    request.extend_from_slice(b"0000");
    Ok(request)
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
    fn peer_response(bytes: &[u8]) -> (Session, UnixStream) {
        let (input, peer) = UnixStream::pair().unwrap();
        let (output, mut source) = UnixStream::pair().unwrap();
        source.write_all(bytes).unwrap();
        source.shutdown(std::net::Shutdown::Write).unwrap();
        let session = Session {
            process: NativeProcess {
                owner: Owner(Mutex::new(None)),
                input: Output(input, None),
                output: BufReader::new(Input {
                    socket: output,
                    stop: None,
                    errors: crate::git::Capture::new(None).unwrap().0,
                    idle: None,
                }),
                _native: None,
            },
            ref_requested: false,
            early_fetch: None,
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
        assert!(BranchQuery::new(std::ffi::OsStr::new("+release"))
            .unwrap()
            .prefixes()
            .any(|name| name == "refs/heads/+release"));
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
            read_http_branch(&[response.as_slice(), b"00000002"].concat(), &query).unwrap(),
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
                assert!(read_v2_capabilities(&mut session.process.output).unwrap());
            }
        }
        for lf in [false, true] {
            let mut bytes = Vec::new();
            text_packet(&mut bytes, b"version 2", true);
            text_packet(&mut bytes, b"fetch=shallow filter", true);
            text_packet(&mut bytes, b"object-format=sha256", lf);
            bytes.extend_from_slice(b"0000");
            let (mut session, _request_peer) = peer_response(&bytes);
            assert!(read_v2_capabilities(&mut session.process.output).is_err());
        }
    }
    #[test]
    fn fetch_sections_accept_optional_lf_without_changing_binary() {
        // Include a binary trailing LF: only text labels may be normalized.
        let (payload, packed) = (0..65536)
            .map(|index| format!("binary payload {index}").into_bytes())
            .map(|data| {
                let bytes = pack(&data);
                (data, bytes)
            })
            .find(|(_, bytes)| bytes.last() == Some(&b'\n'))
            .unwrap();
        let oid = Oid::hash_object(git2::ObjectType::Blob, &payload).unwrap();
        for lf in [false, true] {
            let mut bytes = Vec::new();
            text_packet(&mut bytes, b"shallow-info", lf);
            text_packet(&mut bytes, format!("shallow {oid}").as_bytes(), lf);
            bytes.extend_from_slice(b"0001");
            text_packet(&mut bytes, b"packfile", lf);
            write!(bytes, "{:04x}", packed.len() + 5).unwrap();
            bytes.push(1);
            bytes.extend_from_slice(&packed);
            bytes.extend_from_slice(b"0000");
            let (mut session, _peer) = peer_response(&bytes);
            let expected = payload.clone();
            let response = crate::pack_stream::Response::new(
                vec![oid],
                Box::new(move |object| {
                    assert_eq!(object.data(), expected);
                    Ok(())
                }),
            );
            let actual = session.fetch_stream(&[oid], response).unwrap();
            assert_eq!(actual.bytes as usize, packed.len());
        }
    }
    #[test]
    fn malformed_packet_fails_closed() {
        for response in [b"-001".as_slice(), b"ffff", b"0003", b"00"] {
            let (mut session, _peer) = peer_response(response);
            let response = crate::pack_stream::Response::new(
                vec![Oid::zero()],
                Box::new(|_| panic!("invalid object was published")),
            );
            assert!(session.fetch_stream(&[Oid::zero()], response).is_err());
        }
    }

    #[test]
    fn filtered_multi_request_retains_present_object_and_identifies_omission() {
        let data = b"present";
        let packed = pack(data);
        let present = Oid::hash_object(git2::ObjectType::Blob, data).unwrap();
        let missing = Oid::hash_object(git2::ObjectType::Blob, b"missing").unwrap();
        let mut bytes = b"000dpackfile\n".to_vec();
        write!(bytes, "{:04x}", packed.len() + 5).unwrap();
        bytes.push(1);
        bytes.extend(packed);
        bytes.extend_from_slice(b"0000");
        let (mut session, _peer) = peer_response(&bytes);
        let oids = [present, missing];
        let collect = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ready = collect.clone();
        let response = crate::pack_stream::Response::new(
            oids.to_vec(),
            Box::new(move |object| {
                ready.lock().unwrap().push(object.oid());
                Ok(())
            }),
        );
        let result = session.fetch_stream(&oids, response).unwrap();
        assert_eq!(*collect.lock().unwrap(), [present]);
        assert!(result.incomplete);
        assert!(!result.objects.contains_key(&missing));
    }
    #[test]
    fn helper_exit_before_first_reply_reports_eof() {
        {
            let (send, receive) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "echo early-helper-exit >&2; exit 7"]);
                let error = Session::connect(command)
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
        let session = Session::connect(command).unwrap().unwrap();
        let pid = session
            .process
            .owner
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .id();
        drop(session);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[test]
    fn native_error_tail_has_bounded_memory() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 262144 /dev/zero >&2; printf 'actual failure' >&2",
        ]);
        let mut session = StartingSession::spawn(command, None, false).unwrap().0;
        assert_eq!(session.process.output.read(&mut [0]).unwrap(), 0);
        let message = session.error_text();
        assert!(
            message.len() <= 65536,
            "unbounded diagnostic retained {} bytes",
            message.len()
        );
        assert!(message.ends_with("actual failure"));
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
