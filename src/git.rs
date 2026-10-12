//! Native Git configuration and signature policy. No replacement objects.
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    cell::RefCell,
    ffi::{OsStr, OsString},
    fs::File,
    marker::PhantomData,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
};

#[derive(Debug)]
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
        let output = run(command, true).context("read effective Git configuration")?;
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
                run(command, true)
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
                    // Remote::open pins the already rewritten transport URL.
                    if key.as_bytes().starts_with(b"url.")
                        && key.as_bytes().ends_with(b".insteadof")
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
            }
            command
                .arg("--git-dir")
                .arg(dir)
                .args(["-c", "core.bare=true"]);
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
    }
    pub fn output(&self, dir: Option<&Path>, args: &[&OsStr]) -> Result<Vec<u8>> {
        let mut command = self.command(dir);
        command.args(args).stdin(Stdio::null());
        let out = run(command, true)?;
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
    pub fn pin_remote_url(&mut self, name: &OsStr, url: &OsStr) {
        let mut key = OsString::from("remote.");
        key.push(name);
        key.push(".url");
        self.config.retain(|(existing, _)| existing != &key);
        self.config.push((key, Some(url.to_os_string())));
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
/// Wait for owned transport/verifier groups without trapping cancellation in
/// blocking pipe reads. Temporary capture files have no application quota.
pub fn run(mut command: Command, capture: bool) -> Result<std::process::Output> {
    use std::{
        io::{Read, Seek},
        os::unix::{io::AsRawFd, process::CommandExt},
        time::Duration,
    };
    initialize_cancellation()?;
    if let Some(signal) = cancelled_signal() {
        bail!("Git operation cancelled by signal {signal}");
    }
    if operations_stopped() {
        bail!("Git operation stopped after command completion");
    }
    check_request_interruption()?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    if capture {
        command
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?);
    }
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok();
    let tty_fd = tty.as_ref().map(AsRawFd::as_raw_fd);
    let parent_group = unsafe { libc::getpgrp() };
    let transfer = tty_fd.is_some_and(|fd| unsafe { libc::tcgetpgrp(fd) == parent_group });
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
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
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error.into()),
            Ok(None) => {}
        }
        if let Some(signal) = cancelled_signal() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            break Err(anyhow::anyhow!(
                "Git operation cancelled by signal {signal}"
            ));
        }
        if operations_stopped() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            break Err(anyhow::anyhow!(
                "Git operation stopped after command completion"
            ));
        }
        if let Err(error) = check_request_interruption() {
            break stop_request_operation(&mut child).and(Err(error));
        }
        if let Some(fd) = tty_fd {
            if let Err(error) = relay_native_job_control(child.id(), fd, parent_group) {
                break stop_request_operation(&mut child).and(Err(error));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
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
    let mut out = Vec::new();
    let mut err = Vec::new();
    if capture {
        stdout.rewind()?;
        stderr.rewind()?;
        stdout.read_to_end(&mut out)?;
        stderr.read_to_end(&mut err)?;
    }
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
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
    if unsafe { info.si_pid() } == 0 {
        return Ok(());
    }
    let stop = unsafe { info.si_status() };
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
        if (result == 0 && unsafe { info.si_pid() } != 0) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    child
        .wait()
        .context("reap interrupted native Git operation")?;
    Ok(())
}
fn set_foreground(fd: i32, group: libc::pid_t) -> std::io::Result<()> {
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

#[derive(Debug)]
struct RequestInterrupted(i32);
impl std::fmt::Display for RequestInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "filesystem requester interrupted by signal {}", self.0)
    }
}
impl std::error::Error for RequestInterrupted {}

thread_local! {
    static REQUEST_STATUS: RefCell<Option<File>> = const { RefCell::new(None) };
}

/// Bind a synchronous FUSE callback to the actual requesting task. Keeping the
/// proc descriptor open prevents a reused task ID from changing this identity.
pub(crate) struct RequestGuard {
    previous: Option<File>,
    _same_thread: PhantomData<Rc<()>>,
}
impl RequestGuard {
    pub(crate) fn enter(pid: u32) -> Self {
        let status = File::open(format!("/proc/{pid}/status")).ok();
        Self {
            previous: REQUEST_STATUS.with(|slot| slot.replace(status)),
            _same_thread: PhantomData,
        }
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        REQUEST_STATUS.with(|slot| slot.replace(self.previous.take()));
    }
}

fn pending_request_signal(status: &[u8]) -> Option<i32> {
    let mut pending = None;
    let mut shared = None;
    let mut blocked = None;
    let mut ignored = None;
    for line in status.split(|byte| *byte == b'\n') {
        let (key, value) = line.split_once_byte(b':');
        let Some(value) = value else {
            continue;
        };
        let destination = match key {
            b"SigPnd" => &mut pending,
            b"ShdPnd" => &mut shared,
            b"SigBlk" => &mut blocked,
            b"SigIgn" => &mut ignored,
            _ => continue,
        };
        *destination = std::str::from_utf8(value)
            .ok()
            .and_then(|value| u64::from_str_radix(value.trim(), 16).ok());
    }
    let actionable = (pending? | shared?) & !blocked? & !ignored?;
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .find(|signal| actionable & (1_u64 << (signal - 1)) != 0)
}

