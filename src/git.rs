//! Native Git configuration and signature policy. No replacement objects.
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct Git {
    pub cwd: PathBuf,
    config: Vec<(OsString, Option<OsString>)>,
    original_dir: Option<PathBuf>,
}
impl Git {
    pub fn capture(cwd: &Path) -> Result<Self> {
        let mut command = Command::new("git");
        command
            .current_dir(cwd)
            .args(["config", "--null", "--list", "--includes"])
            .stdin(Stdio::null());
        let output = run_builtin(command).map_err(|error| {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) {
                error.context("native Git was not found on PATH; revision resolution requires native Git; install Git with your distribution tools or select an existing Git on PATH")
            } else {
                error.context("read effective Git configuration")
            }
        })?;
        if !output.status.success() {
            bail!(
                "read effective Git configuration: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut config = Vec::new();
        for record in output.stdout.split(|b| *b == 0).filter(|x| !x.is_empty()) {
            let (key, value) = record.split_once_byte(b'\n');
            // Includes were expanded in the caller's original repository context.
            if key.starts_with(b"include.") || key.starts_with(b"includeif.") {
                continue;
            }
            config.push((
                OsString::from_vec(key.to_vec()),
                value.map(|v| OsString::from_vec(v.to_vec())),
            ));
        }
        Ok(Self {
            cwd: cwd.to_path_buf(),
            config,
            original_dir: {
                let mut command = Command::new("git");
                command
                    .current_dir(cwd)
                    .args(["rev-parse", "--absolute-git-dir"])
                    .stdin(Stdio::null());
                run_builtin(command)
                    .ok()
                    .filter(|x| x.status.success())
                    .map(|x| {
                        PathBuf::from(OsString::from_vec(
                            x.stdout.strip_suffix(b"\n").unwrap_or(&x.stdout).to_vec(),
                        ))
                    })
            },
        })
    }
    pub fn command(&self, dir: Option<&Path>) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.cwd)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1");
        if let Some(dir) = dir {
            if self.original_dir.as_deref() != Some(dir) {
                // Replay each effective value exactly once in the private repo.
                command
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env_remove("GIT_CONFIG_PARAMETERS")
                    .env_remove("GIT_CONFIG_COUNT");
                let mut count = 0;
                for (key, value) in &self.config {
                    // Object/ref storage belongs to the private repository,
                    // rather than the caller's format. Replaying format zero
                    // over a newly created promisor pack would make native Git
                    // reject its intentionally missing tree objects.
                    if key == "core.repositoryformatversion"
                        || key.as_bytes().starts_with(b"extensions.")
                        // The deprecated loose-object durability switch can
                        // override core.fsync=none. Private objects are never
                        // resumed after a crash; preserve the caller's setting
                        // only for operations in its own repository.
                        || key == "core.fsyncobjectfiles"
                    {
                        continue;
                    }
                    command.env(format!("GIT_CONFIG_KEY_{count}"), key).env(
                        format!("GIT_CONFIG_VALUE_{count}"),
                        value.as_deref().unwrap_or(OsStr::new("true")),
                    );
                    count += 1;
                }
                command.env("GIT_CONFIG_COUNT", count.to_string());
                for key in [
                    "GIT_DIR",
                    "GIT_WORK_TREE",
                    "GIT_COMMON_DIR",
                    "GIT_OBJECT_DIRECTORY",
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    "GIT_INDEX_FILE",
                ] {
                    command.env_remove(key);
                }
            }
            command
                .arg("--git-dir")
                .arg(dir)
                .args(["-c", "core.bare=true"]);
        }
        if let Some(program) = self.config.iter().rev().find_map(|(key, value)| {
            (key == "core.askpass")
                .then_some(value.as_deref())
                .flatten()
        }) {
            crate::terminal_ui::core_askpass(&mut command, program);
        }
        command
    }
    pub(crate) fn demand_session_compatible(&self) -> Result<bool> {
        // Keep caller-defined server programs, server options and fsck policy
        // on native fetch's complete compatibility path.
        if self.config.iter().any(|(key, value)| {
            let key = key.as_bytes();
            key.ends_with(b".uploadpack")
                || key.ends_with(b".vcs")
                || key.ends_with(b".serveroption")
                || key.starts_with(b"fsck.")
                || key == b"fetch.fsckobjects"
                || key == b"transfer.fsckobjects"
                || (key == b"protocol.version" && value.as_deref() != Some(OsStr::new("2")))
        }) {
            return Ok(false);
        }

        let Some(inherited) = std::env::var_os("GIT_EXEC_PATH") else {
            return Ok(true);
        };
        // Git exports its standard helper directory to external subcommands.
        // A genuinely custom directory still selects the compatibility path.
        let mut command = self.command(None);
        command
            .arg("--exec-path")
            .env_remove("GIT_EXEC_PATH")
            .stdin(Stdio::null());
        let result = run_builtin(command)?;
        Ok(result.status.success()
            && result.stdout.strip_suffix(b"\n") == Some(inherited.as_bytes()))
    }
    pub(crate) fn http2_compatible(&self) -> bool {
        // Native Git remains responsible for explicit HTTP configuration
        // and transport policy. Apply this before any libcurl discovery.
        self.transport_policy_is_default("https")
            && !self.config.iter().any(|(key, _)| {
                let key = key.as_bytes();
                key.starts_with(b"http.")
                    || key.starts_with(b"remote.")
                        && (key.ends_with(b".proxy") || key.ends_with(b".proxyauthmethod"))
            })
            && !std::env::vars_os().any(|(key, _)| {
                let key = key.as_bytes();
                key.starts_with(b"GIT_HTTP_")
                    || key.starts_with(b"GIT_SSL_") && key != b"GIT_SSL_CAINFO"
                    || matches!(
                        key,
                        b"GIT_TRACE_CURL" | b"GIT_TRACE_PACKET" | b"GIT_CURL_VERBOSE"
                    )
            })
    }
    pub(crate) fn transport_policy_is_default(&self, protocol: &str) -> bool {
        let key = format!("protocol.{protocol}.allow");
        !self
            .config
            .iter()
            .any(|(configured, _)| configured == "protocol.allow" || configured == key.as_str())
            && std::env::var_os("GIT_ALLOW_PROTOCOL").is_none()
            && std::env::var_os("GIT_PROTOCOL_FROM_USER").is_none()
    }

    pub(crate) fn sets_protocol_version(&self) -> bool {
        self.config.iter().any(|(key, _)| key == "protocol.version")
    }

    pub fn output(&self, dir: Option<&Path>, args: &[&OsStr]) -> Result<Vec<u8>> {
        let mut command = self.command(dir);
        command.args(args).stdin(Stdio::null());
        let out = if local_query(args) {
            run_builtin(command)
        } else {
            run(command, true)
        }?;
        if !out.status.success() {
            bail!(
                "Git {} failed: {}",
                args[0].to_string_lossy(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }
    pub fn resolve(&self, revision: &OsStr) -> Result<Oid> {
        let out = self.output(
            None,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                revision,
            ],
        )?;
        Oid::from_str(std::str::from_utf8(&out)?.trim())
            .context("resolve exact SHA-1 revision (SHA-256 repositories unsupported)")
    }
    pub fn repository_dir(&self) -> Result<&Path> {
        self.original_dir
            .as_deref()
            .context("native Git did not identify the selected repository directory")
    }
    pub fn verify(&self, dir: &Path, oid: Oid, kind: git2::ObjectType) -> Result<()> {
        let operation = match kind { git2::ObjectType::Commit => "verify-commit", git2::ObjectType::Tag => "verify-tag", _ => bail!("--verify requires a signed commit or annotated tag; raw trees and blobs have no signature") };
        let mut command = self.command(Some(dir));
        command
            .arg(operation)
            .arg(oid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // Native Git owns acceptance, including the caller's configured trust threshold.
        // Its default can accept unknown-trust OpenPGP; a git-on threshold here
        // would silently replace that policy.
        let status = run(command, false)?.status;
        if !status.success() {
            bail!("native Git {operation} failed for {oid}; execution forbidden");
        }
        Ok(())
    }
    pub fn ssh_command(&self) -> Option<OsString> {
        std::env::var_os("GIT_SSH_COMMAND")
            .or_else(|| {
                self.config.iter().rev().find_map(|(key, value)| {
                    (key == "core.sshcommand").then(|| value.clone()).flatten()
                })
            })
            .or_else(|| {
                std::env::var_os("GIT_SSH").map(|path| {
                    // GIT_SSH is an executable path, not a shell command.
                    let mut quoted = vec![b'\''];
                    for byte in path.as_bytes() {
                        if *byte == b'\'' {
                            quoted.extend(b"'\\''");
                        } else {
                            quoted.push(*byte);
                        }
                    }
                    quoted.push(b'\'');
                    OsString::from_vec(quoted)
                })
            })
    }
    pub fn ssh_variant(&self) -> Option<OsString> {
        std::env::var_os("GIT_SSH_VARIANT").or_else(|| {
            self.config
                .iter()
                .rev()
                .find_map(|(key, value)| (key == "ssh.variant").then(|| value.clone()).flatten())
        })
    }
}

fn local_query(args: &[&OsStr]) -> bool {
    match args {
        // Resolving an index operand with rev-parse can invoke core.fsmonitor.
        [operation, option] if *operation == "rev-parse" => matches!(
            option.as_bytes(),
            b"--show-prefix" | b"--show-cdup" | b"--local-env-vars"
        ),
        // Later options can negate --get-url and turn it into a transport call.
        [operation, option, separator, _] => {
            *operation == "ls-remote" && *option == "--get-url" && *separator == "--"
        }
        _ => false,
    }
}
trait SplitByte {
    fn split_once_byte(&self, b: u8) -> (&[u8], Option<&[u8]>);
}
static CANCELLED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static CANCELLATION_HANDLERS: std::sync::OnceLock<std::io::Result<()>> = std::sync::OnceLock::new();
static OPERATIONS_STOPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// End owned native operations after the command finishes without inventing a
/// termination signal. The runner process handles one command invocation.
pub fn stop_operations() {
    OPERATIONS_STOPPED.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn operations_stopped() -> bool {
    OPERATIONS_STOPPED.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn cancelled_signal() -> Option<i32> {
    let signal = CANCELLED.load(std::sync::atomic::Ordering::Relaxed);
    (signal != 0).then_some(signal)
}
/// Stop owned native operations when the foreground command is interrupted.
pub fn cancel_by_signal(signal: i32) {
    if matches!(
        signal,
        libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
    ) {
        CANCELLED.store(signal, std::sync::atomic::Ordering::Relaxed);
    }
}
/// Register the cancellation latch before other subscribers can consume a
/// signal. Concurrent first callers wait for all four handlers, including a
/// registration failure, rather than observing partly initialized handlers.
pub fn initialize_cancellation() -> std::io::Result<()> {
    CANCELLATION_HANDLERS
        .get_or_init(|| {
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
                unsafe {
                    signal_hook::low_level::register(signal, move || {
                        CANCELLED.store(signal, std::sync::atomic::Ordering::Relaxed);
                    })?;
                }
            }
            Ok(())
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))
}
/// Wait for owned transport/verifier groups while draining their output and
/// servicing cancellation.
pub fn run(command: Command, capture: bool) -> Result<std::process::Output> {
    let managed = crate::terminal_ui::owns_terminal();
    let mut output = run_captured(
        command,
        capture || managed,
        capture || managed,
        None,
        None,
        false,
        CaptureMode::MayDetach,
    )?;
    if managed && !capture {
        use std::io::Write;
        let _drawing = crate::terminal_ui::native_terminal_lease(true);
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        output.stdout.clear();
        output.stderr.clear();
    }
    Ok(output)
}

/// Capture a Git builtin that does not invoke caller-configured helpers.
pub(crate) fn run_builtin(command: Command) -> Result<std::process::Output> {
    run_captured(command, true, true, None, None, false, CaptureMode::Channel)
}

pub(crate) fn run_ref_advertisement(mut command: Command) -> Result<std::process::Output> {
    // The capability check uses the packet trace. Native Git's listing of
    // every advertised ref has no consumer here.
    command.stdout(Stdio::null());
    run_captured(
        command,
        false,
        true,
        None,
        None,
        false,
        CaptureMode::MayDetach,
    )
}

pub(crate) fn run_fetch(
    command: Command,
    input: Option<&mut dyn std::io::Read>,
) -> Result<std::process::Output> {
    let progress = crate::terminal_ui::progress_enabled();
    let mut output = run_captured(
        command,
        true,
        true,
        None,
        input,
        true,
        CaptureMode::MayDetach,
    )?;
    if progress {
        output.stderr = crate::terminal_ui::progress_diagnostics(&output.stderr);
    }
    Ok(output)
}
/// Stream native stdout to its consumer while the caller retains process,
/// cancellation and terminal supervision. The consumer must read to EOF or fail;
/// its errors stop and reap the owned operation before they are returned.
pub(crate) fn run_with_output<T: Send>(
    mut command: Command,
    input: Option<&mut dyn std::io::Read>,
    consume: impl FnOnce(&mut dyn std::io::Read) -> Result<T> + Send,
) -> Result<(std::process::Output, T)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
    command.stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
        writer,
    )));
    let failed = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    reader.set_read_timeout(Some(std::time::Duration::from_millis(10)))?;
    std::thread::scope(|scope| {
        let transfer = std::thread::Builder::new()
            .name("git-output".into())
            .spawn_scoped(scope, || {
                let mut output = NativeOutput {
                    reader,
                    finished: &finished,
                    start_drain: Some(|input: &mut std::os::unix::net::UnixStream| {
                        input.set_nonblocking(true)
                    }),
                };
                let result = consume(&mut output);
                failed.store(result.is_err(), Ordering::Release);
                result
            })?;
        let result = run_captured(
            command,
            false,
            true,
            Some(&failed),
            input,
            false,
            CaptureMode::MayDetach,
        );
        finished.store(true, Ordering::Release);
        let value = transfer
            .join()
            .map_err(|_| anyhow::anyhow!("native output consumer panicked"))??;
        Ok((result?, value))
    })
}

