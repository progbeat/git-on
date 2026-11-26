use anyhow::{bail, Context, Result};
use git_on::{
    backend::Backend,
    filesystem::Overlay,
    git::set_foreground as foreground,
    platform::{child_exited, mount_is_ours, remove_empty_temporary_group, ssh_control_path},
    policy::WritePolicy,
    snapshot::{Preparation, Snapshot},
};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};
use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    os::unix::{
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

#[cfg(feature = "test-metrics")]
use std::{io::Write, os::fd::FromRawFd};

mod cli;
mod run_directory;

use run_directory::{private_dir, LockedRun, PendingGroupCleanup};

fn kill_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}
struct Terminal {
    tty: Option<File>,
    parent: Option<libc::pid_t>,
    child: Option<libc::pid_t>,
}
impl Terminal {
    fn capture() -> Self {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok();
        let parent = tty.as_ref().and_then(|f| {
            (unsafe { libc::tcgetpgrp(f.as_raw_fd()) } >= 0).then_some(unsafe { libc::getpgrp() })
        });
        Self {
            tty,
            parent,
            child: None,
        }
    }
    fn descriptor(&self) -> Option<i32> {
        self.tty.as_ref().map(AsRawFd::as_raw_fd)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let (Some(pgid), Some(fd)) = (self.parent, self.descriptor()) {
            let current = unsafe { libc::tcgetpgrp(fd) };
            if self.child == Some(current) {
                let _ = foreground(fd, pgid);
            }
        }
    }
}
fn clean_unmount(mountpoint: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    let unmount = {
        let mut c = Command::new("fusermount3");
        c.args(["-u", "--"]);
        c
    };
    #[cfg(target_os = "macos")]
    let unmount = Command::new("/sbin/umount");
    clean_unmount_with(unmount, mountpoint)
}

