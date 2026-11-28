use anyhow::{bail, Context, Result};
use git_on::{
    backend::Backend,
    filesystem::Overlay,
    git::set_foreground as foreground,
    platform::{ensure_unmounted, mount_is_ours},
    policy::WritePolicy,
    snapshot::Snapshot,
};
use serde::{Deserialize, Serialize};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};
use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

mod cli;
mod run_directory;

// Ephemeral recovery information only: no invocation history, arguments or logs.
#[derive(Serialize, Deserialize)]
struct Recovery {
    format: String,
    id: String,
    // Preserve these fields when reading recovery records from 1.5.2.
    #[serde(skip_serializing)]
    git_dir: Option<PathBuf>,
    #[serde(skip_serializing)]
    pin_ref: Option<String>,
    #[serde(skip_serializing)]
    tree_oid: Option<String>,
}
fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}
fn save(dir: &Path, state: &Recovery) -> Result<()> {
    let path = dir.join(".state-new");
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    serde_json::to_writer(&mut f, state)?;
    f.flush()?;
    fs::rename(path, dir.join(".state"))?;
    Ok(())
}
fn lock(dir: &Path, create: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(".lock"))?;
    anyhow::ensure!(f.metadata()?.is_file(), "invalid temporary run lock");
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("run is active or locked");
    }
    Ok(f)
}
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
                bail!("clean unmount cancelled by signal {signal}; recovery state preserved for --cleanup");
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
            bail!("mount is busy or unmount failed; transient data preserved for --cleanup");
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
            let result =
                mount_is_ours(self.mountpoint.parent().unwrap(), &self.id).and_then(|mounted| {
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
                // Process exit closes the FUSE connection. The run directory
                // remains available for checked stale cleanup.
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
fn attach_nfs(mounted: &Mounted, policy: WritePolicy, signals: &mut Signals) -> Result<()> {
    use std::io::{Read, Seek};
    #[cfg(feature = "test-metrics")]
    if env::var_os("GIT_ON_TEST_NFS_MOUNT_ERROR").as_deref() == Some(std::ffi::OsStr::new("1")) {
        bail!("test NFS mount failure");
    }
    let mut diagnostic = tempfile::tempfile_in(mounted.mountpoint.parent().unwrap())?;
    let mut helper =
        git_on::backend::nfs::Session::mount_command(&mounted.mountpoint, &mounted.id, policy)?
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(diagnostic.try_clone()?)
            .spawn()
            .context("start mount_nfs")?;
    let status = loop {
        if git_on::git::cancelled_signal().is_some() || mounted.failed() {
            kill_group(helper.id(), libc::SIGKILL);
            helper.wait().context("reap mount_nfs")?;
            bail!("NFS mount interrupted or server stopped");
        }
        match helper.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                signals.wait().for_each(drop);
            }
            Err(error) => {
                kill_group(helper.id(), libc::SIGKILL);
                let _ = helper.wait();
                return Err(error).context("wait for mount_nfs");
            }
        }
    };
    if !status.success() {
        let mut message = Vec::new();
        diagnostic.rewind()?;
        diagnostic.take(4096).read_to_end(&mut message)?;
        bail!(
            "mount_nfs returned {status}: {}",
            String::from_utf8_lossy(&message).trim()
        );
    }
    if !mount_is_ours(mounted.mountpoint.parent().unwrap(), &mounted.id)? {
        bail!("NFS mount did not become ready");
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
) -> Result<Option<Mounted>> {
    let session = match git_on::backend::start_nfs(overlay, id) {
        Ok(session) => session,
        Err(error)
            if allow_fallback
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).context("start private NFS server"),
    };
    let mut mounted = Mounted {
        session: Some(session),
        mountpoint: dir.join("mount"),
        id: id.into(),
    };
    match attach_nfs(&mounted, policy, signals) {
        Ok(()) => Ok(Some(mounted)),
        Err(attach) => {
            let filesystem_failed = match mounted.session.as_ref() {
                Some(git_on::backend::Session::Macos(nfs)) => nfs.filesystem_failed(),
                None => false,
            };
            if let Err(cleanup) = mounted.finish() {
                return Err(anyhow::anyhow!("{attach:#}; cleanup: {cleanup:#}"));
            }
            ensure_unmounted(dir)?;
            git_on::platform::cleanup_nfs_socket(id)?;
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

fn child_exited(pid: u32) -> Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e.into());
    }
    Ok(git_on::platform::siginfo_pid(&info) != 0)
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
fn cleanup(dir: &Path) -> Result<()> {
    let dir = fs::canonicalize(dir)?;
    let _group = run_directory::Group::for_run(&dir);
    let held_lock = lock(&dir, false)?;
    let state: Recovery = serde_json::from_reader(File::open(dir.join(".state"))?)?;
    if state.format != "git-on-temp-v1" {
        bail!("unrecognized temporary directory");
    }
    if mount_is_ours(&dir, &state.id)? {
        #[cfg(target_os = "linux")]
        clean_unmount(&dir.join("mount"))?;
        #[cfg(target_os = "macos")]
        unmount_dead_server(&dir.join("mount"))?;
    }
    ensure_unmounted(&dir)?;
    git_on::remote::stop_connection(&dir.join("ssh-control"))?;
    #[cfg(target_os = "linux")]
    git_on::git::cleanup_native_scratch(&dir, &state.id)?;
    #[cfg(target_os = "macos")]
    git_on::platform::cleanup_nfs_socket(&state.id)?;
    git_on::materialize::prepare_cleanup(&dir.join("mount"))?;
    // Finish checked mount and socket teardown even when the old record
    // must remain available to release its source ref with the prior version.
    if let (Some(pin), Some(repository)) = (&state.pin_ref, &state.git_dir) {
        if !repository.starts_with(&dir) {
            anyhow::ensure!(
                pin == &format!("refs/git-on/{}", state.id),
                "invalid legacy source reference in recovery record"
            );
            let repo = git2::Repository::open(repository).with_context(|| {
                format!(
                    "legacy source repository unavailable: {}",
                    repository.display()
                )
            })?;
            match repo.find_reference(pin) {
                Ok(reference) => {
                    let recorded = state
                        .tree_oid
                        .as_deref()
                        .and_then(|oid| git2::Oid::from_str(oid).ok());
                    if recorded.is_none() || reference.target() != recorded {
                        bail!("legacy source ref {pin} has a different target; recovery record retained");
                    }
                    bail!(
                    "legacy run records source ref {pin} in {}; use git-on 1.5.2 to clean up this run",
                    repository.display()
                    );
                }
                Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                Err(error) => return Err(error).context("inspect legacy recovery reference"),
            };
        }
    }
    // Close the writable marker before unlinking it. NFS can retain an
    // unlinked file while its descriptor is open and keep the directory busy.
    drop(held_lock);
    fs::remove_dir_all(dir)?;
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
    use std::os::fd::FromRawFd;
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
fn execute(invoked: Instant, run: cli::RunArgs<'_>) -> Result<ExitStatus> {
    let cli::RunArgs {
        tree,
        repository: source,
        command: args,
        verify,
        write_policy,
        backend,
    } = run;
    let mut signals = subscribe_signals()?;
    #[cfg(target_os = "macos")]
    let _child_exit = UnblockedChildExit::new().context("unblock child exit notification")?;
    if let Some(signal) = git_on::git::cancelled_signal() {
        return Ok(ExitStatus::from_raw(signal));
    }
    let (_group, dir, id) = run_directory::Group::create().with_context(|| {
            format!("cannot create a private snapshot in {}; check the directory's write/search permissions and available space, or choose another TMPDIR", env::temp_dir().display())
        })?;
    let held_lock = lock(&dir, true).inspect_err(|_| {
        let _ = fs::remove_dir_all(&dir);
    })?;
    let state = Recovery {
        format: "git-on-temp-v1".into(),
        id: id.clone(),
        git_dir: None,
        pin_ref: None,
        tree_oid: None,
    };
    let mut observed = None;
    let mut progress = None;
    #[cfg(feature = "test-metrics")]
    let mut measured_backend = None;
    let mut result = (|| -> Result<ExitStatus> {
        #[cfg(feature = "test-metrics")]
        if let Some(fd) = env::var_os("GIT_ON_TEST_METRICS_FD") {
            let fd: i32 = fd.to_str().context("invalid metrics fd")?.parse()?;
            if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                bail!("invalid test metrics descriptor");
            }
        }
        save(&dir, &state)?;
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
        let snapshot = Snapshot::load_source(
            &env::current_dir()?,
            source.as_deref(),
            &tree,
            verify,
            materialized,
            &dir,
        )?;
        observed = Some(snapshot.clone());

        private_dir(&dir.join("mount"))?;
        private_dir(&dir.join("backing"))?;

        let unavailable = unavailable?;
        let mut session = if materialized {
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
                &id,
                write_policy,
                allow_fallback,
                &mut signals,
            )?
        };
        if session.is_none() {
            if let Some(signal) = git_on::git::cancelled_signal() {
                return Ok(ExitStatus::from_raw(signal));
            }
            if !materialized {
                snapshot.prepare_copy_after_mount_failure()?;
            }
            git_on::materialize::materialize(&snapshot, &dir.join("mount"))?;
        }
        if let Some(signal) = signals
            .pending()
            .find(|signal| matches!(*signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT))
        {
            return Ok(ExitStatus::from_raw(signal));
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
        progress.finish();
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
        let mut outcome = supervise(&mut child, &mut signals, &terminal, session.as_ref());

        if let Ok(status) = &outcome {
            if let Some(signal) = status.signal() {
                git_on::git::cancel_by_signal(signal);
            }
        }
        if outcome.is_err() {
            kill_group(pid, libc::SIGKILL);
            let _ = child.wait();
        }
        drop(terminal);
        // Normal exit still needs the verdict of packs that supplied bytes.
        // Cancel other work before unmount so unused reads cannot block it.
        if outcome
            .as_ref()
            .is_ok_and(|status| status.signal().is_none())
            && git_on::git::cancelled_signal().is_none()
        {
            snapshot.finish_remote_reads();
            if let Some(signal) = git_on::git::cancelled_signal() {
                outcome = Ok(ExitStatus::from_raw(signal));
            }
        }
        git_on::git::stop_operations();

        let unmount = session.as_mut().map_or(Ok(()), Mounted::finish);

        match (outcome, unmount) {
            (Ok(status), Ok(())) => Ok(status),
            (Err(a), Err(b)) => bail!("{a:#}; cleanup: {b:#}"),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    })();
    if let Some(mut progress) = progress {
        progress.finish();
    }
    // Also cover errors before the child could start. State inspection below
    // must never wait behind an owned operation that the run no longer needs.
    git_on::git::stop_operations();

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
        if mount_is_ours(&dir, &id)? {
            bail!(
                "temporary mount remains at {}; use --cleanup after stopping its users",
                dir.display()
            );
        }
        ensure_unmounted(&dir)?;
        // Also cover errors before an observed Snapshot could be returned.
        git_on::remote::stop_connection(&dir.join("ssh-control"))?;
        #[cfg(target_os = "linux")]
        git_on::git::cleanup_native_scratch(&dir, &id)?;
        #[cfg(target_os = "macos")]
        git_on::platform::cleanup_nfs_socket(&id)?;
        git_on::materialize::prepare_cleanup(&dir.join("mount"))?;
        match fs::remove_file(dir.join(git_on::remote::SSH_CONTROL_OWNER_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove SSH recovery identity"),
        }
        drop(observed.take());
        drop(held_lock);
        fs::remove_dir_all(&dir)?;
        Ok(())
    })();

    if let Err(e) = cleanup_result {
        // Leave the retained run for explicit cleanup without retrying transport shutdown.
        if let Some(snapshot) = observed.take() {
            std::mem::forget(snapshot);
        }
        if git_on::git::cancelled_signal().is_some() {
            eprintln!("git-on: {e:#}");
        }
        result = Err(match result {
            Ok(_) => e,
            Err(a) => anyhow::anyhow!("{a:#}; cleanup: {e:#}"),
        });
    }
    result
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
fn handle_file_size_errors() -> std::io::Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    if unsafe { libc::sigaction(libc::SIGXFSZ, std::ptr::null(), action.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { action.assume_init() }.sa_sigaction == libc::SIG_DFL {
        // Catch daemon write-limit signals so EFBIG reaches normal cleanup.
        // Exec resets caught handlers to default; an inherited SIG_IGN remains
        // unchanged, preserving the caller's choice in Git and the command.
        unsafe { signal_hook::low_level::register(libc::SIGXFSZ, || {}) }?;
    }
    Ok(())
}
fn main() {
    let invoked = Instant::now();
    if let Some(status) = git_on::terminal_ui::invoke_askpass() {
        std::process::exit(status);
    }
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let result = (|| -> Result<ExitStatus> {
        // main has not created signal handlers, worker threads or Git handles.
        unsafe { git_on::object::configure_pack_descriptors() }?;
        handle_file_size_errors()?;
        match cli::parse(&args) {
            Ok(cli::Invocation::Cleanup(directory)) => {
                cleanup(Path::new(&directory))?;
                Ok(ExitStatus::from_raw(0))
            }
            Ok(cli::Invocation::Run(run)) => execute(invoked, run),
            Err(error) => {
                error.print()?;
                Ok(ExitStatus::from_raw(error.exit_code() << 8))
            }
        }
    })();
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
        Err(e) => {
            if let Some(signal) = git_on::git::cancelled_signal() {
                unsafe {
                    libc::signal(signal, libc::SIG_DFL);
                    libc::raise(signal);
                }
                std::process::exit(128 + signal);
            }
            eprintln!("git-on: {e:#}");
            std::process::exit(125);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    // Fork briefly inherits other threads' flock descriptions before exec.
    // Keep process-spawning and lock-release assertions in separate test epochs.
    static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn recovery() -> Recovery {
        Recovery {
            format: "git-on-temp-v1".into(),
            id: "unit-test".into(),
            git_dir: None,
            pin_ref: None,
            tree_oid: None,
        }
    }
    #[test]
    fn recovery_refuses_active_runs_and_removes_disposable_state() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("temp");
        private_dir(&dir).unwrap();
        let held = lock(&dir, true).unwrap();
        save(&dir, &recovery()).unwrap();
        assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        cleanup(&dir).unwrap();
        assert!(!dir.exists());
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