pub(crate) fn run_to_writer<W: std::io::Write + Send>(
    command: Command,
    mut output: W,
    input: Option<&mut dyn std::io::Read>,
) -> Result<std::process::Output> {
    run_with_output(command, input, |reader| {
        let mut bytes = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut bytes)?;
            if count == 0 {
                return Ok(());
            }
            output.write_all(&bytes[..count])?;
        }
    })
    .map(|(result, ())| result)
}

struct NativeOutput<'a, R, F> {
    reader: R,
    finished: &'a std::sync::atomic::AtomicBool,
    start_drain: Option<F>,
}
impl<R: std::io::Read, F: FnOnce(&mut R) -> std::io::Result<()>> std::io::Read
    for NativeOutput<'_, R, F>
{
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // Preserve typed cancellation rather than reporting truncated Git
            // data after the supervisor closes an interrupted decoder's pipe.
            crate::object::check_transfer().map_err(std::io::Error::other)?;
            match self.reader.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.finished.load(std::sync::atomic::Ordering::Acquire) {
                        // A timeout can precede the final write/exit. Observe
                        // completion before freshly draining queued bytes.
                        match self.start_drain.take() {
                            Some(prepare) => prepare(&mut self.reader)?,
                            None => return Ok(0),
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

pub(crate) fn controlling_terminal() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

fn run_captured(
    mut command: Command,
    capture_stdout: bool,
    capture_stderr: bool,
    failed_output: Option<&std::sync::atomic::AtomicBool>,
    input: Option<&mut dyn std::io::Read>,
    progress: bool,
    mode: CaptureMode,
) -> Result<std::process::Output> {
    use std::{
        os::unix::{io::AsRawFd, process::CommandExt},
        time::Duration,
    };
    initialize_cancellation()?;
    if let Some(signal) = cancelled_signal() {
        return Err(crate::object::OperationCancelled)
            .context(format!("Git operation cancelled by signal {signal}"));
    }
    if operations_stopped() {
        return Err(crate::object::OperationCancelled)
            .context("Git operation stopped after command completion");
    }
    let mut input = input
        .map(|source| NativeInput::new(&mut command, source))
        .transpose()?;
    let mut stdout = if capture_stdout {
        let (capture, writer) = NativeCapture::new(mode)?;
        command.stdout(writer);
        Some(capture)
    } else {
        None
    };
    let mut stderr = if capture_stderr {
        let (capture, writer) = NativeCapture::new(mode)?;
        command.stderr(writer);
        Some(capture)
    } else {
        None
    };
    let mut progress = (progress && crate::terminal_ui::progress_enabled())
        .then(crate::terminal_ui::NativeProgress::new);
    let managed = crate::terminal_ui::owns_terminal();
    crate::terminal_ui::configure(&mut command);
    if managed && input.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        command.stdin(Stdio::null());
    }
    let tty = (!managed).then(controlling_terminal).flatten();
    let tty_fd = tty.as_ref().map(AsRawFd::as_raw_fd);
    let parent_group = unsafe { libc::getpgrp() };
    let transfer = tty_fd.is_some_and(|fd| unsafe { libc::tcgetpgrp(fd) == parent_group });
    let _progress_lease = crate::terminal_ui::native_terminal_lease(transfer);
    unsafe {
        command.pre_exec(move || {
            if if managed {
                libc::setsid()
            } else {
                libc::setpgid(0, 0)
            } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if transfer {
                set_foreground(tty_fd.unwrap(), libc::getpgrp())?;
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if transfer {
                let _ = set_foreground(tty_fd.unwrap(), parent_group);
            }
            return Err(error).context("start native Git operation");
        }
    };
    drop(command);
    let _operation = crate::terminal_ui::NativeOperation::new(child.id());
    let wakeup = ChildExitWakeup::new(child.id());
    let outcome = 'wait: loop {
        for capture in [&mut stdout, &mut stderr].into_iter().flatten() {
            if let Err(error) = capture.drain(64 * 1024) {
                break 'wait stop_request_operation(&mut child)
                    .and(Err(error).context("read native Git output"));
            }
        }
        if let (Some(progress), Some(NativeCapture::File(stderr))) = (&mut progress, &stderr) {
            progress.poll(stderr);
        }
        match crate::terminal_ui::try_wait(&mut child) {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error.into()),
            Ok(None) => {}
        }
        if failed_output.is_some_and(|failed| failed.load(std::sync::atomic::Ordering::Acquire)) {
            break stop_request_operation(&mut child)
                .and(Err(anyhow::anyhow!("native Git output consumer failed")));
        }
        if let Some(signal) = cancelled_signal() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = crate::terminal_ui::reap(&mut child);
            break Err(crate::object::OperationCancelled)
                .context(format!("Git operation cancelled by signal {signal}"));
        }
        if operations_stopped() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = crate::terminal_ui::reap(&mut child);
            break Err(crate::object::OperationCancelled)
                .context("Git operation stopped after command completion");
        }
        if let Some(stdin) = &mut input {
            match stdin.advance() {
                Ok(true) => input = None,
                Ok(false) => {}
                Err(error) => {
                    break stop_request_operation(&mut child)
                        .and(Err(error).context("write native Git input"));
                }
            }
        }
        if let Some(fd) = tty_fd {
            if let Err(error) = relay_native_job_control(child.id(), fd, parent_group) {
                break stop_request_operation(&mut child).and(Err(error));
            }
        }
        wakeup.wait(
            Duration::from_millis(10),
            input.as_ref().map(|input| input.pipe.as_raw_fd()),
            [
                stdout.as_ref().and_then(NativeCapture::fd),
                stderr.as_ref().and_then(NativeCapture::fd),
            ],
        );
    };
    drop(progress);
    // A background resume must leave the terminal with the invoking shell.
    if let Some(fd) = tty_fd {
        if unsafe { libc::tcgetpgrp(fd) } == child.id() as libc::pid_t {
            let _ = set_foreground(fd, parent_group);
        }
    }
    let status = outcome?;
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        // Terminal-generated signals go to the temporarily foreground Git
        // group, so the runner's own signal handler may never receive them.
        cancel_by_signal(signal);
    }
    Ok(std::process::Output {
        status,
        stdout: stdout
            .map(NativeCapture::finish)
            .transpose()?
            .unwrap_or_default(),
        stderr: stderr
            .map(NativeCapture::finish)
            .transpose()?
            .unwrap_or_default(),
    })
}