#[cfg(target_os = "macos")]
fn unmount_dead_server(mountpoint: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    // A normal unmount asks the server to flush its data. Once the embedded
    // server is gone, a hard mount can wait indefinitely for that response.
    // The caller has already checked the exact mount's source and location.
    let target = std::ffi::CString::new(mountpoint.as_os_str().as_bytes())?;
    if unsafe { libc::unmount(target.as_ptr(), libc::MNT_FORCE) } != 0 {
        return Err(std::io::Error::last_os_error()).context("unmount after NFS server exit");
    }
    Ok(())
}
struct UnblockedChildExit(libc::sigset_t);
impl UnblockedChildExit {
    fn new() -> std::io::Result<Self> {
        unsafe {
            let mut set = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
            let error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, &mut old);
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            Ok(Self(old))
        }
    }
}
impl Drop for UnblockedChildExit {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
        }
    }
}
fn clean_unmount_with(mut unmount: Command, mountpoint: &Path) -> Result<()> {
    git_on::git::initialize_cancellation()?;
    // Observe signals received during cleanup, rather than a cancellation that
    // already stopped the command. The first cancellation still gets a normal
    // unmount attempt; a subsequent signal can stop a hung helper.
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT, libc::SIGCHLD])?;
    // A launcher may block SIGCHLD across exec. Our event wait needs it
    // deliverable on this thread; restore the caller's mask on every return.
    let _child_exit = UnblockedChildExit::new().context("unblock unmount exit notification")?;
    unmount
        .arg(mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        unmount.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start = Instant::now();
    loop {
        // A successful clean unmount may reclaim every visited inode. Its
        // duration grows with the actual traversal; elapsed time alone cannot
        // distinguish that work from a busy mount. Wait for the helper's real
        // result, and bound only retries of completed failures below.
        let mut child = unmount.spawn().context("start clean unmount")?;
        let mut pending = signals.pending();
        let status = loop {
            if let Some(signal) =
                pending.find(|s| matches!(*s, SIGINT | SIGTERM | SIGHUP | SIGQUIT))
            {
                git_on::git::cancel_by_signal(signal);
                kill_group(child.id(), libc::SIGKILL);
                child.wait().context("reap cancelled unmount helper")?;
                bail!("clean unmount cancelled by signal {signal}");
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => pending = signals.wait(),
                Err(error) => {
                    kill_group(child.id(), libc::SIGKILL);
                    let _ = child.wait();
                    return Err(error).context("wait for clean unmount");
                }
            }
        };
        if status.success() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            bail!(
                "mount is busy or unmount failed at {}",
                mountpoint.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
struct Mounted {
    session: Option<git_on::backend::Session>,
    mountpoint: PathBuf,
    id: String,
}
impl Mounted {
    fn failed(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(git_on::backend::Session::failed)
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            #[cfg(target_os = "macos")]
            let git_on::backend::Session::Macos(nfs) = &session;
            let result = mount_is_ours(
                self.mountpoint.parent().unwrap(),
                &self.id,
                #[cfg(target_os = "macos")]
                Some(nfs.socket_path()),
            )
            .and_then(|mounted| {
                if mounted {
                    #[cfg(target_os = "macos")]
                    if session.failed() {
                        return unmount_dead_server(&self.mountpoint);
                    }
                    clean_unmount(&self.mountpoint)
                } else {
                    Ok(())
                }
            });
            #[cfg(target_os = "linux")]
            if let Err(error) = result {
                // Do not invoke fuser's lazy-unmount fallback on a busy mount.
                // Process exit closes the FUSE connection; the persistent pin
                // and run artifacts allow explicit, checked stale cleanup.
                std::mem::forget(session);
                return Err(error);
            }
            let joined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.join()))
                .map_err(|_| anyhow::anyhow!("filesystem thread panicked"))
                .and_then(|result| result.context("stop filesystem server"));
            #[cfg(target_os = "macos")]
            if let Err(error) = result {
                return match joined {
                    Ok(()) => Err(error),
                    Err(join_error) => Err(anyhow::anyhow!("{error:#}; {join_error:#}")),
                };
            }
            joined?;
        }
        Ok(())
    }
}
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
#[cfg(target_os = "macos")]
fn attach_nfs(
    mounted: &Mounted,
    policy: WritePolicy,
    signals: &mut Signals,
    _socket: &Path,
) -> Result<()> {
    use std::{
        io::{ErrorKind, Read},
        os::{fd::OwnedFd, unix::net::UnixStream},
    };
    #[cfg(feature = "test-metrics")]
    if let Some(path) = env::var_os("GIT_ON_TEST_NFS_MOUNT_ERROR").filter(|path| !path.is_empty()) {
        if path != "1" {
            fs::write(path, _socket.as_os_str().as_encoded_bytes())?;
        }
        bail!("test NFS mount failure");
    }
    let (mut diagnostic, writer) = UnixStream::pair()?;
    diagnostic.set_nonblocking(true)?;
    let mut message = Vec::new();
    let fd = diagnostic.as_raw_fd();
    let mut drain = || -> std::io::Result<bool> {
        let mut buffer = [0; 4096];
        for _ in 0..16 {
            match diagnostic.read(&mut buffer) {
                Ok(0) => return Ok(false),
                Ok(count) => message.extend_from_slice(&buffer[..count.min(4096 - message.len())]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(true),
                Err(error) if error.kind() == ErrorKind::Interrupted => (),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    };
    let Some(git_on::backend::Session::Macos(session)) = mounted.session.as_ref() else {
        bail!("NFS server is unavailable");
    };
    let mut helper = session
        .mount_command(&mounted.mountpoint, policy)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(OwnedFd::from(writer)))
        .spawn()
        .context("start mount_nfs")?;
    let wakeup = git_on::git::ChildExitWakeup::new(helper.id());
    let mut diagnostic_open = true;
    let status = loop {
        if git_on::git::cancelled_signal().is_some() || mounted.failed() {
            kill_group(helper.id(), libc::SIGKILL);
            helper.wait().context("reap mount_nfs")?;
            bail!("NFS mount interrupted or server stopped");
        }
        match helper.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if diagnostic_open {
                    match drain() {
                        Ok(open) => diagnostic_open = open,
                        Err(error) => {
                            kill_group(helper.id(), libc::SIGKILL);
                            let _ = helper.wait();
                            return Err(error).context("read mount_nfs diagnostics");
                        }
                    }
                }
                signals.pending().for_each(drop);
                wakeup.wait(
                    Duration::from_millis(25),
                    None,
                    [diagnostic_open.then_some(fd), None],
                );
            }
            Err(error) => {
                kill_group(helper.id(), libc::SIGKILL);
                let _ = helper.wait();
                return Err(error).context("wait for mount_nfs");
            }
        }
    };
    if !status.success() {
        drain().context("read mount_nfs diagnostics")?;
        bail!(
            "mount_nfs returned {status}: {}",
            String::from_utf8_lossy(&message).trim()
        );
    }
    if !mount_is_ours(
        mounted.mountpoint.parent().unwrap(),
        &mounted.id,
        Some(session.socket_path()),
    )? {
        bail!("NFS mount did not become ready");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_partial_nfs(id: &str, socket: &mut Option<PathBuf>) -> Result<()> {
    if let Some(path) = socket.as_deref() {
        git_on::platform::cleanup_nfs_socket(id, Some(path))?;
        *socket = None;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn mount_overlay(
    overlay: Overlay,
    dir: &Path,
    id: &str,
    policy: WritePolicy,
    allow_fallback: bool,
    signals: &mut Signals,
    nfs_socket: &mut Option<PathBuf>,
) -> Result<Option<Mounted>> {
    let start = (|| -> std::io::Result<_> {
        let socket = git_on::platform::reserve_nfs_socket(id)?;
        *nfs_socket = Some(socket.clone());
        git_on::backend::start_nfs(overlay, id, socket)
    })();
    let session = match start {
        Ok(session) => session,
        Err(error)
            if allow_fallback
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
        {
            remove_partial_nfs(id, nfs_socket)?;
            return Ok(None);
        }
        Err(error) => return Err(error).context("start private NFS server"),
    };
    let mut mounted = Mounted {
        session: Some(session),
        mountpoint: dir.join("mount"),
        id: id.into(),
    };
    match attach_nfs(&mounted, policy, signals, nfs_socket.as_deref().unwrap()) {
        Ok(()) => Ok(Some(mounted)),
        Err(attach) => {
            let filesystem_failed = match mounted.session.as_ref() {
                Some(git_on::backend::Session::Macos(nfs)) => nfs.filesystem_failed(),
                None => false,
            };
            if let Err(cleanup) = mounted.finish() {
                return Err(anyhow::anyhow!("{attach:#}; cleanup: {cleanup:#}"));
            }
            git_on::platform::ensure_unmounted(dir, None)?;
            remove_partial_nfs(id, nfs_socket)?;
            if allow_fallback && !filesystem_failed && git_on::git::cancelled_signal().is_none() {
                Ok(None)
            } else {
                Err(attach)
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn mount_overlay(
    overlay: Overlay,
    dir: &Path,
    id: &str,
    policy: WritePolicy,
    _allow_fallback: bool,
    _signals: &mut Signals,
) -> Result<Option<Mounted>> {
    let mountpoint = dir.join("mount");
    let session = git_on::backend::mount(overlay, &mountpoint, id, policy)
        .with_context(|| format!(
            "cannot mount snapshot at {}; check that /dev/fuse is usable and fusermount3 is on PATH; containers need host-granted FUSE mount support",
            mountpoint.display()
        ))?;
    let mounted = Mounted {
        session: Some(session),
        mountpoint,
        id: id.into(),
    };
    if !mount_is_ours(dir, id)? {
        bail!("FUSE mount did not become ready");
    }
    Ok(Some(mounted))
}

fn relay_job_control(pid: u32, terminal: &Terminal) -> Result<()> {
    let (Some(parent_pgid), Some(fd)) = (terminal.parent, terminal.descriptor()) else {
        return Ok(());
    };
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WSTOPPED | libc::WNOHANG) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        // WSTOPPED alone reports ECHILD for an exited-but-unreaped child.
        // The WEXITED|WNOWAIT probe below still owns and observes its exit.
        if e.raw_os_error() == Some(libc::ECHILD) {
            return Ok(());
        }
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e.into());
        }
    }
    let current = unsafe { libc::tcgetpgrp(fd) };
    // The invoking shell may have foregrounded a previously background job.
    // Give its child the terminal without requiring a second `fg`.
    if current == parent_pgid {
        foreground(fd, pid as libc::pid_t)?;
    }
    if git_on::platform::siginfo_pid(&info) != 0 {
        let stop = git_on::platform::siginfo_status(&info);
        if current == parent_pgid && (stop == libc::SIGTTIN || stop == libc::SIGTTOU) {
            kill_group(pid, libc::SIGCONT);
            return Ok(());
        }
        let current = unsafe { libc::tcgetpgrp(fd) };
        if current == pid as libc::pid_t {
            foreground(fd, parent_pgid)?;
        }
        unsafe {
            libc::kill(-parent_pgid, libc::SIGSTOP);
        }
        // `fg` gives this runner the terminal; `bg` does not. Never steal it
        // from the invoking shell on a background resume.
        if unsafe { libc::tcgetpgrp(fd) } == parent_pgid {
            foreground(fd, pid as libc::pid_t)?;
        }
        kill_group(pid, libc::SIGCONT);
    }
    Ok(())
}
fn supervise(
    child: &mut std::process::Child,
    signals: &mut Signals,
    terminal: &Terminal,
    mounted: Option<&Mounted>,
) -> Result<ExitStatus> {
    let pid = child.id();
    loop {
        if mounted.is_some_and(Mounted::failed) {
            bail!("filesystem server stopped while the command was running");
        }
        for signal in signals.pending() {
            if matches!(signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT) {
                kill_group(pid, signal);
            }
        }
        relay_job_control(pid, terminal)?;
        if child_exited(pid)? {
            break;
        }
        // `fg` on an already-running background job need not send SIGCONT.
        // Only background terminal jobs poll for that ownership transition;
        // ordinary foreground/noninteractive commands block on signals.
        if terminal
            .descriptor()
            .is_some_and(|fd| unsafe { libc::tcgetpgrp(fd) != pid as libc::pid_t })
        {
            std::thread::sleep(Duration::from_millis(10));
        } else {
            for signal in signals.wait() {
                if matches!(signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT) {
                    kill_group(pid, signal);
                }
            }
        }
    }
    // Descendants cannot keep an ephemeral mount alive after its command exits.
    // Keep the leader unreaped until signalling to avoid process-group ID reuse.
    kill_group(pid, SIGTERM);
    kill_group(pid, libc::SIGKILL);
    Ok(child.wait()?)
}
#[cfg(feature = "test-metrics")]
fn report_metrics(snapshot: &Snapshot, backend: Option<&str>) -> Result<()> {
    if let Some(fd) = env::var_os("GIT_ON_TEST_METRICS_FD") {
        let fd: i32 = fd.to_str().context("invalid metrics fd")?.parse()?;
        if fd < 3 {
            bail!("test metrics descriptor must be separate from standard I/O");
        }
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut output = unsafe { File::from_raw_fd(dup) };
        let mut values = serde_json::to_value(snapshot.metrics())?;
        values["backend"] = serde_json::json!(backend);
        serde_json::to_writer(&mut output, &values)?;
        output.write_all(b"\n")?;
    }
    Ok(())
}
fn finish_before_spawn(finish: impl FnOnce() -> Result<()>) -> Result<Option<ExitStatus>> {
    let finish = finish();
    if let Some(signal) = git_on::git::cancelled_signal() {
        return Ok(Some(ExitStatus::from_raw(signal)));
    }
    finish?;
    Ok(None)
}

fn stop_and_unmount(
    outcome: Result<ExitStatus>,
    unmount: impl FnOnce() -> Result<()>,
) -> Result<ExitStatus> {
    // A filesystem request can still be waiting on a remote Git operation.
    git_on::git::stop_operations();
    match (outcome, unmount()) {
        (Ok(status), Ok(())) => Ok(status),
        (Err(a), Err(b)) => bail!("{a:#}; cleanup: {b:#}"),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

fn finish_preparation(result: Result<ExitStatus>, terminal: Result<()>) -> Result<ExitStatus> {
    use git_on::terminal_ui::TerminalClearFailed;
    match (result, terminal) {
        (Err(error), Err(clearing)) if !error.is::<TerminalClearFailed>() => {
            Err(error.context(clearing))
        }
        (Ok(_), Err(clearing)) => Err(clearing),
        (result, _) => result,
    }
}

#[cfg(test)]
#[test]
fn preparation_reports_why_terminal_handoff_failed() {
    use git_on::terminal_ui::TerminalClearFailed;
    let load = Err(anyhow::anyhow!("native helper could not read a credential"));
    let terminal = Err(TerminalClearFailed.into());
    let error = finish_preparation(load, terminal).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "cannot clear the startup indicator on stderr: native helper could not read a credential"
    );
    let handoff = Err(anyhow::Error::from(TerminalClearFailed).context("launch native Git"));
    let error = finish_preparation(handoff, Err(TerminalClearFailed.into())).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "launch native Git: cannot clear the startup indicator on stderr"
    );
}

#[cfg(test)]
#[test]
fn startup_abort_stops_operations_before_unmount() {
    const CHILD: &str = "GIT_ON_TEST_CANCEL_DURING_FINISH";
    if let Some(mode) = env::var_os(CHILD) {
        let _signals = subscribe_signals().unwrap();
        let outcome = finish_before_spawn(|| {
            if mode == "clear-failure" {
                bail!("fixture terminal clear failure");
            }
            assert_eq!(unsafe { libc::raise(SIGTERM) }, 0);
            Ok(())
        })
        .and_then(|status| status.context("startup attempted to spawn a command"));
        let result = stop_and_unmount(outcome, || {
            assert!(
                git_on::git::operations_stopped(),
                "unmount started while Git operations were active"
            );
            Ok(())
        });
        if mode == "clear-failure" {
            assert_eq!(
                result.unwrap_err().to_string(),
                "fixture terminal clear failure"
            );
        } else {
            assert_eq!(result.unwrap().signal(), Some(SIGTERM));
        }
        return;
    }
    #[cfg(target_os = "linux")]
    let _process = tests::PROCESS_TEST_LOCK.lock().unwrap();
    for mode in ["clear-failure", "signal"] {
        let status = Command::new(env::current_exe().unwrap())
            .args(["--exact", "startup_abort_stops_operations_before_unmount"])
            .env(CHILD, mode)
            .status()
            .unwrap();
        assert!(status.success(), "{mode}");
    }
}

fn execute(invoked: Instant, run: cli::RunArgs<'_>) -> Result<ExitStatus> {
    let mut signals = subscribe_signals()?;
    #[cfg(target_os = "macos")]
    let _child_exit = UnblockedChildExit::new().context("unblock child exit notification")?;
    if let Some(signal) = git_on::git::cancelled_signal() {
        return Ok(ExitStatus::from_raw(signal));
    }

    let temporary = git_on::platform::temporary_base();
    let explain_temp_error = || {
        format!(
            "cannot create a private snapshot in {}. Check the directory's write/search permissions and available space, or choose another TMPDIR",
            temporary.display()
        )
    };
    let parent = fs::canonicalize(&temporary).with_context(explain_temp_error)?;
    let (dir, locked) = run_directory::create(&parent).with_context(explain_temp_error)?;
    execute_in_group(invoked, run, dir, locked, &mut signals)
}

fn execute_in_group(
    invoked: Instant,
    run: cli::RunArgs<'_>,
    dir: PathBuf,
    locked: LockedRun,
    signals: &mut Signals,
) -> Result<ExitStatus> {
    let id = locked.id();
    let cli::RunArgs {
        tree,
        repository: source,
        command: args,
        verify,
        write_policy,
        backend,
    } = run;
    #[cfg(target_os = "macos")]
    let mut nfs_socket = None;
    let mut observed = None;
    let mut progress = None;
    let mut session = None;
    #[cfg(feature = "test-metrics")]
    let mut measured_backend = None;
    let outcome = (|| -> Result<ExitStatus> {
        #[cfg(feature = "test-metrics")]
        for key in ["GIT_ON_TEST_METRICS_FD", "GIT_ON_TEST_DEMAND_FD"] {
            if let Some(fd) = env::var_os(key) {
                let fd: i32 = fd.to_str().context("invalid test descriptor")?.parse()?;
                if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    bail!("invalid {key} descriptor");
                }
            }
        }
        let progress = progress.insert(git_on::terminal_ui::StartupProgress::start(
            invoked, &tree, &dir,
        )?);

        let native = Backend::native();
        let selected = if backend == Backend::Auto {
            native
        } else {
            backend
        };
        let allow_fallback = backend == Backend::Auto && write_policy == WritePolicy::All;
        let unavailable = match selected {
            Backend::Copy => Ok(None),
            selected if selected == native => git_on::platform::mount_unavailable(),
            _ => Ok(Some(format!("this platform provides {}", native.as_str()))),
        };
        let materialized = selected == Backend::Copy
            || (allow_fallback && unavailable.as_ref().is_ok_and(Option::is_some));
        let preparation = if materialized {
            Preparation::Copy
        } else if cfg!(target_os = "macos") && allow_fallback {
            Preparation::MountOrCopy
        } else {
            Preparation::Mount
        };
        let snapshot = Snapshot::load_source_for_execution(
            &env::current_dir()?,
            source.as_deref(),
            &tree,
            verify,
            preparation,
            &dir,
        )?;
        observed = Some(snapshot.clone());

        private_dir(&dir.join("mount"))?;
        private_dir(&dir.join("backing"))?;

        let unavailable = unavailable?;
        session = if materialized {
            None
        } else {
            if let Some(reason) = unavailable {
                let name = selected.as_str().to_ascii_uppercase();
                bail!("{name} unavailable: {reason}; use --backend=auto with explicit --write-policy=all to allow a copy");
            }
            let overlay =
                Overlay::with_write_policy(snapshot.clone(), dir.join("backing"), write_policy)?;
            if let Some(signal) = git_on::git::cancelled_signal() {
                return Ok(ExitStatus::from_raw(signal));
            }
            mount_overlay(
                overlay,
                &dir,
                id,
                write_policy,
                allow_fallback,
                signals,
                #[cfg(target_os = "macos")]
                &mut nfs_socket,
            )?
        };
        if session.is_none() {
            if let Some(signal) = git_on::git::cancelled_signal() {
                return Ok(ExitStatus::from_raw(signal));
            }
            if !materialized {
                snapshot.fetch_copy_objects()?;
            }
            git_on::materialize::materialize(&snapshot, &dir.join("mount"))?;
        }
        if let Some(signal) = signals
            .pending()
            .find(|signal| matches!(*signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT))
        {
            return Ok(ExitStatus::from_raw(signal));
        }
        if session.is_some() {
            snapshot.begin_mounted_reads();
        }
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .current_dir(dir.join("mount"))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .env("GIT_ON_TREE_OID", snapshot.tree_oid.to_string());
        let mut terminal = Terminal::capture();
        let tty_fd = terminal.descriptor();
        let transfer = tty_fd.is_some_and(|fd| unsafe { libc::tcgetpgrp(fd) == libc::getpgrp() });
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if transfer {
                    foreground(tty_fd.unwrap(), libc::getpgrp())?;
                }
                Ok(())
            });
        }

        #[cfg(feature = "test-metrics")]
        {
            measured_backend = Some(
                match session.as_ref().and_then(|mount| mount.session.as_ref()) {
                    #[cfg(target_os = "linux")]
                    Some(git_on::backend::Session::Linux(_)) => "fuse",
                    #[cfg(target_os = "macos")]
                    Some(git_on::backend::Session::Macos(_)) => "nfs",
                    None => Backend::Copy.as_str(),
                },
            );
        }
        if let Some(status) = finish_before_spawn(|| progress.finish())? {
            return Ok(status);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                if transfer {
                    let _ = foreground(tty_fd.unwrap(), unsafe { libc::getpgrp() });
                }
                let next_step = match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        "check the command on PATH, its path inside the selected revision, and its interpreter"
                    }
                    std::io::ErrorKind::PermissionDenied => {
                        "check the executable bit committed in the selected revision and the host mount's execution permissions"
                    }
                    _ => "check the command path and its runtime requirements",
                };
                return Err(e)
                    .with_context(|| format!("cannot start command {:?}; {next_step}", args[0]));
            }
        };

        let pid = child.id();
        terminal.child = Some(pid as libc::pid_t);
        let outcome = supervise(&mut child, signals, &terminal, session.as_ref());

        if let Ok(status) = &outcome {
            if let Some(signal) = status.signal() {
                git_on::git::cancel_by_signal(signal);
            }
        }
        if outcome.is_err() {
            git_on::git::stop_operations();
            kill_group(pid, libc::SIGKILL);
            let _ = child.wait();
        }
        drop(terminal);
        outcome.and_then(|status| {
            if status.success() && git_on::git::cancelled_signal().is_none() {
                snapshot.verify_used_packs()?;
                if session.is_some() {
                    snapshot.finish_idle_native_reader()?;
                }
            }
            Ok(status)
        })
    })();
    let mut result = stop_and_unmount(outcome, || session.as_mut().map_or(Ok(()), Mounted::finish));
    let terminal = progress
        .as_mut()
        .map_or(Ok(()), |progress| progress.finish());

    let mut transport_cleanup_failure = None;
    if let Some(snapshot) = &observed {
        if let Err(error) = snapshot.stop_remote_transport() {
            transport_cleanup_failure = Some(error);
        }
        if let Some(failure) = snapshot.remote_failure() {
            result = Err(anyhow::anyhow!(
                "remote object loading failed; execution incomplete: {failure}"
            ));
        }
        #[cfg(feature = "test-metrics")]
        if let Err(e) = report_metrics(snapshot, measured_backend) {
            result = Err(e);
        }
    }

    let cleanup_result = (|| -> Result<()> {
        if let Some(error) = transport_cleanup_failure {
            return Err(error).with_context(|| {
                format!(
                    "owned remote transport shutdown failed; recovery state retained at {}",
                    dir.display()
                )
            });
        }
        locked.check_directory(&dir)?;
        if mount_is_ours(
            &dir,
            id,
            #[cfg(target_os = "macos")]
            nfs_socket.as_deref(),
        )? {
            return Err(locked.remaining_mount_error(&dir));
        }
        locked.check_unmounted(&dir)?;
        // Also cover errors before an observed Snapshot could be returned.
        git_on::remote::stop_connection(&ssh_control_path(&dir))?;
        #[cfg(target_os = "macos")]
        if let Some(socket) = nfs_socket.as_deref() {
            git_on::platform::cleanup_nfs_socket(id, Some(socket))?;
        }
        locked.prepare_cleanup()?;
        // Workers and mounts are gone. Release cached object descriptors
        // before deleting their backing files, including on NFS clients.
        locked.check_directory(&dir)?;
        drop(observed.take());
        locked.cleanup_service_workspace(&dir)?;
        Ok(())
    })()
    .and_then(|()| locked.remove(&dir))
    .map(|_| ());

    finish_execution(
        &dir,
        finish_preparation(result, terminal),
        cleanup_result,
        observed,
    )
}

