use anyhow::{bail, Context, Result};
use git_on::{filesystem::Overlay, snapshot::Snapshot};
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
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

mod cli;

// Ephemeral recovery information only: no invocation history, arguments or logs.
#[derive(Serialize, Deserialize)]
struct Recovery {
    format: String,
    id: String,
    keep: bool,
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
fn mount_is_ours(dir: &Path, id: &str) -> Result<bool> {
    let p = dir.join("mount");
    let source = format!("git-on:{id}");
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let mut found = false;
    for line in mounts.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() > 6 && decode_mount(fields[4]) == p.as_os_str().as_bytes() {
            let sep = fields
                .iter()
                .position(|x| *x == "-")
                .context("invalid mountinfo")?;
            if fields.get(sep + 2) == Some(&source.as_str()) {
                if found {
                    bail!("stacked mounts at the temporary mountpoint");
                }
                found = true;
                continue;
            }
            bail!("mountpoint is occupied by a different filesystem");
        }
    }
    Ok(found)
}
fn ensure_unmounted(dir: &Path) -> Result<()> {
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        if let Some(point) = line.split_whitespace().nth(4) {
            let bytes = decode_mount(point);
            let path = Path::new(std::ffi::OsStr::from_bytes(&bytes));
            if path.starts_with(dir) {
                bail!("a mount remains below {}; refusing deletion", dir.display());
            }
        }
    }
    Ok(())
}
fn decode_mount(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|v| (b'0'..=b'7').contains(v))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + b[i + 3] - b'0');
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
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
    let start = Instant::now();
    loop {
        let mut child = Command::new("fusermount3")
            .args(["-u", "--"])
            .arg(mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start clean unmount")?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() > Duration::from_secs(3) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("clean unmount timed out; run --cleanup after stopping mount users");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
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
    session: Option<fuser::BackgroundSession>,
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
            if let Err(error) = result {
                // Do not invoke fuser's lazy-unmount fallback on a busy mount.
                // Process exit closes the FUSE connection; the persistent pin
                // and run artifacts allow explicit, checked stale cleanup.
                std::mem::forget(session);
                return Err(error);
            }
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.join())).is_err() {
                bail!("FUSE session failed during unmount");
            }
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
    Ok(unsafe { info.si_pid() } != 0)
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
    if unsafe { info.si_pid() } != 0 {
        let stop = unsafe { info.si_status() };
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
    if state.keep {
        bail!("kept temporary files must be removed explicitly by their owner");
    }
    if mount_is_ours(&dir, &state.id)? {
        clean_unmount(&dir.join("mount"))?;
    }
    ensure_unmounted(&dir)?;
    release_pin(&state)?;
    git_on::remote::stop_connection(&dir.join("ssh-control"));
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
    keep: bool,
    source: Option<OsString>,
    verify: bool,
) -> Result<ExitStatus> {
    let temp = tempfile::Builder::new()
        .prefix("git-on-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let dir = fs::canonicalize(temp.path())?;
    let held_lock = lock(&dir, true)?;
    let _retained = temp.keep();
    let id = uuid::Uuid::new_v4().to_string();
    let mut state = Recovery {
        format: "git-on-temp-v1".into(),
        id: id.clone(),
        keep,
        git_dir: None,
        tree_oid: None,
        pin_ref: None,
    };
    let _lock = held_lock;
    let mut observed = None;
    let mut kept_export = None;
    let mut result = (|| -> Result<ExitStatus> {
        #[cfg(feature = "test-metrics")]
        if let Some(fd) = env::var_os("GIT_ON_TEST_METRICS_FD") {
            let fd: i32 = fd.to_str().context("invalid metrics fd")?.parse()?;
            if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                bail!("invalid test metrics descriptor");
            }
        }
        save(&dir, &state)?;
        let mut signals = Signals::new([
            SIGINT,
            SIGTERM,
            SIGHUP,
            SIGQUIT,
            libc::SIGCHLD,
            libc::SIGCONT,
        ])?;
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
        private_dir(&dir.join("mount"))?;
        private_dir(&dir.join("backing"))?;
        let mut overlay = Overlay::new(snapshot, dir.join("backing"))?;
        if keep {
            kept_export = Some(overlay.enable_keep_export(dir.join("kept")));
        }
        if let Some(signal) = signals
            .pending()
            .find(|signal| matches!(*signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT))
        {
            return Ok(ExitStatus::from_raw(signal));
        }
        let session = fuser::spawn_mount2(
            overlay,
            dir.join("mount"),
            &[
                fuser::MountOption::FSName(format!("git-on:{id}")),
                fuser::MountOption::Subtype("git-on".into()),
                fuser::MountOption::NoSuid,
                fuser::MountOption::NoDev,
                fuser::MountOption::DefaultPermissions,
            ],
        )
        .context("mount snapshot (Linux /dev/fuse and fusermount3 required)")?;
        let mut session = Mounted {
            session: Some(session),
            mountpoint: dir.join("mount"),
            id: id.clone(),
        };
        if !mount_is_ours(&dir, &id)? {
            bail!("FUSE mount did not become ready");
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
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                if transfer {
                    let _ = foreground(tty_fd.unwrap(), unsafe { libc::getpgrp() });
                }
                return Err(e).context("spawn command");
            }
        };
        let pid = child.id();
        terminal.child = Some(pid as libc::pid_t);
        let outcome = supervise(&mut child, &mut signals, &terminal);
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
        let unmount = session.finish();
        match (outcome, unmount) {
            (Ok(status), Ok(())) => Ok(status),
            (Err(a), Err(b)) => bail!("{a:#}; cleanup: {b:#}"),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    })();
    // Also cover errors before the child could start. State inspection below
    // must never wait behind an owned operation that the run no longer needs.
    git_on::git::stop_operations();
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
        // On a busy mount retain the pin for explicit stale recovery.
        if mount_is_ours(&dir, &id).unwrap_or(true) {
            snapshot.stop_remote_transport();
            std::mem::forget(snapshot);
        }
    }
    let cleanup_result = (|| -> Result<()> {
        if mount_is_ours(&dir, &id)? {
            bail!(
                "temporary mount remains at {}; use --cleanup after stopping its users",
                dir.display()
            );
        }
        ensure_unmounted(&dir)?;
        release_pin(&state)?;
        if dir.join("remote-repository").exists() {
            fs::remove_dir_all(dir.join("remote-repository"))?;
        }
        match fs::remove_file(dir.join(git_on::remote::SSH_CONTROL_OWNER_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove SSH recovery identity"),
        }
        if keep {
            if let Some(export) = kept_export.as_ref() {
                match export
                    .lock()
                    .map_err(|_| anyhow::anyhow!("keep export lock poisoned"))?
                    .as_ref()
                {
                    Some(Ok(())) => {
                        fs::remove_dir_all(dir.join("backing"))?;
                        fs::rename(dir.join("kept"), dir.join("backing"))?;
                    }
                    Some(Err(e)) => bail!(
                        "cannot export kept files: {e}; raw backing retained at {}",
                        dir.display()
                    ),
                    None => bail!(
                        "keep export did not complete; raw backing retained at {}",
                        dir.display()
                    ),
                }
            }
            if dir.join("mount").exists() {
                fs::remove_dir(dir.join("mount"))?;
            }
            eprintln!("git-on: kept temporary files at {}", dir.display());
        } else {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    })();
    if let Err(e) = cleanup_result {
        result = Err(match result {
            Ok(_) => e,
            Err(a) => anyhow::anyhow!("{a:#}; cleanup: {e:#}"),
        });
    }
    result
}
fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let result = (|| -> Result<ExitStatus> {
        if args.first().is_some_and(|a| a == "--help" || a == "-h") {
            println!("Usage: {}\n       git on --cleanup <temporary-directory>\n\nRun argv with inherited standard I/O on a lazy immutable Git snapshot.\nOne operand selects local REV; two select repository and REV.\nNew paths must be ignored by the snapshot's .gitignore. Linux/FUSE3 required.\nTemporary backing uses the system temporary directory (TMPDIR).\n--keep, -k retain generated files for debugging after unmount.\n--verify delegates exact commit/tag signature verification to native Git.\nRunner options follow REV and precede the command boundary.\nThe first -- is required before every command; all following argv is literal.\nGIT_ON_TREE_OID identifies the pinned source tree.\nExit: child status; runner failure 125; usage 2.", cli::RUN_USAGE);
            return Ok(ExitStatus::from_raw(0));
        }
        if args.first().is_some_and(|a| a == "--version") {
            println!("git-on {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitStatus::from_raw(0));
        }
        if args.first().is_some_and(|a| a == "--cleanup") {
            if args.len() != 2 {
                eprintln!("usage: git on --cleanup <temporary-directory>");
                return Ok(ExitStatus::from_raw(2 << 8));
            }
            cleanup(Path::new(&args[1]))?;
            return Ok(ExitStatus::from_raw(0));
        }
        match cli::parse_run_args(&args) {
            Ok(run) => execute(
                run.tree.clone(),
                run.command.to_vec(),
                run.keep,
                run.repository.cloned(),
                run.verify,
            ),
            Err(error) => {
                eprintln!("git-on: {error}");
                eprintln!("usage: {}", cli::RUN_USAGE);
                Ok(ExitStatus::from_raw(2 << 8))
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

#[cfg(test)]
mod tests {
    use super::*;
    // Fork briefly inherits other threads' flock descriptions before exec.
    // Keep process-spawning and lock-release assertions in separate test epochs.
    static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn recovery(keep: bool) -> Recovery {
        Recovery {
            format: "git-on-temp-v1".into(),
            id: "unit-test".into(),
            keep,
            git_dir: None,
            tree_oid: None,
            pin_ref: None,
        }
    }
    #[test]
    fn recovery_refuses_active_and_kept_data() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("temp");
        private_dir(&dir).unwrap();
        let held = lock(&dir, true).unwrap();
        save(&dir, &recovery(false)).unwrap();
        assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        save(&dir, &recovery(true)).unwrap();
        assert!(cleanup(&dir).unwrap_err().to_string().contains("kept"));
        assert!(dir.exists());
        save(&dir, &recovery(false)).unwrap();
        cleanup(&dir).unwrap();
        assert!(!dir.exists());
    }
    #[test]
    fn recovery_releases_only_its_own_pin() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("repo")).unwrap();
        let oid = repo.treebuilder(None).unwrap().write().unwrap();
        let mut state = recovery(false);
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
        assert_eq!(decode_mount("/tmp/a\\040b\\134c"), b"/tmp/a b\\c");
    }
}