/// fuser dispatches synchronously and cannot process INTERRUPT while a native
/// operation is blocked. Reply EINTR when this requester has a pending signal,
/// then let its actual disposition/handler determine the command's outcome.
/// This is per-request interruption, never global command cancellation.
pub(crate) fn check_request_interruption() -> Result<()> {
    use std::io::{Read, Seek};
    let signal = REQUEST_STATUS.with(|slot| {
        let mut slot = slot.borrow_mut();
        let status = slot.as_mut()?;
        status.rewind().ok()?;
        let mut bytes = Vec::new();
        status.read_to_end(&mut bytes).ok()?;
        pending_request_signal(&bytes)
    });
    if let Some(signal) = signal {
        return Err(RequestInterrupted(signal).into());
    }
    Ok(())
}
pub(crate) fn is_request_interruption(error: &anyhow::Error) -> bool {
    error.is::<RequestInterrupted>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::Child;
    use std::time::{Duration, Instant};

    struct Requester(Child);
    impl Requester {
        fn stopped(mode: &str) -> Self {
            let script = "import ctypes,os,signal,sys,time\nmode=sys.argv[1]\nif mode=='caught': signal.signal(signal.SIGTERM,lambda *_:None)\nif mode=='ignored': signal.signal(signal.SIGTERM,signal.SIG_IGN)\nif mode=='blocked': signal.pthread_sigmask(signal.SIG_BLOCK,{signal.SIGTERM})\nctypes.CDLL(None).prctl(15,b'\\xffrequester',0,0,0)\nprint('ready',flush=True)\nos.kill(os.getpid(),signal.SIGSTOP)\ntime.sleep(300)";
            let mut child = Command::new("python3")
                .args(["-c", script, mode])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready.trim(), "ready");
            let requester = Self(child);
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let status = std::fs::read(format!("/proc/{}/status", requester.0.id())).unwrap();
                if status
                    .split(|byte| *byte == b'\n')
                    .any(|line| line.starts_with(b"State:\tT"))
                {
                    return requester;
                }
                assert!(Instant::now() < deadline, "requester never stopped");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn signal(&self, signal: i32) {
            assert_eq!(unsafe { libc::kill(self.0.id() as i32, signal) }, 0);
        }
    }
    impl Drop for Requester {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
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
    fn request_masks_preserve_blocked_ignored_and_caught_dispositions() {
        let status = |pending, shared, blocked, ignored| {
            format!("SigPnd:\t{pending:016x}\nShdPnd:\t{shared:016x}\nSigBlk:\t{blocked:016x}\nSigIgn:\t{ignored:016x}\nSigCgt:\tffffffffffffffff\n")
        };
        let term = 1_u64 << (libc::SIGTERM - 1);
        for (pending, shared) in [(term, 0), (0, term)] {
            assert_eq!(
                pending_request_signal(status(pending, shared, 0, 0).as_bytes()),
                Some(libc::SIGTERM)
            );
            for (blocked, ignored) in [(term, 0), (0, term)] {
                assert_eq!(
                    pending_request_signal(status(pending, shared, blocked, ignored).as_bytes()),
                    None
                );
            }
        }
        assert_eq!(pending_request_signal(b"SigPnd:\t4000\n"), None);
        let mut long_status = b"Name:\t\xffrequester\nGroups:\t".to_vec();
        long_status.extend(b"10000 ".repeat(4096));
        long_status.extend_from_slice(b"\n");
        long_status.extend_from_slice(status(0, term, 0, 0).as_bytes());
        assert_eq!(pending_request_signal(&long_status), Some(libc::SIGTERM));
        {
            use std::io::Write;
            let _request = RequestGuard::enter(0);
            let mut held = tempfile::tempfile().unwrap();
            held.write_all(&long_status).unwrap();
            REQUEST_STATUS.with(|slot| slot.replace(Some(held)));
            assert!(is_request_interruption(
                &check_request_interruption().unwrap_err()
            ));
        }
        let user = 1_u64 << (libc::SIGUSR1 - 1);
        assert_eq!(
            pending_request_signal(status(0, user, 0, 0).as_bytes()),
            None
        );
    }

    #[test]
    fn request_scope_reads_real_dispositions_and_keeps_original_task_identity() {
        for (mode, signal, interrupts) in [
            ("default", libc::SIGTERM, true),
            ("caught", libc::SIGTERM, true),
            ("python-int", libc::SIGINT, true),
            ("ignored", libc::SIGTERM, false),
            ("blocked", libc::SIGTERM, false),
        ] {
            let requester = Requester::stopped(mode);
            let _request = RequestGuard::enter(requester.0.id());
            requester.signal(signal);
            match check_request_interruption() {
                Err(error) => assert!(interrupts && is_request_interruption(&error)),
                Ok(()) => assert!(!interrupts, "missed pending {signal} in {mode}"),
            }
            {
                let _nested = RequestGuard::enter(0);
                check_request_interruption().unwrap();
            }
            assert_eq!(check_request_interruption().is_err(), interrupts);
            // The held proc descriptor belongs to this task even after exit;
            // later checks never reopen its numeric ID or cancel another task.
            drop(requester);
            check_request_interruption().unwrap();
        }
        check_request_interruption().unwrap();
    }

    #[test]
    fn pending_request_signal_stops_only_its_owned_operation() {
        let requester = Requester::stopped("caught");
        let _request = RequestGuard::enter(requester.0.id());
        let temporary = tempfile::tempdir().unwrap();
        let started = temporary.path().join("native-pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 300", "request-test"])
            .arg(&started);
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                wait_for_pid(&started);
                requester.signal(libc::SIGTERM);
            });
            run(command, true).unwrap_err()
        });
        assert!(is_request_interruption(&error));
        let pid = std::fs::read_to_string(&started).unwrap();
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists());
        assert_eq!(cancelled_signal(), None);
        assert!(!operations_stopped());
        let status = std::fs::read(format!("/proc/{}/status", requester.0.id())).unwrap();
        assert!(status
            .split(|byte| *byte == b'\n')
            .any(|line| line.starts_with(b"State:\tT")));
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
