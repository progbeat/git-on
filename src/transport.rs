//! Invocation-owned native transports. Protocol v2 permits reference selection
//! and successive exact-object requests on the same channel.
use crate::{platform::child_exited, service_storage::CaptureFile};
use anyhow::{bail, ensure, Context, Result};
use git2::Oid;
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
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

#[cfg(target_os = "macos")]
fn observe_exit_after_darwin_group_denial(pid: u32) -> bool {
    use std::time::{Duration, Instant};

    // Darwin can return EPERM while the leader is exiting, just before
    // waitid can observe it. A genuine denial must not block shutdown.
    let wakeup = crate::git::ChildExitWakeup::new(pid);
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match child_exited(pid) {
            Ok(false) => (),
            Ok(true) => return true,
            Err(_) => return false,
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_millis(1) {
            return false;
        }
        wakeup.wait(remaining.min(Duration::from_millis(10)), None, [None; 2]);
    }
}

struct Input {
    socket: UnixStream,
    stop: Stop,
    leader: Option<u32>,
    close_after_command: Option<CloseAfterCommand>,
}

struct CloseAfterCommand {
    request: Arc<UnixStream>,
    finished: Arc<AtomicBool>,
}

impl Read for Input {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        loop {
            check_transfer(&self.stop)?;
            if self
                .close_after_command
                .as_ref()
                .is_some_and(|watch| watch.finished.load(Ordering::Acquire))
            {
                let watch = self
                    .close_after_command
                    .take()
                    .expect("command exit watcher");
                shutdown_request(&watch.request)?;
            }
            let mut poll = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut poll, 1, 10) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => {
                    if let Some(pid) = self.leader {
                        if child_exited(pid)? {
                            // Drain bytes already queued by SSH. A surviving
                            // writer must not delay EOF after the leader exits.
                            let count = unsafe {
                                libc::recv(
                                    self.socket.as_raw_fd(),
                                    data.as_mut_ptr().cast(),
                                    data.len(),
                                    libc::MSG_DONTWAIT,
                                )
                            };
                            if count >= 0 {
                                return Ok(count as usize);
                            }
                            let error = io::Error::last_os_error();
                            match error.kind() {
                                io::ErrorKind::WouldBlock => return Ok(0),
                                io::ErrorKind::Interrupted => continue,
                                _ => return Err(error),
                            }
                        }
                    }
                }
                _ => return self.socket.read(data),
            }
        }
    }
}
struct Output(Arc<UnixStream>, Stop);
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
                _ => match self.0.as_ref().write(data) {
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
struct Owner(Mutex<Option<OwnedChild>>);

struct OwnedChild {
    process: Child,
    terminal: Option<crate::terminal_ui::NativeOperation>,
}

impl OwnedChild {
    fn id(&self) -> u32 {
        self.process.id()
    }

    fn wait(mut self) -> io::Result<ExitStatus> {
        // Stop sending terminal signals before the PID can be reused.
        drop(self.terminal.take());
        self.process.wait()
    }
}

impl Owner {
    fn new(process: Child, terminal: bool) -> Self {
        let terminal = terminal.then(|| crate::terminal_ui::NativeOperation::new(process.id()));
        Self(Mutex::new(Some(OwnedChild { process, terminal })))
    }

    fn stop(&self) -> Result<()> {
        self.stop_with_status().map(|_| ())
    }

    fn stop_with_status(&self) -> Result<Option<ExitStatus>> {
        let mut owned = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        let Some(child) = owned.as_ref() else {
            return Ok(None);
        };
        let exited = child_exited(child.id()).unwrap_or(false);
        let (status, exited) = Self::kill_and_reap(&mut owned, exited)?;
        // SIGKILL can be our own cancellation. Any other terminal signal or
        // status belongs to the process, including an already observed SIGKILL.
        Ok((exited || status.signal() != Some(libc::SIGKILL)).then_some(status))
    }

    fn stop_group(pid: u32) -> Option<io::Error> {
        let killed = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        (killed != 0).then(io::Error::last_os_error)
    }

    fn kill_and_reap(owned: &mut Option<OwnedChild>, exited: bool) -> Result<(ExitStatus, bool)> {
        let error = Self::stop_group(owned.as_ref().expect("owned transport").id());
        Self::reap_after_group_signal(owned, exited, error)
    }

    fn reap_after_group_signal(
        owned: &mut Option<OwnedChild>,
        mut exited: bool,
        error: Option<io::Error>,
    ) -> Result<(ExitStatus, bool)> {
        let pid = owned.as_ref().expect("owned transport").id();
        let error =
            if !exited && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::EPERM) {
                #[cfg(target_os = "macos")]
                {
                    exited = observe_exit_after_darwin_group_denial(pid);
                    if exited {
                        // The unreaped leader still reserves the group ID.
                        Self::stop_group(pid)
                    } else {
                        error
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    exited = child_exited(pid).unwrap_or(false);
                    error
                }
            } else {
                error
            };
        let error = error.filter(|error| {
            let errno = error.raw_os_error();
            // A group containing only its unreaped leader can return EPERM.
            errno != Some(libc::ESRCH) && !(exited && errno == Some(libc::EPERM))
        });
        if !exited {
            if let Some(error) = error {
                // Leave the child owned so Drop can retry shutdown.
                return Err(error).context("stop owned demand transport group");
            }
        }
        let status = owned
            .take()
            .expect("owned transport")
            .wait()
            .context("wait for owned demand transport")?;
        if let Some(error) = error {
            return Err(error).context("stop owned demand transport group");
        }
        Ok((status, exited))
    }

    fn finish(
        &self,
        mut output: &UnixStream,
        check_end: bool,
        check: impl Fn() -> Result<()>,
    ) -> Result<std::process::ExitStatus> {
        let mut owned = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        let pid = owned.as_ref().context("transport has already closed")?.id();
        output.set_nonblocking(true)?;
        let mut fd = Some(output.as_raw_fd());
        let mut buffer = [0; 8192];
        let wakeup = crate::git::ChildExitWakeup::new(pid);
        loop {
            check()?;
            let exited = child_exited(pid)?;
            if fd.is_some() {
                match output.read(&mut buffer) {
                    Ok(0) => fd = None,
                    Ok(_) => {
                        ensure!(!check_end, "unexpected data after Git response");
                        if !exited {
                            continue;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                    Err(error) => return Err(error.into()),
                }
            }
            if exited {
                // Bytes written by the leader are now drained or rejected.
                // Its PID still reserves the group while we stop descendants.
                return Self::kill_and_reap(&mut owned, true).map(|(status, _)| status);
            }
            wakeup.wait(std::time::Duration::from_millis(10), None, [fd, None]);
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn spawn_piped(mut command: Command, stop: Stop) -> Result<(Child, Output, BufReader<Input>)> {
    check_transfer(&stop)?;
    let (input, to_child) = UnixStream::pair()?;
    input.set_nonblocking(true)?;
    let (output, from_child) = UnixStream::pair()?;
    command
        .stdin(Stdio::from(OwnedFd::from(to_child)))
        .stdout(Stdio::from(OwnedFd::from(from_child)));
    unsafe {
        command.pre_exec(|| {
            // Give the owned process a separate group and no controlling TTY.
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    Ok((
        child,
        Output(Arc::new(input), stop.clone()),
        BufReader::with_capacity(
            64 * 1024,
            Input {
                socket: output,
                stop,
                leader: None,
                close_after_command: None,
            },
        ),
    ))
}

pub(crate) struct BatchProcess {
    owner: Owner,
    input: Output,
    output: BufReader<Input>,
    errors: Diagnostics,
}

impl BatchProcess {
    pub(crate) fn spawn(mut command: Command) -> Result<Self> {
        let (errors, writer) = DiagnosticChannel::open()?;
        command.stderr(writer);
        let (child, input, mut output) =
            spawn_piped(command, None).context("start native Git object reader")?;
        output.get_mut().leader = Some(child.id());
        Ok(Self {
            owner: Owner::new(child, false),
            input,
            output,
            errors,
        })
    }

    pub(crate) fn exchange<T>(
        &mut self,
        request: &[u8],
        receive: impl FnOnce(&mut dyn BufRead) -> Result<T>,
    ) -> Result<T> {
        let result = (|| {
            self.input.write_all(request)?;
            receive(&mut self.output)
        })();
        result.map_err(|error| {
            let status = self.owner.stop_with_status().ok().flatten();
            let error = match status {
                Some(status) if !status.success() => {
                    error.context(format!("native Git object reader exited with {status}"))
                }
                _ => error,
            };
            self.with_diagnostics(error)
        })
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.owner.stop()
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        ensure!(
            self.output.buffer().is_empty(),
            "unexpected data after the last Git object"
        );
        shutdown_request(&self.input.0)?;
        let status = self.owner.finish(&self.output.get_ref().socket, true, || {
            Ok(check_transfer(&self.input.1)?)
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(self.with_diagnostics(anyhow::anyhow!(
                "native Git object reader exited with {status}"
            )))
        }
    }

    fn with_diagnostics(&mut self, error: anyhow::Error) -> anyhow::Error {
        let bytes = self.errors.tail();
        let diagnostic = String::from_utf8_lossy(&bytes);
        match diagnostic.trim() {
            "" => error,
            diagnostic => error.context(format!("native Git object reader: {diagnostic}")),
        }
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
    ssh: Option<Arc<SshSocket>>,
}

impl Connection {
    pub(crate) fn new(command: Command, ssh: Option<SshSocket>) -> Self {
        Self {
            command: Arc::new(command),
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

    pub(crate) fn connect_additional(&self, stop: Arc<AtomicBool>) -> Result<Option<Session>> {
        self.start_additional(Some(stop))?.negotiate()
    }

    pub(crate) fn start_additional(&self, stop: Stop) -> Result<StartingSession> {
        self.check()?;
        let mut command = copy_command(&self.command);
        if self.is_ssh() {
            command.env("GIT_ON_SSH_ADDITIONAL", "1");
            StartingSession::spawn(command, stop)
        } else {
            StartingSession::upload_pack(command, stop)
        }
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
    errors: Diagnostics,
    completed_fetch: AtomicBool,
}

const DIAGNOSTIC_LIMIT: usize = 64 * 1024;

enum Diagnostics {
    File(CaptureFile),
    Channel(DiagnosticChannel),
}

impl Diagnostics {
    fn file() -> Result<(Self, Stdio)> {
        Self::capture(CaptureFile::new()?)
    }

    fn capture(errors: CaptureFile) -> Result<(Self, Stdio)> {
        let writer = errors.file.try_clone()?.into();
        Ok((Self::File(errors), writer))
    }

    fn clear(&mut self) -> io::Result<()> {
        match self {
            Self::File(capture) => {
                capture.file.set_len(0)?;
                capture.file.rewind()
            }
            Self::Channel(channel) => {
                let mut buffer = channel.buffer.lock().unwrap();
                buffer.receive_pending()?;
                buffer.bytes.clear();
                Ok(())
            }
        }
    }

    fn tail(&mut self) -> Vec<u8> {
        match self {
            Self::File(capture) => {
                let mut file = &capture.file;
                let mut bytes = Vec::new();
                let length = file.metadata().map_or(0, |metadata| metadata.len());
                let _ = file.seek(io::SeekFrom::Start(
                    length.saturating_sub(DIAGNOSTIC_LIMIT as u64),
                ));
                let _ = file.take(DIAGNOSTIC_LIMIT as u64).read_to_end(&mut bytes);
                bytes
            }
            Self::Channel(channel) => {
                let mut buffer = channel.buffer.lock().unwrap();
                let _ = buffer.receive_pending();
                buffer.bytes.iter().copied().collect()
            }
        }
    }
}

struct DiagnosticChannel {
    buffer: Arc<Mutex<DiagnosticBuffer>>,
    worker: Option<JoinHandle<()>>,
}

struct DiagnosticBuffer {
    reader: UnixStream,
    bytes: VecDeque<u8>,
}

impl DiagnosticChannel {
    fn open() -> io::Result<(Diagnostics, Stdio)> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let fd = reader.as_raw_fd();
        let buffer = Arc::new(Mutex::new(DiagnosticBuffer {
            reader,
            bytes: VecDeque::new(),
        }));
        let shared = buffer.clone();
        let worker = std::thread::Builder::new()
            .name("upload-pack-stderr".into())
            .spawn(move || {
                let mut poll = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                loop {
                    if unsafe { libc::poll(&mut poll, 1, -1) } < 0
                        && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                    if !matches!(shared.lock().unwrap().receive(DIAGNOSTIC_LIMIT), Ok(true)) {
                        break;
                    }
                }
            })?;
        Ok((
            Diagnostics::Channel(Self {
                buffer,
                worker: Some(worker),
            }),
            Stdio::from(OwnedFd::from(writer)),
        ))
    }
}

impl Drop for DiagnosticChannel {
    fn drop(&mut self) {
        let _ = self
            .buffer
            .lock()
            .unwrap()
            .reader
            .shutdown(std::net::Shutdown::Read);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl DiagnosticBuffer {
    fn receive_pending(&mut self) -> io::Result<()> {
        let mut pending: libc::c_int = 0;
        if unsafe { libc::ioctl(self.reader.as_raw_fd(), libc::FIONREAD, &mut pending) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.receive(pending as usize)?;
        Ok(())
    }

    // Keep only the latest diagnostics, draining independently of the
    // protocol data so a verbose child cannot block the requested transfer.
    fn receive(&mut self, mut remaining: usize) -> io::Result<bool> {
        let mut chunk = [0; 8192];
        while remaining != 0 {
            let limit = chunk.len().min(remaining);
            let count = match self.reader.read(&mut chunk[..limit]) {
                Ok(0) => return Ok(false),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            let discard = (self.bytes.len() + count).saturating_sub(DIAGNOSTIC_LIMIT);
            self.bytes.drain(..discard);
            self.bytes.extend(&chunk[..count]);
            remaining -= count;
        }
        Ok(true)
    }
}

pub(crate) struct StartingSession(Session);

#[derive(Debug)]
pub(crate) struct FailedTransportExit(ExitStatus);

impl std::fmt::Display for FailedTransportExit {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "native demand transport exited with {}", self.0)
    }
}
impl std::error::Error for FailedTransportExit {}

#[derive(Debug)]
pub(crate) struct FailedTransportCompletion;

impl std::fmt::Display for FailedTransportCompletion {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("cannot confirm completed SSH response")
    }
}
impl std::error::Error for FailedTransportCompletion {}

#[derive(Clone, Copy)]
pub(crate) enum FetchMode {
    Filtered,
    Snapshot,
}

impl StartingSession {
    pub(crate) fn spawn(command: Command, stop: Stop) -> Result<Self> {
        check_transfer(&stop)?;
        Self::start(command, stop, Diagnostics::file()?)
    }

    pub(crate) fn upload_pack(command: Command, stop: Stop) -> Result<Self> {
        check_transfer(&stop)?;
        Self::start(command, stop, DiagnosticChannel::open()?)
    }

    fn start(
        mut command: Command,
        stop: Stop,
        (errors, writer): (Diagnostics, Stdio),
    ) -> Result<Self> {
        check_transfer(&stop)?;
        command.stderr(writer);
        let (child, input, output) =
            spawn_piped(command, stop).context("start invocation demand transport")?;
        Ok(Self(Session {
            owner: Owner::new(child, crate::terminal_ui::preparing()),
            input,
            output,
            errors,
            completed_fetch: AtomicBool::new(false),
        }))
    }

    pub(crate) fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.0.set_stop(stop);
    }

    pub(crate) fn wait_for_advertisement(
        sessions: &[Self],
        mode: FetchMode,
    ) -> io::Result<Option<usize>> {
        check_transfer(&None)?;
        let mut sockets: Vec<_> = sessions
            .iter()
            .map(|session| libc::pollfd {
                fd: session.0.output.get_ref().socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if unsafe { libc::poll(sockets.as_mut_ptr(), sockets.len() as libc::nfds_t, 20) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            return Ok(None);
        }
        let mut partial = false;
        for (index, socket) in sockets.iter().enumerate() {
            if socket.revents & !libc::POLLIN != 0 {
                return Ok(Some(index));
            }
            if socket.revents & libc::POLLIN != 0 {
                if Self::advertisement_ready(socket.fd, mode)? {
                    return Ok(Some(index));
                }
                partial = true;
            }
        }
        // A wrapper can exit while a descendant keeps its stream open.
        if let Some(index) = sessions.iter().position(|starting| starting.0.has_exited()) {
            return Ok(Some(index));
        }
        if partial {
            // The unread prefix keeps poll readable until more bytes arrive.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(None)
    }

    fn advertisement_ready(fd: libc::c_int, mode: FetchMode) -> io::Result<bool> {
        let mut preview = [0; 4096];
        let count = unsafe {
            libc::recv(
                fd,
                preview.as_mut_ptr().cast(),
                preview.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(false),
                _ => Err(error),
            };
        }
        let count = count as usize;
        // GitHub's complete advertisement fits here. Let the ordinary reader
        // handle EOF and larger advertisements.
        if count == 0 || count == preview.len() {
            return Ok(true);
        }
        Ok(!matches!(
            read_v2_capabilities(&mut &preview[..count], mode),
            Err(error) if error.downcast_ref::<io::Error>().is_some_and(|error|
                error.kind() == io::ErrorKind::UnexpectedEof)
        ))
    }

    pub(crate) fn negotiate(self) -> Result<Option<Session>> {
        self.negotiate_for(FetchMode::Filtered)
    }

    pub(crate) fn negotiate_for(mut self, mode: FetchMode) -> Result<Option<Session>> {
        let session = &mut self.0;
        session.observe_leader()?;
        match read_v2_capabilities(&mut session.output, mode) {
            Ok(true) => Ok(Some(self.0)),
            Ok(false) => Ok(None),
            Err(error) => {
                let diagnostic = session.error_text();
                Err(error)
                    .with_context(|| format!("open native demand channel: {}", diagnostic.trim()))
            }
        }
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
    fn observe_leader(&mut self) -> Result<()> {
        let owned = self
            .owner
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        self.output.get_mut().leader = owned.as_ref().map(OwnedChild::id);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn connect(command: Command) -> Result<Option<Self>> {
        StartingSession::upload_pack(command, None)?.negotiate()
    }
    pub(crate) fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.input.1 = Some(stop.clone());
        self.output.get_mut().stop = Some(stop);
    }
    pub(crate) fn select_ref(&mut self, query: &RefQuery) -> Result<Option<Oid>> {
        self.input.write_all(&reference_request(query)?)?;
        read_selected_ref(&mut self.output, query)
    }

    pub(crate) fn fetch_stream(
        &mut self,
        oids: &[Oid],
        mode: FetchMode,
        response: crate::pack_stream::Response<'_>,
    ) -> Result<crate::pack_stream::Received> {
        self.fetch_response(oids, mode, response, false)
    }

    fn fetch_response(
        &mut self,
        oids: &[Oid],
        mode: FetchMode,
        mut response: crate::pack_stream::Response<'_>,
        last_request: bool,
    ) -> Result<crate::pack_stream::Received> {
        response.set_stop(self.input.1.clone());
        let result = (|| {
            if self.output.get_ref().leader.is_none() {
                self.observe_leader()?;
            }
            self.errors.clear()?;
            self.input.write_all(&fetch_request(oids, mode)?)?;
            if last_request {
                // This is the final request. Let the peer close its channel
                // while the pack is still arriving.
                self.close_input()?;
            }
            let pack = response.read(&mut self.output)?;
            self.completed_fetch.store(true, Ordering::Relaxed);
            Ok(pack)
        })();
        self.with_fetch_context(oids, result)
    }

    fn with_fetch_context<T>(&mut self, oids: &[Oid], result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            let status = self.owner.stop_with_status().ok().flatten();
            let diagnostic = self.stderr_tail();
            let error = match status {
                // Cancellation of the current request cannot hide an
                // independent failure on a previously used channel.
                Some(status) if !status.success() => error.context(FailedTransportExit(status)),
                _ => error,
            };
            let wanted = if let [oid] = oids {
                format!("object {oid}")
            } else {
                format!("objects ({} requests, first {})", oids.len(), oids[0])
            };
            error.context(format!(
                "remote fetch of exact {wanted} failed: {}",
                diagnostic.trim()
            ))
        })
    }
    pub(crate) fn fetch_snapshot(
        mut self,
        oid: Oid,
        response: crate::pack_stream::Response<'_>,
        transport: &str,
    ) -> Result<crate::pack_stream::Received> {
        self.observe_leader()?;
        let received = self.fetch_response(&[oid], FetchMode::Snapshot, response, true)?;
        let result = self.verify_exit(transport, || Ok(check_transfer(&self.input.1)?));
        self.with_fetch_context(&[oid], result).map(|()| received)
    }

    pub(crate) fn reuse_ssh_snapshot(
        self,
        oid: Oid,
        response: crate::pack_stream::Response<'_>,
    ) -> Result<Option<crate::pack_stream::Received>> {
        if self.has_exited() {
            self.confirm_ssh_exit().and(self.stop())?;
            return Ok(None);
        }
        self.fetch_snapshot(oid, response, "SSH").map(Some)
    }

    pub(crate) fn fetch_last_ssh(
        mut self,
        oids: &[Oid],
        response: crate::pack_stream::Response<'_>,
    ) -> Result<crate::pack_stream::Received> {
        let received = self.fetch_response(oids, FetchMode::Filtered, response, true)?;
        let result = self
            .verify_exit("SSH", || Ok(check_transfer(&self.input.1)?))
            .context(FailedTransportCompletion);
        self.with_fetch_context(oids, result).map(|()| received)
    }

    fn verify_exit(&self, transport: &str, check: impl Fn() -> Result<()>) -> Result<()> {
        ensure!(
            self.output.buffer().is_empty(),
            "unexpected data after final Git response"
        );
        let status = self.finish_with(true, check)?;
        ensure!(
            status.success(),
            "{transport} transport exited with {status}"
        );
        Ok(())
    }

    pub(crate) fn completed_fetch(&self) -> bool {
        self.completed_fetch.load(Ordering::Relaxed)
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.owner.0.lock().unwrap().as_ref().unwrap().id()
    }

    pub(crate) fn has_exited(&self) -> bool {
        child_exited(self.process_id()).is_ok_and(|exited| exited)
    }

    pub(crate) fn confirm_ssh_exit(&self) -> Result<()> {
        if !self.completed_fetch.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        self.verify_exit("SSH", || {
            // The user's command has ended. This is an orderly close of an
            // idle, used channel, independent of cancelled background reads.
            if crate::git::cancelled_signal().is_some() {
                return Err(crate::object::OperationCancelled.into());
            }
            Ok(())
        })
    }

    fn finish_with(
        &self,
        check_end: bool,
        check: impl Fn() -> Result<()>,
    ) -> Result<std::process::ExitStatus> {
        self.close_input()?;
        // The next native operation can reuse the control master. Wait until
        // the discovery channel releases its remote session slot. A wrapper's
        // detached helper can retain stdout after the SSH process has exited.
        self.owner
            .finish(&self.output.get_ref().socket, check_end, check)
    }

    pub(crate) fn close_input_after_command(&mut self, finished: Arc<AtomicBool>) {
        self.output.get_mut().close_after_command = Some(CloseAfterCommand {
            request: self.input.0.clone(),
            finished,
        });
    }

    pub(crate) fn close_input(&self) -> Result<()> {
        Ok(shutdown_request(&self.input.0)?)
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.owner.stop()
    }
    pub(crate) fn error_text(&mut self) -> String {
        let _ = self.owner.stop();
        self.stderr_tail()
    }
    fn stderr_tail(&mut self) -> String {
        String::from_utf8_lossy(&self.errors.tail()).into_owned()
    }
}

fn shutdown_request(socket: &UnixStream) -> io::Result<()> {
    match socket.shutdown(std::net::Shutdown::Write) {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
            ) =>
        {
            Ok(())
        }
        result => result,
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

fn read_v2_capabilities(input: &mut impl Read, mode: FetchMode) -> Result<bool> {
    if !matches!(read_packet(input)?, Packet::Data(version) if packet_text(&version) == b"version 2")
    {
        return Ok(false);
    }
    let mut fetch = false;
    let mut filter = false;
    let mut shallow = false;
    loop {
        match read_packet(input)? {
            Packet::Flush => break,
            Packet::Data(capability) => {
                if let Some(values) = capability.strip_prefix(b"fetch=") {
                    fetch = true;
                    for value in values.split(u8::is_ascii_whitespace) {
                        filter |= value == b"filter";
                        shallow |= value == b"shallow";
                    }
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
    match mode {
        FetchMode::Filtered => {
            ensure!(fetch && filter, "demand transport lacks object filtering");
            Ok(true)
        }
        FetchMode::Snapshot => Ok(fetch && shallow),
    }
}

#[derive(Clone)]
pub(crate) struct RefQuery {
    selectable: Vec<String>,
    alternatives: Vec<String>,
}

impl RefQuery {
    pub(crate) fn for_network(revision: &std::ffi::OsStr) -> Option<Self> {
        let revision = revision.to_str()?;
        if revision == "HEAD" || revision.starts_with("refs/") {
            return (revision == "HEAD" || git2::Reference::is_valid_name(revision)).then(|| {
                Self {
                    selectable: vec![revision.to_owned()],
                    alternatives: Vec::new(),
                }
            });
        }
        if revision.is_empty() || revision.len() > 1024 || revision.starts_with(['-', '+']) {
            return None;
        }
        let mut selectable = vec![
            format!("refs/heads/{revision}"),
            format!("refs/tags/{revision}"),
        ];
        let mut alternatives = vec![
            revision.to_owned(),
            format!("refs/remotes/{revision}"),
            format!("refs/remotes/{revision}/HEAD"),
        ];
        let under_refs = format!("refs/{revision}");
        if revision.starts_with("heads/") || revision.starts_with("tags/") {
            selectable.push(under_refs);
        } else {
            alternatives.push(under_refs);
        }
        selectable
            .iter()
            .all(|name| git2::Reference::is_valid_name(name))
            .then_some(Self {
                selectable,
                alternatives,
            })
    }

    fn prefixes(&self) -> impl Iterator<Item = &str> {
        self.selectable
            .iter()
            .chain(&self.alternatives)
            .map(String::as_str)
    }
}

pub(crate) fn reference_request(query: &RefQuery) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request_packet(&mut request, b"command=ls-refs\n")?;
    request.extend_from_slice(b"0001");
    for prefix in query.prefixes() {
        request_packet(&mut request, format!("ref-prefix {prefix}\n").as_bytes())?;
    }
    request.extend_from_slice(b"0000");
    Ok(request)
}

#[derive(Default)]
struct SelectedRef {
    oid: Option<Oid>,
    ambiguous: bool,
}

impl SelectedRef {
    fn observe(&mut self, query: &RefQuery, line: &[u8]) -> Result<()> {
        let mut fields = packet_text(line).split(|&byte| byte == b' ');
        let id = fields.next().unwrap_or_default();
        let name = fields.next().unwrap_or_default();
        if query
            .selectable
            .iter()
            .any(|candidate| candidate.as_bytes() == name)
        {
            self.ambiguous |= self.oid.is_some();
            ensure!(
                id.len() == 40 && id.iter().all(u8::is_ascii_hexdigit),
                "invalid Git reference object ID"
            );
            self.oid = Some(Oid::from_str(std::str::from_utf8(id)?)?);
        } else if query
            .alternatives
            .iter()
            .any(|candidate| candidate.as_bytes() == name)
        {
            self.ambiguous = true;
        }
        Ok(())
    }

    fn oid(&self) -> Option<Oid> {
        self.oid.filter(|_| !self.ambiguous)
    }
}

fn read_selected_ref(input: &mut impl Read, query: &RefQuery) -> Result<Option<Oid>> {
    let mut selected = SelectedRef::default();
    loop {
        match read_packet(input)? {
            Packet::Data(line) => selected.observe(query, &line)?,
            Packet::Flush => return Ok(selected.oid()),
            _ => return Err(invalid("invalid Git reference response")),
        }
    }
}

/// Keep at most one incomplete pkt-line between HTTP callbacks. Short ref
/// prefixes can match many entries even when just one exact ref is selected.
pub(crate) struct HttpRefs {
    query: RefQuery,
    selected: SelectedRef,
    pending: Vec<u8>,
    flushed: bool,
}

impl HttpRefs {
    pub(crate) fn new(query: RefQuery) -> Self {
        Self {
            query,
            selected: SelectedRef::default(),
            pending: Vec::new(),
            flushed: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.selected = SelectedRef::default();
        self.pending.clear();
        self.flushed = false;
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.extend_from_slice(bytes);
        let mut input = self.pending.as_slice();
        while !self.flushed && input.len() >= 4 {
            let length = packet_length(input[..4].try_into().unwrap())?;
            if length == 0 {
                self.flushed = true;
                input = &input[4..];
                break;
            }
            ensure!(
                (4..=65520).contains(&length),
                "invalid Git reference response"
            );
            let Some((packet, rest)) = input.split_at_checked(length) else {
                break;
            };
            check_packet_error(&packet[4..])?;
            self.selected.observe(&self.query, &packet[4..])?;
            input = rest;
        }
        ensure!(
            !self.flushed || b"0002".starts_with(input),
            "unexpected data after the Git HTTP response"
        );
        let consumed = self.pending.len() - input.len();
        self.pending.drain(..consumed);
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<Option<Oid>> {
        ensure!(
            self.flushed && (self.pending.is_empty() || self.pending == b"0002"),
            "incomplete Git HTTP ref reply"
        );
        Ok(self.selected.oid())
    }
}

pub(crate) fn read_http_capabilities(mut input: &[u8], mode: FetchMode) -> Result<bool> {
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
    read_v2_capabilities(&mut input, mode)
}

pub(crate) fn fetch_request(oids: &[Oid], mode: FetchMode) -> Result<Vec<u8>> {
    // One small exact-demand request, not one syscall/signal-status read
    // for every pkt-line header and body. Every wanted OID is explicit.
    let mut request = Vec::with_capacity(256);
    request_packet(&mut request, b"command=fetch\n")?;
    request.extend_from_slice(b"0001");
    for oid in oids {
        request_packet(&mut request, format!("want {oid}\n").as_bytes())?;
    }
    request_packet(&mut request, b"deepen 1\n")?;
    let filtered = !matches!(mode, FetchMode::Snapshot);
    if filtered {
        request_packet(&mut request, b"filter tree:0\n")?;
    } else {
        request_packet(&mut request, b"ofs-delta\n")?;
    }
    if filtered || !crate::terminal_ui::progress_enabled() {
        request_packet(&mut request, b"no-progress\n")?;
    }
    request_packet(&mut request, b"done\n")?;
    request.extend_from_slice(b"0000");
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_checks_the_exit_after_its_last_response() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "read request; printf 'response\\n'; /bin/cat >/dev/null; echo 'reader diagnostic' >&2; exit 7",
        ]);
        let mut decoder = BatchProcess::spawn(command)?;
        let response = decoder.exchange(b"request\n", |input| {
            let mut response = String::new();
            input.read_line(&mut response)?;
            Ok(response)
        })?;
        assert_eq!(response, "response\n");
        let error = decoder.finish().unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("exit status: 7"), "{error}");
        assert!(error.contains("reader diagnostic"), "{error}");
        Ok(())
    }

    #[test]
    fn destination_failure_stops_a_batch_reader_before_the_rest_of_the_object() -> Result<()> {
        use std::time::Duration;
        let directory = tempfile::tempdir()?;
        let release = directory.path().join("continue");
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s blob 8\\nx' \"$2\"; while [ ! -f \"$1\" ]; do sleep 0.02; done",
            "batch-reader",
        ]);
        let oid = git2::Oid::ZERO_SHA1;
        command.arg(&release).arg(oid.to_string());
        let mut decoder = BatchProcess::spawn(command)?;
        let (failed, failure) = std::sync::mpsc::channel();
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> Result<()> {
            scope.spawn(move || {
                let result = decoder
                    .exchange(format!("{oid}\n").as_bytes(), |input| {
                        crate::git::receive_batch_object(
                            oid,
                            crate::git::BatchRead::Materialize,
                            input,
                            |_, _, data| {
                                let mut prefix = [0];
                                data.read_exact(&mut prefix)?;
                                assert_eq!(prefix, *b"x");
                                failed.send(())?;
                                Err::<(), _>(io::Error::from_raw_os_error(libc::ENOSPC).into())
                            },
                        )
                    })
                    .and_then(std::convert::identity);
                let _ = done.send(result);
            });
            let result = failure
                .recv_timeout(Duration::from_secs(5))
                .and_then(|()| outcome.recv_timeout(Duration::from_secs(5)));
            fs::File::create(&release)?;
            let error = result
                .context("destination failure waited for the remaining object data")?
                .expect_err("destination failure must reach the materializer");
            assert_eq!(
                error
                    .downcast_ref::<io::Error>()
                    .and_then(io::Error::raw_os_error),
                Some(libc::ENOSPC),
                "{error:#}"
            );
            Ok(())
        })
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn group_stop_preserves_exit_when_darwin_reports_eperm_before_waitid() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 17"]);
        let (child, _request, _reply) = spawn_piped(command, None)?;
        let (status, exited) = Owner::reap_after_group_signal(
            &mut Some(OwnedChild {
                process: child,
                terminal: None,
            }),
            false,
            Some(io::Error::from_raw_os_error(libc::EPERM)),
        )?;
        assert_eq!(status.code(), Some(17));
        assert!(exited);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn group_stop_returns_darwin_eperm_while_the_leader_is_alive() -> Result<()> {
        let (child, request, _reply) = spawn_piped(Command::new("/bin/cat"), None)?;
        let mut owned = Some(OwnedChild {
            process: child,
            terminal: None,
        });
        let (release, hold) = std::sync::mpsc::channel();
        let result = std::thread::scope(|scope| {
            scope.spawn(move || {
                // Bound the red test too. The child otherwise waits for EOF.
                let _ = hold.recv_timeout(std::time::Duration::from_secs(2));
                drop(request);
            });
            let result = Owner::reap_after_group_signal(
                &mut owned,
                false,
                Some(io::Error::from_raw_os_error(libc::EPERM)),
            );
            let _ = release.send(());
            result
        });
        let child = owned.context("signal denial discarded the live child")?;
        assert!(child.wait()?.success());
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error),
            Some(libc::EPERM)
        );
        Ok(())
    }

    #[test]
    fn an_expired_channel_leaves_copy_free_to_open_a_new_one() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '000eversion 2\\n0019fetch=shallow filter\\n0000'",
        ]);
        let session = Session::connect(command)?.context("local channel did not negotiate")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !session.has_exited() {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "local channel did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let directory = tempfile::tempdir()?;
        let scratch = crate::raw_blob::RawBlobs::new(directory.path())?;
        let oid = Oid::from_bytes(&[1; 20])?;
        let response = crate::pack_stream::Response::for_materialization(scratch, oid);
        assert!(session.reuse_ssh_snapshot(oid, response)?.is_none());
        Ok(())
    }
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
            owner: Owner(Mutex::new(None)),
            input: Output(Arc::new(input), None),
            output: BufReader::new(Input {
                socket: output,
                stop: None,
                leader: None,
                close_after_command: None,
            }),
            errors: Diagnostics::File(CaptureFile::fixture(tempfile::tempfile().unwrap())),
            completed_fetch: AtomicBool::new(false),
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
    fn ref_selection_ignores_longer_prefixes_and_rejects_ambiguous_names() {
        let query = RefQuery::for_network(std::ffi::OsStr::new("release/candidate")).unwrap();
        let mut response = Vec::new();
        let selected = Oid::from_bytes(&[3; 20]).unwrap();
        let other = Oid::from_bytes(&[4; 20]).unwrap();
        for (oid, name) in [
            (selected, "refs/heads/release/candidate"),
            (other, "refs/heads/release/candidate-next"),
        ] {
            text_packet(&mut response, format!("{oid} {name}").as_bytes(), true);
        }
        let mut http = HttpRefs::new(query.clone());
        for part in [response.as_slice(), b"00000002"].concat().chunks(3) {
            http.feed(part).unwrap();
        }
        assert_eq!(http.finish().unwrap(), Some(selected));
        text_packet(
            &mut response,
            format!("{other} refs/tags/release/candidate").as_bytes(),
            true,
        );
        response.extend_from_slice(b"0000");
        let (mut session, _peer) = peer_response(&response);
        assert_eq!(session.select_ref(&query).unwrap(), None);
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
                assert!(read_v2_capabilities(&mut session.output, FetchMode::Filtered).unwrap());
            }
        }
        for lf in [false, true] {
            let mut bytes = Vec::new();
            text_packet(&mut bytes, b"version 2", true);
            text_packet(&mut bytes, b"fetch=shallow filter", true);
            text_packet(&mut bytes, b"object-format=sha256", lf);
            bytes.extend_from_slice(b"0000");
            let (mut session, _request_peer) = peer_response(&bytes);
            assert!(read_v2_capabilities(&mut session.output, FetchMode::Filtered).is_err());
        }
    }
    #[test]
    fn fetch_sections_accept_optional_lf_without_changing_binary() {
        let root = tempfile::tempdir().unwrap();
        // Include a binary trailing LF: only text labels may be normalized.
        let (payload, good) = (0..65536)
            .map(|index| {
                let payload = format!("binary payload {index}");
                let pack = pack(payload.as_bytes());
                (payload, pack)
            })
            .find(|(_, bytes)| bytes.last() == Some(&b'\n'))
            .unwrap();
        let cache = crate::raw_blob::RawBlobs::new(root.path()).unwrap();
        let oid = Oid::hash_object(git2::ObjectType::Blob, payload.as_bytes()).unwrap();
        for shallow_lf in [false, true] {
            for pack_lf in [false, true] {
                let mut bytes = Vec::new();
                text_packet(&mut bytes, b"shallow-info", shallow_lf);
                text_packet(
                    &mut bytes,
                    format!("shallow {}", Oid::ZERO_SHA1).as_bytes(),
                    shallow_lf,
                );
                bytes.extend_from_slice(b"0001");
                text_packet(&mut bytes, b"packfile", pack_lf);
                write!(bytes, "{:04x}", good.len() + 5).unwrap();
                bytes.push(1);
                bytes.extend_from_slice(&good);
                bytes.extend_from_slice(b"0000");
                let (mut session, _request_peer) = peer_response(&bytes);
                let response = crate::pack_stream::Response::new(
                    cache.clone(),
                    vec![oid],
                    Box::new(|object| {
                        assert_eq!(object.data(), payload.as_bytes());
                        Ok(())
                    }),
                );
                let received = session
                    .fetch_stream(&[oid], FetchMode::Filtered, response)
                    .unwrap();
                assert_eq!(received.bytes, good.len() as u64);
            }
        }
    }
    #[test]
    fn pack_trailer_is_checked_across_all_short_chunk_boundaries() {
        let good = pack(b"payload");
        for step in 1..=good.len() {
            let mut checksum = crate::pack_stream::Checksum::default();
            for bytes in good.chunks(step) {
                checksum.update(bytes);
            }
            checksum.finish().unwrap();
        }
        let mut bad = good.clone();
        bad[15] ^= 1;
        for truncated_or_bad in [&good[..19], &bad[..]] {
            let mut checksum = crate::pack_stream::Checksum::default();
            checksum.update(truncated_or_bad);
            assert!(checksum.finish().is_err());
        }
    }
    #[test]
    fn direct_pack_bytes_require_oid_zlib_end_and_no_hidden_tail() {
        let root = tempfile::tempdir().unwrap();
        let cache = crate::raw_blob::RawBlobs::new(root.path()).unwrap();
        let receive = |pack: &[u8], oid: Oid| {
            let mut bytes = b"000dpackfile\n".to_vec();
            write!(bytes, "{:04x}", pack.len() + 5).unwrap();
            bytes.push(1);
            bytes.extend_from_slice(pack);
            bytes.extend_from_slice(b"0000");
            let mut published = None;
            let response = crate::pack_stream::Response::new(
                cache.clone(),
                vec![oid],
                Box::new(|object| {
                    assert_eq!(object.kind(), git2::ObjectType::Blob);
                    assert_eq!(object.oid(), oid);
                    published = Some(Arc::unwrap_or_clone(object.into_data()));
                    Ok(())
                }),
            );
            let (mut session, _peer) = peer_response(&bytes);
            session
                .fetch_stream(&[oid], FetchMode::Filtered, response)
                .map(|_| published.unwrap())
        };
        for data in [Vec::new(), vec![b'x'; 1024 * 1024]] {
            let good = pack(&data);
            let oid = Oid::hash_object(git2::ObjectType::Blob, &data).unwrap();
            assert_eq!(receive(&good, oid).unwrap(), data);
            assert!(receive(&good, Oid::ZERO_SHA1).is_err());
            let body = &good[..good.len() - 20];
            let truncated = checksum(body[..body.len() - 2].to_vec());
            assert!(receive(&truncated, oid).is_err());
            let trailing = checksum([body, b"trailing"].concat());
            assert!(receive(&trailing, oid).is_err());
        }
    }
    #[test]
    fn malformed_packet_and_pack_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let cache = crate::raw_blob::RawBlobs::new(root.path()).unwrap();
        for malformed in [b"-001".as_slice(), b"ffff", b"0003", b"00"] {
            let (mut session, _peer) = peer_response(malformed);
            let response = crate::pack_stream::Response::new(
                cache.clone(),
                vec![Oid::ZERO_SHA1],
                Box::new(|_| panic!("malformed response published an object")),
            );
            assert!(session
                .fetch_stream(&[Oid::ZERO_SHA1], FetchMode::Filtered, response)
                .is_err());
        }
    }
    #[test]
    fn transport_exit_before_first_reply_reports_eof() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        // The descendant holds the response descriptor after the leader exits.
        command.args(["-c", "sleep 10 & echo early-transport-exit >&2; exit 7"]);
        let sessions = [StartingSession::upload_pack(command, None)?];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while StartingSession::wait_for_advertisement(&sessions, FetchMode::Filtered)?.is_none() {
            ensure!(
                std::time::Instant::now() < deadline,
                "exited transport did not notify the handshake while its response stayed open"
            );
        }
        let [starting] = sessions;
        let error = starting
            .negotiate()
            .err()
            .context("early transport exit was accepted")?;
        assert!(
            format!("{error:#}").contains("early-transport-exit"),
            "{error:#}"
        );
        Ok(())
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
        let pid = session.owner.0.lock().unwrap().as_ref().unwrap().id();
        drop(session);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn cancelled_fetch_keeps_an_independent_transport_failure() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '000eversion 2\\n0019fetch=shallow filter\\n0000'; exit 49",
        ]);
        let mut session = Session::connect(command)?.context("persistent transport")?;
        let pid = session.owner.0.lock().unwrap().as_ref().unwrap().id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !child_exited(pid)? {
            assert!(std::time::Instant::now() < deadline, "fixture did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let root = tempfile::tempdir()?;
        let cache = crate::raw_blob::RawBlobs::new(root.path())?;
        let oid = Oid::ZERO_SHA1;
        session.set_stop(Arc::new(AtomicBool::new(true)));
        let response = crate::pack_stream::Response::new(
            cache,
            vec![oid],
            Box::new(|_| panic!("cancelled response published an object")),
        );
        let error = session
            .fetch_stream(&[oid], FetchMode::Filtered, response)
            .err()
            .context("cancelled fetch succeeded")?;
        assert!(crate::object::is_cancelled(&error));
        assert!(error.is::<FailedTransportExit>(), "{error:#}");
        assert!(format!("{error:#}").contains("exit status: 49"));
        Ok(())
    }

    #[test]
    fn native_error_tail_has_bounded_memory() -> Result<()> {
        let capture = Diagnostics::capture(CaptureFile::fixture(tempfile::tempfile()?))?;
        for (mut diagnostics, writer) in [capture, DiagnosticChannel::open()?] {
            let status = Command::new("/bin/sh")
                .args([
                    "-c",
                    "dd if=/dev/zero bs=8192 count=64 >&2 2>/dev/null; printf 'actual failure' >&2",
                ])
                .stderr(writer)
                .status()?;
            assert!(status.success());
            let message = diagnostics.tail();
            assert_eq!(message.len(), DIAGNOSTIC_LIMIT);
            assert!(message.ends_with(b"actual failure"));
            diagnostics.clear()?;
            assert!(diagnostics.tail().is_empty());
        }
        Ok(())
    }
}
#[test]
fn http_advertisement_accepts_the_version_two_service_preamble() {
    let mut capabilities = Vec::new();
    request_packet(&mut capabilities, b"version 2\n").unwrap();
    request_packet(&mut capabilities, b"fetch=shallow filter\n").unwrap();
    capabilities.extend_from_slice(b"0000");
    assert!(read_http_capabilities(&capabilities, FetchMode::Filtered).unwrap());

    let mut announced = Vec::new();
    request_packet(&mut announced, b"# service=git-upload-pack\n").unwrap();
    announced.extend_from_slice(b"0000");
    announced.extend_from_slice(&capabilities);
    assert!(read_http_capabilities(&announced, FetchMode::Filtered).unwrap());
}
