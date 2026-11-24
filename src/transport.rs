//! Invocation-owned native transports. Protocol v2 permits branch selection
//! and successive exact-object requests on the same channel.
use crate::service_memory::CaptureFile;
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

pub(crate) fn child_exited(pid: u32) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(crate::platform::siginfo_pid(&info) != 0)
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
struct Owner(Mutex<Option<Child>>);
impl Owner {
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

    fn kill_and_reap(owned: &mut Option<Child>, exited: bool) -> Result<(ExitStatus, bool)> {
        let error = Self::stop_group(owned.as_ref().expect("owned transport").id());
        Self::reap_after_group_signal(owned, exited, error)
    }

    fn reap_after_group_signal(
        owned: &mut Option<Child>,
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
}

impl BatchProcess {
    pub(crate) fn spawn(command: Command) -> Result<Self> {
        let (child, input, mut output) =
            spawn_piped(command, None).context("start native Git object reader")?;
        output.get_mut().leader = Some(child.id());
        Ok(Self {
            owner: Owner(Mutex::new(Some(child))),
            input,
            output,
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
            match status {
                Some(status) if !status.success() => {
                    error.context(format!("native Git object reader exited with {status}"))
                }
                _ => error,
            }
        })
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.owner.stop()
    }

    pub(crate) fn finish(&self) -> Result<()> {
        ensure!(
            self.output.buffer().is_empty(),
            "unexpected data after the last Git object"
        );
        shutdown_request(&self.input.0)?;
        let status = self.owner.finish(&self.output.get_ref().socket, true, || {
            Ok(check_transfer(&self.input.1)?)
        })?;
        ensure!(
            status.success(),
            "native Git object reader exited with {status}"
        );
        Ok(())
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

    pub(crate) fn is_stateless(&self) -> bool {
        self.helper
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
        let command = self.command(additional)?;
        let starting = if self.helper || self.is_ssh() {
            StartingSession::spawn(command, self.helper, stop)?
        } else {
            StartingSession::upload_pack(command, stop)?
        };
        starting.negotiate()
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
    errors: Diagnostics,
    stateless: bool,
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
    FilteredSelection { selecting_ref: bool },
    Snapshot { selecting_ref: bool },
}

impl StartingSession {
    pub(crate) fn spawn(command: Command, helper: bool, stop: Stop) -> Result<Self> {
        check_transfer(&stop)?;
        Self::start(command, helper, stop, Diagnostics::file()?)
    }

    pub(crate) fn upload_pack(command: Command, stop: Stop) -> Result<Self> {
        check_transfer(&stop)?;
        Self::start(command, false, stop, DiagnosticChannel::open()?)
    }

    fn start(
        mut command: Command,
        helper: bool,
        stop: Stop,
        (errors, writer): (Diagnostics, Stdio),
    ) -> Result<Self> {
        check_transfer(&stop)?;
        command.stderr(writer);
        let (child, input, output) =
            spawn_piped(command, stop).context("start invocation demand transport")?;
        Ok(Self(Session {
            owner: Owner(Mutex::new(Some(child))),
            input,
            output,
            errors,
            stateless: helper,
            completed_fetch: AtomicBool::new(false),
        }))
    }

    pub(crate) fn wait_for_advertisement(sessions: &[Self]) -> io::Result<Option<usize>> {
        check_transfer(&None)?;
        let mut sockets: Vec<_> = sessions
            .iter()
            .map(|session| libc::pollfd {
                fd: session.0.output.get_ref().socket.as_raw_fd(),
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
                self.0.output.get_ref().socket.as_raw_fd(),
                preview.as_mut_ptr().cast(),
                preview.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        read > 0
            && read_v2_capabilities(&mut &preview[..read as usize], FetchMode::Filtered)
                .unwrap_or(false)
    }

    pub(crate) fn negotiate(mut self) -> Result<Option<Session>> {
        let session = &mut self.0;
        match session.handshake(session.stateless, FetchMode::Filtered) {
            Ok(true) => Ok(Some(self.0)),
            Ok(false) => Ok(None),
            Err(error) => {
                let diagnostic = session.error_text();
                Err(error)
                    .with_context(|| format!("open native demand channel: {}", diagnostic.trim()))
            }
        }
    }

    pub(crate) fn negotiate_selection(
        mut self,
        branch: Option<&BranchQuery>,
        mode: FetchMode,
    ) -> Result<Option<(Session, Option<Oid>)>> {
        self.0.observe_leader()?;
        if self.0.handshake(false, mode).unwrap_or(false) {
            match branch {
                None => return Ok(Some((self.0, None))),
                Some(query) => {
                    if let Ok(Some(oid)) = self.0.select_branch(query) {
                        return Ok(Some((self.0, Some(oid))));
                    }
                }
            }
        }
        self.0.finish(false)?;
        Ok(None)
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
    fn observe_leader(&mut self) -> Result<()> {
        let owned = self
            .owner
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport owner poisoned"))?;
        self.output.get_mut().leader = owned.as_ref().map(Child::id);
        Ok(())
    }

    /// A missing protocol/helper capability is an optional optimization miss,
    /// before any fetch. Errors after a fetch request must never retry it.
    #[cfg(test)]
    pub(crate) fn connect(command: Command, helper: bool) -> Result<Option<Self>> {
        StartingSession::start(command, helper, None, DiagnosticChannel::open()?)?.negotiate()
    }
    pub(crate) fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.input.1 = Some(stop.clone());
        self.output.get_mut().stop = Some(stop);
    }
    fn handshake(&mut self, helper: bool, mode: FetchMode) -> Result<bool> {
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
        read_v2_capabilities(&mut self.output, mode)
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
        check_transfer(&self.output.get_ref().stop)?;
        if !self.output.buffer().is_empty() {
            return Ok(true);
        }
        let mut socket = libc::pollfd {
            fd: self.output.get_ref().socket.as_raw_fd(),
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
            if !self.stateless && self.output.get_ref().leader.is_none() {
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
            self.check_helper_end()?;
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
    fn check_helper_end(&mut self) -> Result<()> {
        if self.stateless {
            ensure!(
                read_packet(&mut self.output)? == Packet::End,
                "missing helper response end"
            );
        }
        Ok(())
    }

    pub(crate) fn fetch_snapshot(
        mut self,
        oid: Oid,
        response: crate::pack_stream::Response<'_>,
        transport: &str,
    ) -> Result<crate::pack_stream::Received> {
        self.observe_leader()?;
        let received = self.fetch_response(
            &[oid],
            FetchMode::Snapshot {
                selecting_ref: false,
            },
            response,
            true,
        )?;
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

    fn finish(&self, check_end: bool) -> Result<std::process::ExitStatus> {
        self.finish_with(check_end, || Ok(check_transfer(&self.input.1)?))
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
    let mut ref_in_want = false;
    loop {
        match read_packet(input)? {
            Packet::Flush => break,
            Packet::Data(capability) => {
                if let Some(values) = capability.strip_prefix(b"fetch=") {
                    fetch = true;
                    for value in values.split(u8::is_ascii_whitespace) {
                        filter |= value == b"filter";
                        shallow |= value == b"shallow";
                        ref_in_want |= value == b"ref-in-want";
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
        // Native Git can request the advertised ref by name when the peer
        // supports ref-in-want (for example, across load-balanced replicas).
        FetchMode::FilteredSelection { selecting_ref } | FetchMode::Snapshot { selecting_ref } => {
            let filtered = matches!(mode, FetchMode::FilteredSelection { .. });
            Ok(fetch && shallow && (!filtered || filter) && !(selecting_ref && ref_in_want))
        }
    }
}

#[derive(Clone)]
pub(crate) struct BranchQuery {
    wanted: String,
    alternatives: Vec<String>,
}

impl BranchQuery {
    pub(crate) fn for_snapshot(revision: &std::ffi::OsStr) -> Option<Self> {
        if revision == "HEAD" {
            Some(Self {
                wanted: "HEAD".into(),
                alternatives: Vec::new(),
            })
        } else {
            Self::new(revision)
        }
    }

    pub(crate) fn new(revision: &std::ffi::OsStr) -> Option<Self> {
        let revision = revision.to_str()?;
        if revision.is_empty()
            || revision.len() > 1024
            || revision.starts_with('-')
            || revision.starts_with('+')
            || matches!(revision, "HEAD" | "tag")
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
    let filtered = !matches!(mode, FetchMode::Snapshot { .. });
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

fn check_http_end(input: impl Read) -> Result<()> {
    let mut trailer = Vec::new();
    input.take(5).read_to_end(&mut trailer)?;
    ensure!(
        trailer.is_empty() || trailer == b"0002",
        "unexpected data after the Git HTTP response"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_checks_the_exit_after_its_last_response() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "read request; printf 'response\\n'; /bin/cat >/dev/null; exit 7",
        ]);
        let mut decoder = BatchProcess::spawn(command)?;
        let response = decoder.exchange(b"request\n", |input| {
            let mut response = String::new();
            input.read_line(&mut response)?;
            Ok(response)
        })?;
        assert_eq!(response, "response\n");
        let error = decoder.finish().unwrap_err();
        assert!(error.to_string().contains("exit status: 7"), "{error:#}");
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
        let oid = git2::Oid::zero();
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
            &mut Some(child),
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
        let mut owned = Some(child);
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
        let mut child = owned.context("signal denial discarded the live child")?;
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
        let session =
            Session::connect(command, false)?.context("local channel did not negotiate")?;
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
            stateless: false,
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
                assert!(session.handshake(false, FetchMode::Filtered).unwrap());
            }
        }
        for lf in [false, true] {
            let mut bytes = Vec::new();
            text_packet(&mut bytes, b"version 2", true);
            text_packet(&mut bytes, b"fetch=shallow filter", true);
            text_packet(&mut bytes, b"object-format=sha256", lf);
            bytes.extend_from_slice(b"0000");
            let (mut session, _request_peer) = peer_response(&bytes);
            assert!(session.handshake(false, FetchMode::Filtered).is_err());
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
                    format!("shallow {}", Oid::zero()).as_bytes(),
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
            assert!(receive(&good, Oid::zero()).is_err());
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
                vec![Oid::zero()],
                Box::new(|_| panic!("malformed response published an object")),
            );
            assert!(session
                .fetch_stream(&[Oid::zero()], FetchMode::Filtered, response)
                .is_err());
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
    fn cancelled_fetch_keeps_an_independent_transport_failure() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '000eversion 2\\n0019fetch=shallow filter\\n0000'; exit 49",
        ]);
        let mut session = Session::connect(command, false)?.context("persistent transport")?;
        let pid = session.owner.0.lock().unwrap().as_ref().unwrap().id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !child_exited(pid)? {
            assert!(std::time::Instant::now() < deadline, "fixture did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let root = tempfile::tempdir()?;
        let cache = crate::raw_blob::RawBlobs::new(root.path())?;
        let oid = Oid::zero();
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