#[derive(Debug)]
struct CleanupIncomplete;

impl std::fmt::Display for CleanupIncomplete {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cleanup incomplete")
    }
}

fn finish_execution(
    dir: &Path,
    mut result: Result<ExitStatus>,
    cleanup_result: Result<()>,
    observed: Option<Snapshot>,
) -> Result<ExitStatus> {
    if let Err(e) = cleanup_result {
        if e.is::<PendingGroupCleanup>() {
            // A command that completed still owns its exit status. A later
            // --cleanup reports a pending result until the open file closes.
            git_on::git::write_run_error(format_args!("git-on: {e:#}\n"));
            return result;
        }
        // Keep the private object files available to a later checked cleanup.
        if let Some(snapshot) = observed {
            if let Err(error) = git_on::service_storage::retain_for_cleanup(dir) {
                git_on::git::write_run_error(format_args!(
                    "git-on: retain service files for cleanup: {error:#}\n"
                ));
            }
            std::mem::forget(snapshot);
        }
        let error = match result {
            Ok(_) => e,
            Err(a) => anyhow::anyhow!("{a:#}; cleanup: {e:#}"),
        };
        result = Err(error.context(CleanupIncomplete));
    }
    let group = dir.parent().unwrap();
    let cleanup = remove_empty_temporary_group(group)
        .with_context(|| format!("remove temporary git-on group {}", group.display()))
        .context(CleanupIncomplete);
    let status = result?;
    cleanup?;
    Ok(status)
}
fn subscribe_signals() -> std::io::Result<Signals> {
    git_on::git::initialize_cancellation()?;
    Signals::new([
        SIGINT,
        SIGTERM,
        SIGHUP,
        SIGQUIT,
        libc::SIGCHLD,
        libc::SIGCONT,
    ])
}
fn main() {
    let invoked = Instant::now();
    if let Some(status) = git_on::service_storage::invoke() {
        std::process::exit(status);
    }
    if let Some(status) = git_on::remote::invoke_ssh_launcher() {
        std::process::exit(status);
    }
    if let Some(status) = git_on::terminal_ui::invoke_askpass() {
        std::process::exit(status);
    }
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let result = (|| -> Result<ExitStatus> {
        // main has not created signal handlers, worker threads or Git handles.
        unsafe { git_on::object::configure_pack_descriptors() }?;
        match cli::parse(&args) {
            Ok(cli::Invocation::Cleanup(directory)) => {
                run_directory::cleanup(Path::new(&directory))?;
                Ok(ExitStatus::from_raw(0))
            }
            Ok(cli::Invocation::Run(run)) => execute(invoked, run),
            Err(error) => {
                error.print()?;
                Ok(ExitStatus::from_raw(error.exit_code() << 8))
            }
        }
    })();
    report_run_result(&result);
    match result {
        Ok(status) => {
            if let Some(signal) = status.signal() {
                unsafe {
                    libc::signal(signal, libc::SIG_DFL);
                    libc::raise(signal);
                }
                std::process::exit(128 + signal);
            }
            std::process::exit(status.code().unwrap_or(125));
        }
        Err(_) => {
            if let Some(signal) = git_on::git::cancelled_signal() {
                unsafe {
                    libc::signal(signal, libc::SIG_DFL);
                    libc::raise(signal);
                }
                std::process::exit(128 + signal);
            }
            std::process::exit(125);
        }
    }
}

