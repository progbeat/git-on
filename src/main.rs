use anyhow::{bail, Context, Result};
use git_on::{
    filesystem::Overlay,
    platform::{ensure_unmounted, mount_is_ours},
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
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

mod cli;
#[cfg(feature = "test-metrics")]
mod startup_timing;

// Ephemeral recovery information only: no invocation history, arguments or logs.
#[derive(Serialize, Deserialize)]
struct Recovery {
    format: String,
    id: String,
    git_dir: Option<PathBuf>,
    tree_oid: Option<String>,
    pin_ref: Option<String>,
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
        .create(create)
        .mode(0o600)
        .open(dir.join(".lock"))?;
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
fn release_pin(m: &Recovery) -> Result<()> {
    if let (Some(repo), Some(pin), Some(oid)) = (&m.git_dir, &m.pin_ref, &m.tree_oid) {
        if pin != &format!("refs/git-on/{}", m.id) {
            bail!("invalid pin reference");
        }
        let repo = git2::Repository::open(repo)?;
        match repo.find_reference(pin) {
            Ok(mut r) => {
                if r.target().map(|x| x.to_string()).as_ref() != Some(oid) {
                    bail!("pin changed; refusing deletion");
                }
                r.delete()?;
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => {}
            Err(e) => return Err(e.into()),
        };
    }
    Ok(())
}
// tcsetpgrp may send SIGTTOU to a background caller; temporarily block it.
fn foreground(fd: i32, pgid: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let rc = libc::tcsetpgrp(fd, pgid);
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if rc != 0 {
            return Err(error);
        }
    }
    Ok(())
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
fn clean_unmount_with(mut unmount: Command, mountpoint: &Path) -> Result<()> {
    unmount
        .arg(mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let start = Instant::now();
    loop {
        // A successful clean unmount may reclaim every visited inode. Its
        // duration grows with the actual traversal; elapsed time alone cannot
        // distinguish that work from a busy mount. Wait for the helper's real
        // result, and bound only retries of completed failures below.
        let status = unmount
            .spawn()
            .context("start clean unmount")?
            .wait()
            .context("wait for clean unmount")?;
        if status.success() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            bail!(
                "mount is busy or unmount failed; transient data and pin preserved for --cleanup"
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
    fn finish(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            let result =
                mount_is_ours(self.mountpoint.parent().unwrap(), &self.id).and_then(|mounted| {
                    if mounted {
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
            #[cfg(target_os = "macos")]
            result?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.join()))
                .map_err(|_| anyhow::anyhow!("filesystem thread panicked"))??;
        }
        Ok(())
    }
}
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = self.finish();
    }
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
    let _lock = lock(&dir, false)?;
    let state: Recovery = serde_json::from_reader(File::open(dir.join(".state"))?)?;
    if state.format != "git-on-temp-v1" {
        bail!("unrecognized temporary directory");
    }
    if mount_is_ours(&dir, &state.id)? {
        clean_unmount(&dir.join("mount"))?;
    }
    ensure_unmounted(&dir)?;
    git_on::remote::stop_connection(&dir.join("ssh-control"))?;
    git_on::materialize::prepare_cleanup(&dir.join("mount"))?;
    release_pin(&state)?;
    fs::remove_dir_all(dir)?;
    Ok(())
}
fn supervise(
    child: &mut std::process::Child,
    signals: &mut Signals,
    terminal: &Terminal,
) -> Result<ExitStatus> {
    let pid = child.id();
    loop {
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
fn report_metrics(snapshot: &Snapshot) -> Result<()> {
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
        serde_json::to_writer(&mut output, &snapshot.metrics())?;
        output.write_all(b"\n")?;
    }
    Ok(())
}
fn execute(
    tree: OsString,
    args: Vec<OsString>,
    source: Option<OsString>,
    verify: bool,
    write_policy: git_on::policy::WritePolicy,
) -> Result<ExitStatus> {
    #[cfg(feature = "test-metrics")]
    let mut timing = startup_timing::Timing::new()?;
    macro_rules! phase {
        ($name:expr) => {
            #[cfg(feature = "test-metrics")]
            timing.mark($name);
        };
    }
    phase!("execute_begin");
    let mut signals = subscribe_signals()?;
    if let Some(signal) = git_on::git::cancelled_signal() {
        return Ok(ExitStatus::from_raw(signal));
    }
    let temp = tempfile::Builder::new()
        .prefix("git-on-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .with_context(|| {
            format!("cannot create a private snapshot in {}; check the directory's write/search permissions and available space, or choose another TMPDIR", env::temp_dir().display())
        })?;
    let dir = fs::canonicalize(temp.path())?;
    let held_lock = lock(&dir, true)?;
    let _retained = temp.keep();
    let id = uuid::Uuid::new_v4().to_string();
    let mut state = Recovery {
        format: "git-on-temp-v1".into(),
        id: id.clone(),
        git_dir: None,
        tree_oid: None,
        pin_ref: None,
    };
    let _lock = held_lock;
    let mut observed = None;
    let mut result = (|| -> Result<ExitStatus> {
        #[cfg(feature = "test-metrics")]
        if let Some(fd) = env::var_os("GIT_ON_TEST_METRICS_FD") {
            let fd: i32 = fd.to_str().context("invalid metrics fd")?.parse()?;
            if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                bail!("invalid test metrics descriptor");
            }
        }
        save(&dir, &state)?;
        phase!("source_begin");
        let snapshot = Snapshot::load_source_with_pin_record(
            &env::current_dir()?,
            source.as_deref(),
            &tree,
            verify,
            &dir,
            &id,
            |git_dir, pin, oid| {
                state.git_dir = Some(git_dir.into());
                state.pin_ref = Some(pin.into());
                state.tree_oid = Some(oid.to_string());
                save(&dir, &state)
            },
        )?;
        observed = Some(snapshot.clone());
        phase!("source_ready");
        private_dir(&dir.join("mount"))?;
        private_dir(&dir.join("backing"))?;
        phase!("backend_begin");
        let unavailable = git_on::platform::fuse_unavailable()?;
        let mut session = if let Some(reason) = unavailable {
            if write_policy != git_on::policy::WritePolicy::All {
                bail!("FUSE unavailable: {reason}; policy {write_policy:?} requires FUSE. Only explicit --write-policy=all supports materialization");
            }
            git_on::materialize::materialize(&snapshot, &dir.join("mount"))?;
            None
        } else {
            let overlay = Overlay::with_write_policy(snapshot, dir.join("backing"), write_policy)?;
            if let Some(signal) = signals
                .pending()
                .find(|signal| matches!(*signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT))
            {
                return Ok(ExitStatus::from_raw(signal));
            }
            let session = git_on::backend::mount(overlay, &dir.join("mount"), &id, write_policy)
                .with_context(|| {
                    #[cfg(target_os = "linux")]
                    let prerequisite = "check that /dev/fuse is usable and fusermount3 is on PATH; containers need host-granted FUSE mount support";
                    #[cfg(target_os = "macos")]
                    let prerequisite = "macOS mounts are outside git-on 1.3.0; explicitly select --write-policy=all for materialization";
                    format!("cannot mount snapshot at {}; {prerequisite}", dir.join("mount").display())
                })?;
            let session = Mounted {
                session: Some(session),
                mountpoint: dir.join("mount"),
                id: id.clone(),
            };
            if !mount_is_ours(&dir, &id)? {
                bail!("FUSE mount did not become ready");
            }
            Some(session)
        };
        phase!(if session.is_some() {
            "backend_fuse_ready"
        } else {
            "backend_materialized_ready"
        });
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
            .env("GIT_ON_TREE_OID", state.tree_oid.as_ref().unwrap());
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
        phase!("command_spawn_begin");
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
        phase!("command_started");
        let pid = child.id();
        terminal.child = Some(pid as libc::pid_t);
        let outcome = supervise(&mut child, &mut signals, &terminal);
        phase!("command_done");
        if let Ok(status) = &outcome {
            if let Some(signal) = status.signal() {
                git_on::git::cancel_by_signal(signal);
            }
        }
        // Escaped or terminated readers can leave a FUSE callback inside a
        // native fetch. Stop it before unmounting or inspecting snapshot state.
        git_on::git::stop_operations();
        if outcome.is_err() {
            kill_group(pid, libc::SIGKILL);
            let _ = child.wait();
        }
        drop(terminal);
        phase!("unmount_begin");
        let unmount = session.as_mut().map_or(Ok(()), Mounted::finish);
        phase!("unmount_done");
        match (outcome, unmount) {
            (Ok(status), Ok(())) => Ok(status),
            (Err(a), Err(b)) => bail!("{a:#}; cleanup: {b:#}"),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    })();
    // Also cover errors before the child could start. State inspection below
    // must never wait behind an owned operation that the run no longer needs.
    git_on::git::stop_operations();
    phase!("transport_cleanup_begin");
    let mut transport_cleanup_failure = None;
    if let Some(snapshot) = observed.take() {
        if let Some(failure) = snapshot.remote_failure() {
            result = Err(anyhow::anyhow!(
                "remote object loading failed; execution incomplete: {failure}"
            ));
        }
        #[cfg(feature = "test-metrics")]
        if let Err(e) = report_metrics(&snapshot) {
            result = Err(e);
        }
        if let Err(error) = snapshot.stop_remote_transport() {
            transport_cleanup_failure = Some(error);
        }
        // A busy mount or failed owned transport shutdown keeps the pin and
        // recovery identity available for explicit stale cleanup.
        if transport_cleanup_failure.is_some() || mount_is_ours(&dir, &id).unwrap_or(true) {
            std::mem::forget(snapshot);
        }
    }
    phase!("transport_cleanup_done");
    phase!("filesystem_cleanup_begin");
    let cleanup_result = (|| -> Result<()> {
        if let Some(error) = transport_cleanup_failure {
            return Err(error).with_context(|| {
                format!(
                    "owned SSH shutdown failed; recovery state retained at {}",
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
        git_on::materialize::prepare_cleanup(&dir.join("mount"))?;
        release_pin(&state)?;
        if dir.join("remote-repository").exists() {
            fs::remove_dir_all(dir.join("remote-repository"))?;
        }
        match fs::remove_file(dir.join(git_on::remote::SSH_CONTROL_OWNER_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove SSH recovery identity"),
        }
        fs::remove_dir_all(&dir)?;
        Ok(())
    })();
    phase!("filesystem_cleanup_done");
    if let Err(e) = cleanup_result {
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
fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let result = (|| -> Result<ExitStatus> {
        // main has not created signal handlers, worker threads or Git handles.
        unsafe { git_on::object::configure_pack_descriptors() }?;
        match cli::parse(&args) {
            Ok(cli::Invocation::Cleanup(directory)) => {
                cleanup(Path::new(&directory))?;
                Ok(ExitStatus::from_raw(0))
            }
            Ok(cli::Invocation::Run(run)) => execute(
                run.tree,
                run.command.to_vec(),
                run.repository,
                run.verify,
                run.write_policy,
            ),
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
            tree_oid: None,
            pin_ref: None,
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
    fn recovery_releases_only_its_own_pin() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("repo")).unwrap();
        let oid = repo.treebuilder(None).unwrap().write().unwrap();
        let mut state = recovery();
        state.git_dir = Some(repo.path().into());
        state.tree_oid = Some(oid.to_string());
        state.pin_ref = Some("refs/git-on/unit-test".into());
        repo.reference(state.pin_ref.as_ref().unwrap(), oid, false, "test")
            .unwrap();
        release_pin(&state).unwrap();
        assert!(repo
            .find_reference(state.pin_ref.as_ref().unwrap())
            .is_err());
        state.pin_ref = Some("refs/heads/master".into());
        assert!(release_pin(&state).is_err());
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
            let status = supervise(&mut child, &mut signals, &terminal).unwrap();
            if script.starts_with("exit") {
                assert_eq!(status.code(), Some(23));
            } else {
                assert_eq!(status.signal(), Some(SIGTERM));
            }
        }
    }
    #[test]
    fn mount_escapes_are_decoded() {
        assert_eq!(
            git_on::platform::decode_mount("/tmp/a\\040b\\134c"),
            b"/tmp/a b\\c"
        );
    }
}