#[derive(Clone, Copy)]
enum CaptureMode {
    Channel,
    MayDetach,
}

enum NativeCapture {
    Channel {
        pipe: Option<std::os::unix::net::UnixStream>,
        bytes: Vec<u8>,
    },
    File(std::fs::File),
}

pub(crate) fn capture_file() -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;
        // Some configured helpers keep writing after Git exits. A memfd
        // preserves a regular file's lifetime and seeking without disk I/O.
        let fd = unsafe { libc::memfd_create(c"git-on-output".as_ptr(), libc::MFD_CLOEXEC) };
        if fd >= 0 {
            return Ok(unsafe { std::fs::File::from_raw_fd(fd) });
        }
    }
    tempfile::tempfile()
}

impl NativeCapture {
    fn new(mode: CaptureMode) -> std::io::Result<(Self, Stdio)> {
        if matches!(mode, CaptureMode::MayDetach) {
            // Native Git can leave a configured hook or a persistent SSH
            // proxy writing to its inherited descriptors after Git exits.
            // A capture channel would return EPIPE when its reader closes.
            let file = capture_file()?;
            let writer = file.try_clone()?.into();
            return Ok((Self::File(file), writer));
        }
        let (pipe, child) = std::os::unix::net::UnixStream::pair()?;
        pipe.set_nonblocking(true)?;
        Ok((
            Self::Channel {
                pipe: Some(pipe),
                bytes: Vec::new(),
            },
            Stdio::from(std::os::fd::OwnedFd::from(child)),
        ))
    }