fn report_run_result(result: &Result<ExitStatus>) {
    if let Err(error) = result {
        // Keep recovery details even if cancellation arrives after teardown.
        if error.is::<CleanupIncomplete>() || git_on::git::cancelled_signal().is_none() {
            git_on::git::write_run_error(format_args!("git-on: {error:#}\n"));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
    // Fork briefly inherits other threads' flock descriptions before exec.
    // Keep process-spawning and lock-release assertions in separate test epochs.
    pub(super) static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn cancelled_cleanup_can_exit_with_blocked_stderr() {
        use std::{
            io::{ErrorKind, Write},
            os::fd::OwnedFd,
            os::unix::net::UnixStream,
        };

        const CHILD: &str = "GIT_ON_TEST_BLOCKED_CLEANUP_DIAGNOSTIC";
        const MODE: &str = "GIT_ON_TEST_CLEANUP_DIAGNOSTIC_MODE";
        if let Some(run) = env::var_os(CHILD) {
            let mode = env::var_os(MODE).unwrap();
            let mut run = PathBuf::from(run);
            if mode == "late-cleanup-failure" {
                let result = finish_execution(
                    &run,
                    Ok(ExitStatus::from_raw(42 << 8)),
                    Err(anyhow::anyhow!("use --cleanup {}", run.display())),
                    None,
                );
                git_on::git::cancel_by_signal(SIGTERM);
                report_run_result(&result);
                return;
            }
            if mode == "late" {
                git_on::git::initialize_cancellation().unwrap();
                // Only displayed. Fill the pipe before the parent signals us.
                run.push("x".repeat(1024 * 1024));
            } else {
                git_on::git::cancel_by_signal(SIGTERM);
            }
            if mode != "error" {
                let status = finish_execution(
                    &run,
                    Ok(ExitStatus::from_raw(42 << 8)),
                    Err(PendingGroupCleanup::retry(&run).into()),
                    None,
                )
                .unwrap();
                assert_eq!(status.code(), Some(42));
                return;
            }
            let result = finish_execution(
                &run,
                Ok(ExitStatus::from_raw(SIGTERM)),
                Err(anyhow::anyhow!("fixture cleanup failed")),
                None,
            );
            report_run_result(&result);
            let error = result.unwrap_err();
            assert!(format!("{error:#}").contains("fixture cleanup failed"));
            return;
        }

        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let run = temporary.path().join("run");
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::cancelled_cleanup_can_exit_with_blocked_stderr",
            ])
            .env(CHILD, &run)
            .env("GIT_TRACE_PACKET", "2");
        let output = command.env(MODE, "pending").output().unwrap();
        assert!(output.status.success());
        let warning = String::from_utf8(output.stderr).unwrap();
        assert!(warning.contains("--cleanup") && warning.contains(&*run.to_string_lossy()));
        let output = command.env(MODE, "late-cleanup-failure").output().unwrap();
        assert!(output.status.success());
        let warning = String::from_utf8(output.stderr).unwrap();
        assert_eq!(warning.matches("--cleanup").count(), 1, "{warning}");
        assert!(warning.contains(&*run.to_string_lossy()));

        for mode in ["pending", "error", "late"] {
            if mode != "pending" {
                command.env_remove("GIT_TRACE_PACKET");
            }
            let (reader, mut writer) = UnixStream::pair().unwrap();
            let watch = if mode == "late" {
                // The diagnostic is bounded before it reaches stderr. A
                // default socket buffer can hold the entire warning.
                let capacity: libc::c_int = 4096;
                assert_eq!(
                    unsafe {
                        libc::setsockopt(
                            writer.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_SNDBUF,
                            std::ptr::from_ref(&capacity).cast(),
                            std::mem::size_of_val(&capacity) as libc::socklen_t,
                        )
                    },
                    0
                );
                Some(writer.try_clone().unwrap())
            } else {
                writer.set_nonblocking(true).unwrap();
                let full = loop {
                    if let Err(error) = writer.write(&[0]) {
                        break error;
                    }
                };
                assert_eq!(full.kind(), ErrorKind::WouldBlock);
                writer.set_nonblocking(false).unwrap();
                None
            };
            let mut child = command
                .env(MODE, mode)
                .stderr(Stdio::from(OwnedFd::from(writer)))
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(4);
            if let Some(watch) = watch {
                let mut ready = [
                    libc::pollfd {
                        fd: reader.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: watch.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    },
                ];
                loop {
                    unsafe { libc::poll(ready.as_mut_ptr(), 2, 10) };
                    if ready[0].revents & libc::POLLIN != 0 && ready[1].revents & libc::POLLOUT == 0
                    {
                        assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, SIGTERM) }, 0);
                        break;
                    }
                    if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("cleanup warning did not reach the full pipe");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "{mode}: {status}");
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("{mode}: cancelled cleanup blocked on stderr");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn clean_unmount_waits_for_active_helper_but_bounds_completed_failures() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let mut slow_success = Command::new("/bin/sh");
        slow_success.args(["-c", "exec /bin/sleep 4", "unmount-test"]);
        let start = Instant::now();
        clean_unmount_with(slow_success, Path::new("unused-test-mount")).unwrap();
        assert!(start.elapsed() >= Duration::from_secs(4));

        let mut busy = Command::new("/bin/sh");
        busy.args(["-c", "exit 1", "unmount-test"]);
        let start = Instant::now();
        let error = clean_unmount_with(busy, Path::new("unused-test-mount")).unwrap_err();
        assert!(error
            .to_string()
            .contains("mount is busy or unmount failed"));
        assert!(!error.to_string().contains("--cleanup"));
        assert!(start.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn review_signal_before_first_native_git_is_not_lost() {
        const CHILD_FIFO: &str = "GIT_ON_TEST_EARLY_SIGNAL_FIFO";
        const CHILD_MODE: &str = "GIT_ON_TEST_EARLY_SIGNAL_MODE";
        if let Some(fifo) = env::var_os(CHILD_FIFO) {
            let directory = Path::new(&fifo).parent().unwrap();
            if env::var_os(CHILD_MODE).is_some_and(|mode| mode == "concurrent") {
                let barrier = std::sync::Barrier::new(4);
                std::thread::scope(|scope| {
                    for _ in 0..4 {
                        scope.spawn(|| {
                            barrier.wait();
                            let error = git_on::git::Git::capture(directory).unwrap_err();
                            assert!(format!("{error:#}").contains("cancelled by signal"));
                        });
                    }
                });
                eprintln!("four concurrent first native Git operations cancelled");
            } else {
                let mut signals = subscribe_signals().unwrap();
                assert_eq!(unsafe { libc::raise(SIGTERM) }, 0);
                assert!(signals.pending().any(|signal| signal == SIGTERM));
                eprintln!("real SIGTERM observed before first native Git operation");
                let error = git_on::git::Git::capture(directory).unwrap_err();
                assert!(format!("{error:#}").contains("cancelled by signal"));
            }
            assert_eq!(git_on::git::cancelled_signal(), Some(SIGTERM));
            return;
        }

        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        for mode in ["early", "concurrent"] {
            let temporary = tempfile::tempdir().unwrap();
            let fifo = temporary.path().join("config-fifo");
            let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
            let native_git = env::split_paths(&env::var_os("PATH").unwrap())
                .map(|directory| directory.join("git"))
                .find(|candidate| {
                    fs::metadata(candidate).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
                .map(|path| fs::canonicalize(path).unwrap())
                .expect("real native Git required for the cancellation regression");
            let programs = temporary.path().join("programs");
            fs::create_dir(&programs).unwrap();
            let started = temporary.path().join("native-started");
            let wrapper = programs.join("git");
            let quoted_git = native_git.to_string_lossy().replace('\'', "'\\''");
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\necho $$ >> '{}'\nexec '{}' \"$@\"\n",
                    started.display(),
                    quoted_git
                ),
            )
            .unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
            let stdout = tempfile::tempfile().unwrap();
            let stderr = tempfile::tempfile().unwrap();
            let mut child = Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::review_signal_before_first_native_git_is_not_lost",
                    "--nocapture",
                ])
                .env(CHILD_FIFO, &fifo)
                .env(CHILD_MODE, mode)
                .env("PATH", programs)
                .env("GIT_CONFIG_GLOBAL", &fifo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .stdout(stdout.try_clone().unwrap())
                .stderr(stderr.try_clone().unwrap())
                .spawn()
                .unwrap();
            let mut deadline =
                Instant::now() + Duration::from_secs(if mode == "early" { 2 } else { 5 });
            let mut native_pids = Vec::new();
            let mut delivered = false;
            let mut native_observed = false;
            let status = loop {
                if let Ok(records) = fs::read_to_string(&started) {
                    native_pids = records
                        .lines()
                        .filter_map(|pid| pid.parse::<u32>().ok())
                        .collect();
                }
                let actual_git = !native_pids.is_empty()
                    && native_pids.iter().all(|pid| {
                        fs::read_link(format!("/proc/{pid}/exe")).ok().as_ref() == Some(&native_git)
                            && fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|args| {
                                args.split(|byte| *byte == 0).nth(1) == Some(b"config".as_slice())
                            })
                    });
                native_observed |= actual_git;
                if mode == "concurrent" && native_pids.len() == 4 && actual_git && !delivered {
                    assert_eq!(unsafe { libc::kill(child.id() as i32, SIGTERM) }, 0);
                    delivered = true;
                    deadline = Instant::now() + Duration::from_secs(2);
                }
                if let Some(status) = child.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            if status.is_none() {
                for pid in &native_pids {
                    kill_group(*pid, libc::SIGKILL);
                }
                child.kill().unwrap();
                child.wait().unwrap();
            }
            use std::io::{Read, Seek};
            let mut stderr = stderr;
            stderr.rewind().unwrap();
            let mut diagnostics = String::new();
            stderr.read_to_string(&mut diagnostics).unwrap();
            assert!(diagnostics.contains(if mode == "early" {
                "real SIGTERM observed before first native Git operation"
            } else {
                "four concurrent first native Git operations cancelled"
            }));
            assert!(
            status.is_some(),
            "early SIGTERM was lost; real native Git observed={native_observed} and blocked on config FIFO: {diagnostics}"
        );
            assert!(status.unwrap().success(), "{diagnostics}");
            if mode == "early" {
                assert!(!started.exists(), "cancelled invocation started native Git");
            } else {
                assert!(delivered && native_observed && native_pids.len() == 4);
                for pid in native_pids {
                    assert!(
                        !Path::new(&format!("/proc/{pid}")).exists(),
                        "native Git survived cancellation"
                    );
                }
            }
        }
    }
    #[test]
    fn event_supervisor_preserves_exit_and_signal() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        for script in ["exit 23", "kill -TERM $$"] {
            let mut signals = Signals::new([
                SIGINT,
                SIGTERM,
                SIGHUP,
                SIGQUIT,
                libc::SIGCHLD,
                libc::SIGCONT,
            ])
            .unwrap();
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let terminal = Terminal {
                tty: None,
                parent: None,
                child: None,
            };
            let status = supervise(&mut child, &mut signals, &terminal, None).unwrap();
            if script.starts_with("exit") {
                assert_eq!(status.code(), Some(23));
            } else {
                assert_eq!(status.signal(), Some(SIGTERM));
            }
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn mount_escapes_are_decoded() {
        assert_eq!(
            git_on::platform::decode_mount("/tmp/a\\040b\\134c"),
            b"/tmp/a b\\c"
        );
    }
}