    fn fd(&self) -> Option<i32> {
        match self {
            Self::Channel { pipe, .. } => pipe.as_ref().map(std::os::fd::AsRawFd::as_raw_fd),
            Self::File(_) => None,
        }
    }

    fn drain(&mut self, limit: u64) -> std::io::Result<()> {
        use std::io::{ErrorKind, Read};

        if let Self::Channel { pipe, bytes } = self {
            if let Some(reader) = pipe.as_ref() {
                match reader.take(limit).read_to_end(bytes) {
                    Ok(count) if (count as u64) < limit => *pipe = None,
                    Ok(_) => (),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => (),
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<Vec<u8>> {
        if let Some(fd) = self.fd() {
            // Read bytes queued between the last drain and the child's exit.
            let mut available: libc::c_int = 0;
            if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            self.drain(available as u64)?;
        }
        match self {
            Self::Channel { bytes, .. } => Ok(bytes),
            Self::File(mut file) => {
                use std::io::{Read, Seek};
                let mut bytes = Vec::new();
                file.rewind()?;
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            }
        }
    }
}

struct NativeInput<'a> {
    source: &'a mut dyn std::io::Read,
    pipe: std::os::unix::net::UnixStream,
    bytes: [u8; 8192],
    pending: std::ops::Range<usize>,
}
impl<'a> NativeInput<'a> {
    fn new(command: &mut Command, source: &'a mut dyn std::io::Read) -> std::io::Result<Self> {
        use std::os::fd::OwnedFd;
        let (pipe, child) = std::os::unix::net::UnixStream::pair()?;
        pipe.set_nonblocking(true)?;
        command.stdin(Stdio::from(OwnedFd::from(child)));
        Ok(Self {
            source,
            pipe,
            bytes: [0; 8192],
            pending: 0..0,
        })
    }
    fn advance(&mut self) -> std::io::Result<bool> {
        use std::io::{ErrorKind, Write};
        // Bound work between cancellation checks, including for a decoder's
        // large segmented request stream. No input worker or full copy is needed.
        for _ in 0..8 {
            if self.pending.is_empty() {
                let count = match self.source.read(&mut self.bytes) {
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    value => value?,
                };
                if count == 0 {
                    return Ok(true);
                }
                self.pending = 0..count;
            }
            match self.pipe.write(&self.bytes[self.pending.clone()]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(count) => self.pending.start += count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
                // A helper can reject a request before consuming stdin. Let
                // supervision collect its exit status and diagnostic output.
                Err(error) if error.kind() == ErrorKind::BrokenPipe => return Ok(true),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
}
/// Exit notification removes the artificial polling floor for short native
/// operations. Cancellation/TTY checks keep the same maximum polling interval.
/// Unsupported kernels or descriptor pressure retain the existing fallback.
/// Pollable child exit notification for native helpers.
pub struct ChildExitWakeup {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fd: Option<std::os::fd::OwnedFd>,
}
impl ChildExitWakeup {
    pub fn new(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
            Self {
                fd: (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
            }
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::FromRawFd;

            // Darwin creates the descriptor close-on-exec. Poll wakes when
            // the notification arrives; the existing try_wait reaps the child.
            let queue = unsafe { libc::kqueue() };
            if queue < 0 {
                return Self { fd: None };
            }
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(queue) };
            let exit = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let registered =
                unsafe { libc::kevent(queue, &exit, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            Self {
                fd: (registered == 0).then_some(fd),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            Self {}
        }
    }
    pub fn wait(
        &self,
        duration: std::time::Duration,
        input: Option<i32>,
        output: [Option<i32>; 2],
    ) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let exit = self.fd.as_ref().map_or(-1, std::os::fd::AsRawFd::as_raw_fd);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let exit = -1;
        let mut descriptors = [
            (exit, libc::POLLIN),
            (input.unwrap_or(-1), libc::POLLOUT),
            (output[0].unwrap_or(-1), libc::POLLIN),
            (output[1].unwrap_or(-1), libc::POLLIN),
        ]
        .map(|(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        });
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                duration.as_millis() as i32,
            )
        };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            std::thread::sleep(duration);
        }
    }
}
/// Native Git owns a separate group while authenticating/verifying. Observe
/// stops without consuming its exit status, then suspend the caller's job so
/// its shell can report the stop and perform ordinary `fg`/`bg` transitions.
fn relay_native_job_control(pid: u32, fd: i32, parent_group: libc::pid_t) -> Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result =
        unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WSTOPPED | libc::WNOHANG) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        // WSTOPPED alone can report an exited child as ECHILD. Child::try_wait
        // remains the sole owner of the exit status on the next iteration.
        if error.raw_os_error() == Some(libc::ECHILD)
            || error.kind() == std::io::ErrorKind::Interrupted
        {
            return Ok(());
        }
        return Err(error.into());
    }
    let group = pid as libc::pid_t;
    let foreground = unsafe { libc::tcgetpgrp(fd) };
    if foreground == parent_group {
        // `fg` can foreground an already-running background job without
        // sending SIGCONT. Pass ownership on during the normal polling loop.
        set_foreground(fd, group)?;
    }
    if crate::platform::siginfo_pid(&info) == 0 {
        return Ok(());
    }
    let stop = crate::platform::siginfo_status(&info);
    if foreground == parent_group && matches!(stop, libc::SIGTTIN | libc::SIGTTOU) {
        // A pending background-I/O stop raced the shell's `fg` transition.
        unsafe { libc::kill(-group, libc::SIGCONT) };
        return Ok(());
    }
    if unsafe { libc::tcgetpgrp(fd) } == group {
        set_foreground(fd, parent_group)?;
    }
    unsafe {
        libc::kill(-parent_group, libc::SIGSTOP);
    }
    // Only `fg` grants the caller's job the terminal. A `bg` resume leaves
    // it with the shell, so another terminal read can stop the job normally.
    if unsafe { libc::tcgetpgrp(fd) } == parent_group {
        set_foreground(fd, group)?;
    }
    unsafe { libc::kill(-group, libc::SIGCONT) };
    Ok(())
}
fn stop_request_operation(child: &mut std::process::Child) -> Result<()> {
    use std::time::{Duration, Instant};
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    // Let native Git roll back its locks. WNOWAIT preserves leader ownership
    // until every remaining group member has been killed, preventing PID reuse.
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            break;
        }
        if (result == 0 && crate::platform::siginfo_pid(&info) != 0) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    crate::terminal_ui::reap(child).context("reap interrupted native Git operation")?;
    Ok(())
}
/// Transfer terminal ownership while blocking SIGTTOU in the calling thread.
pub fn set_foreground(fd: i32, group: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let result = libc::tcsetpgrp(fd, group);
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if result != 0 {
            return Err(error);
        }
        Ok(())
    }
}
impl SplitByte for [u8] {
    fn split_once_byte(&self, b: u8) -> (&[u8], Option<&[u8]>) {
        match self.iter().position(|x| *x == b) {
            Some(n) => (&self[..n], Some(&self[n + 1..])),
            None => (self, None),
        }
    }
}

#[cfg(test)]
mod transport_policy_tests {
    use super::*;

    #[test]
    fn url_query_channels_require_the_exact_local_command() {
        let local = ["ls-remote", "--get-url", "--", "source"].map(OsStr::new);
        let remote = ["ls-remote", "--get-url", "--no-get-url", "source"].map(OsStr::new);
        assert!(local_query(&local));
        assert!(!local_query(&remote));
    }

    #[test]
    fn revision_resolution_preserves_configured_fsmonitor_output() {
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };

        const FIXTURE_ENV: &str = "GIT_ON_TEST_FSMONITOR_FIXTURE";
        if std::env::var_os(FIXTURE_ENV).is_none() {
            let mut test = Command::new(std::env::current_exe().unwrap());
            for (key, _) in std::env::vars_os() {
                if key.as_bytes().starts_with(b"GIT_") {
                    test.env_remove(key);
                }
            }
            let result = test
                .env(FIXTURE_ENV, "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["--exact", "git::transport_policy_tests::revision_resolution_preserves_configured_fsmonitor_output"])
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

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let repo = git2::Repository::init(root).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Git-on tests", "test@example.invalid").unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        assert!(Command::new("git")
            .current_dir(root)
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{commit},view")
            ])
            .status()
            .unwrap()
            .success());

        let hook = root.join("fsmonitor");
        std::fs::write(
            &hook,
            br#"#!/bin/sh
(
    trap '' PIPE
    attempts=0
    while ! test -e helper-ready; do
        test "$attempts" -lt 300 || exit 1
        attempts=$((attempts + 1))
        sleep .01
    done
    if printf 'late stderr\n' >&2; then printf y; else printf n; fi >helper-done
) >/dev/null &
printf 'clock\000'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.fsmonitor", hook.to_str().unwrap())
            .unwrap();
        let selected = Git::capture(root).unwrap().resolve(OsStr::new(":view"));
        std::fs::write(root.join("helper-ready"), b"").unwrap();
        assert_eq!(selected.unwrap(), commit);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let outcome = std::fs::read(root.join("helper-done")).ok();
            if let Some(outcome) = outcome.filter(|bytes| !bytes.is_empty()) {
                assert_eq!(outcome, b"y");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fsmonitor helper did not write its result"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_capture_keeps_exit_status_after_child_closes_input() {
        use std::{
            io::{Read, Write},
            os::{fd::OwnedFd, unix::net::UnixStream},
            time::Duration,
        };

        let (sync, child_sync) = UnixStream::pair().unwrap();
        sync.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "read -r ready <&1; printf 'cannot read request\\n' >&2; exec 0<&-; printf x; exit 42",
            ])
            .stdout(Stdio::from(OwnedFd::from(child_sync)));
        struct RequestAfterInputCloses(Option<UnixStream>);
        impl Read for RequestAfterInputCloses {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let Some(mut sync) = self.0.take() else {
                    return Ok(0);
                };
                sync.write_all(b"ready\n")?;
                sync.read_exact(&mut [0])?;
                bytes[0] = b'x';
                Ok(1)
            }
        }
        let output = run_captured(
            command,
            false,
            true,
            None,
            Some(&mut RequestAfterInputCloses(Some(sync))),
            false,
            CaptureMode::Channel,
        )
        .expect("preserve the outcome of a child that closes stdin");
        assert_eq!(output.status.code(), Some(42));
        assert_eq!(output.stderr, b"cannot read request\n");
    }

    #[test]
    fn native_capture_drains_both_streams_without_files() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat >&2",
        ]);
        unsafe {
            command.pre_exec(|| {
                for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
                    if libc::fstat(fd, metadata.as_mut_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if metadata.assume_init().st_mode & libc::S_IFMT == libc::S_IFREG {
                        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                    }
                }
                Ok(())
            });
        }
        let bytes = vec![b'x'; 1024 * 1024];
        let output = run_captured(
            command,
            true,
            true,
            None,
            Some(&mut bytes.as_slice()),
            false,
            CaptureMode::Channel,
        )
        .expect("capture Git output without regular files");
        assert!(output.status.success());
        assert_eq!(output.stdout, vec![0; 4 * 65536]);
        assert_eq!(output.stderr, bytes);
    }

    #[test]
    fn explicit_transport_policy_keeps_http2_on_native_git() {
        for key in ["protocol.allow", "protocol.https.allow"] {
            let git = Git {
                cwd: PathBuf::new(),
                config: vec![(key.into(), Some("never".into()))],
                original_dir: None,
            };
            assert!(!git.http2_compatible(), "{key}");
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn darwin_child_exit_notifies_the_wait_loop() {
    use std::os::fd::AsRawFd;

    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let wakeup = ChildExitWakeup::new(child.id());
    drop(child.stdin.take());
    let mut event = libc::pollfd {
        fd: wakeup.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd),
        events: libc::POLLIN,
        revents: 0,
    };
    let notified = event.fd >= 0 && unsafe { libc::poll(&mut event, 1, 5000) } == 1;
    assert!(child.wait().unwrap().success());
    assert!(notified && event.revents & libc::POLLIN != 0);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn native_input_and_output_progress_with_pipe_backpressure() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat"]);
        let bytes = vec![b'x'; 1024 * 1024];
        let mut input = bytes.as_slice();
        let mut output = Vec::new();
        let result = run_to_writer(command, &mut output, Some(&mut input)).unwrap();
        assert!(result.status.success());
        assert_eq!(&output[..262144], vec![0; 262144]);
        assert_eq!(&output[262144..], bytes);
    }

    #[test]
    fn native_output_writer_error_stops_the_owned_operation() {
        struct Failed;
        impl std::io::Write for Failed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("not-stopped");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf payload; sleep 3; printf late > \"$1\"",
                "fixture",
            ])
            .arg(&marker);
        let error = run_to_writer(command, Failed, None).unwrap_err();
        assert!(error.to_string().contains("fixture output failure"));
        assert!(
            !marker.exists(),
            "native operation continued after output failed"
        );
    }

    #[test]
    fn streamed_output_retains_the_native_exit_status() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf payload; exit 42"]);
        let (result, bytes) = run_with_output(command, None, |reader| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(bytes, b"payload");
        assert_eq!(result.status.code(), Some(42));
    }

    fn wait_for_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|pid| pid.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "owned operation never started");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn interrupted_owned_group_stops_even_with_ignored_term_or_early_leader_exit() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        for leader_exits in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let started = temporary.path().join("stubborn-pid");
            let script = if leader_exits {
                "sh -c 'trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300' stubborn \"$1\" & wait"
            } else {
                "trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300"
            };
            let mut child = Command::new("sh")
                .args(["-c", script, "owned-group-test"])
                .arg(&started)
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = wait_for_pid(&started);
            stop_request_operation(&mut child).unwrap();
            assert_eq!(
                child.wait().unwrap().signal(),
                Some(if leader_exits {
                    libc::SIGTERM
                } else {
                    libc::SIGKILL
                })
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
                if state.is_err() || state.unwrap().split_whitespace().nth(2) == Some("Z") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "owned descendant survived SIGKILL"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod output_race_tests {
    use super::*;
    use std::{
        io::{self, Read, Write},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };
    struct StaleTimeout<'a> {
        input: UnixStream,
        writer: UnixStream,
        finished: &'a AtomicBool,
        raced: bool,
    }
    impl Read for StaleTimeout<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if !self.raced {
                self.raced = true;
                let stale = self.input.read(output);
                assert!(
                    matches!(&stale, Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
                );
                // A native write and completed-child notification race a read
                // that has already returned a timeout to the transfer thread.
                self.writer
                    .write_all(b"verified output queued before exit")?;
                self.finished.store(true, Ordering::Release);
                return stale;
            }
            self.input.read(output)
        }
    }
    fn input(finished: &AtomicBool) -> StaleTimeout<'_> {
        let (input, writer) = UnixStream::pair().unwrap();
        input
            .set_read_timeout(Some(std::time::Duration::from_millis(1)))
            .unwrap();
        StaleTimeout {
            input,
            writer,
            finished,
            raced: false,
        }
    }
    #[test]
    fn finished_output_is_drained_after_a_stale_timeout() {
        let finished = AtomicBool::new(false);
        let mut input = input(&finished);
        let mut output = Vec::new();
        let mut reader = NativeOutput {
            reader: &mut input,
            finished: &finished,
            start_drain: Some(|input: &mut &mut StaleTimeout<'_>| {
                input.input.set_nonblocking(true)
            }),
        };
        io::copy(&mut reader, &mut output).unwrap();
        assert_eq!(output, b"verified output queued before exit");
    }
    #[test]
    fn final_output_drain_keeps_staging_failures_visible() {
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("staging failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let finished = AtomicBool::new(false);
        let mut input = input(&finished);
        let mut reader = NativeOutput {
            reader: &mut input,
            finished: &finished,
            start_drain: Some(|input: &mut &mut StaleTimeout<'_>| {
                input.input.set_nonblocking(true)
            }),
        };
        assert!(io::copy(&mut reader, &mut Failed).is_err());
    }
}
