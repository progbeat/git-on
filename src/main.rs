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
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Default, Serialize, Deserialize)]
struct Status {
    exit_code: Option<i32>,
    signal: Option<i32>,
    runner_error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    run_id: String,
    tree_oid: Option<String>,
    argv: Vec<String>,
    argv_hex: Vec<String>,
    started_at_ms: u128,
    finished_at_ms: Option<u128>,
    phase: String,
    status: Status,
    logs_complete: bool,
    git_dir: Option<PathBuf>,
    pin_ref: Option<String>,
    #[serde(default)]
    snapshot_metrics: Option<serde_json::Value>,
}
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create fresh directory {}", path.display()))
}
fn file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}
fn save(dir: &Path, m: &Manifest) -> Result<()> {
    let tmp = dir.join(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = file(&tmp)?;
        serde_json::to_writer_pretty(&mut f, m)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join("manifest.json"))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
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
fn run_dir(id: &str) -> Result<PathBuf> {
    let p = if let Some(p) = env::var_os("GIT_ON_RUN_DIR") {
        PathBuf::from(p)
    } else {
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .context("set HOME, XDG_DATA_HOME, or GIT_ON_RUN_DIR")?;
        let parent = data.join("git-on/runs");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)?;
        parent.join(id)
    };
    private_dir(&p)?;
    Ok(fs::canonicalize(p)?)
}
fn nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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
    for line in mounts.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() > 6 && decode_mount(fields[4]) == p.as_os_str().as_bytes() {
            let sep = fields
                .iter()
                .position(|x| *x == "-")
                .context("invalid mountinfo")?;
            if fields.get(sep + 2) == Some(&source.as_str()) {
                return Ok(true);
            }
            bail!("mountpoint is occupied by a different filesystem");
        }
    }
    Ok(false)
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
fn release_pin(m: &Manifest) -> Result<()> {
    if let (Some(repo), Some(pin), Some(oid)) = (&m.git_dir, &m.pin_ref, &m.tree_oid) {
        if pin != &format!("refs/git-on/{}", m.run_id) {
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
fn cleanup(dir: &Path) -> Result<()> {
    let dir = fs::canonicalize(dir)?;
    let _lock = lock(&dir, false)?;
    let m: Manifest = serde_json::from_reader(File::open(dir.join("manifest.json"))?)?;
    if m.format != "git-on-run-v1" {
        bail!("unrecognized run format");
    }
    if mount_is_ours(&dir, &m.run_id)? {
        clean_unmount(&dir.join("mount"))?;
    }
    release_pin(&m)?;
    for name in ["mount", "backing"] {
        let p = dir.join(name);
        if p.exists() {
            if fs::symlink_metadata(&p)?.file_type().is_symlink() {
                bail!("unexpected symlink at {}", p.display());
            }
            fs::remove_dir_all(p)?;
        }
    }
    Ok(())
}
// tcsetpgrp may send SIGTTOU to a background caller; temporarily block it.
fn foreground(pgid: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let rc = libc::tcsetpgrp(0, pgid);
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if rc != 0 {
            return Err(error);
        }
    }
    Ok(())
}
struct Terminal {
    parent: Option<libc::pid_t>,
    child: Option<libc::pid_t>,
}
impl Terminal {
    fn capture() -> Self {
        let pgid = unsafe { libc::tcgetpgrp(0) };
        Self {
            parent: (pgid >= 0 && pgid == unsafe { libc::getpgrp() }).then_some(pgid),
            child: None,
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(pgid) = self.parent {
            let current = unsafe { libc::tcgetpgrp(0) };
            if self.child.is_none_or(|child| current == child) {
                let _ = foreground(pgid);
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
}
impl Mounted {
    fn finish(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            let result = clean_unmount(&self.mountpoint);
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
fn terminal_write(fd: i32, bytes: &[u8]) -> std::io::Result<usize> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let count = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if count < 0 {
            Err(error)
        } else {
            Ok(count as usize)
        }
    }
}
fn relay_job_control(pid: u32, parent_pgid: Option<libc::pid_t>) -> Result<()> {
    let Some(parent_pgid) = parent_pgid else {
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
    if unsafe { info.si_pid() } != 0 {
        let current = unsafe { libc::tcgetpgrp(0) };
        if current == pid as libc::pid_t {
            foreground(parent_pgid)?;
        }
        unsafe {
            libc::kill(-parent_pgid, libc::SIGSTOP);
        }
        // `fg` gives this runner the terminal; `bg` does not. Never steal it
        // from the invoking shell on a background resume.
        if unsafe { libc::tcgetpgrp(0) } == parent_pgid {
            foreground(pid as libc::pid_t)?;
        }
        kill_group(pid, libc::SIGCONT);
    }
    Ok(())
}
fn tee_write(fd: i32, mut bytes: &[u8], signals: &mut Signals, pid: u32) -> Result<()> {
    let start = Instant::now();
    while !bytes.is_empty() {
        for signal in signals.pending() {
            kill_group(pid, signal);
        }
        let remaining = Duration::from_secs(1).saturating_sub(start.elapsed());
        if remaining.is_zero() {
            bail!("console tee blocked; raw log continues");
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
        if rc < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if rc <= 0 || poll.revents & libc::POLLOUT == 0 {
            bail!("console tee failed or blocked; raw log continues");
        }
        match terminal_write(fd, bytes) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("console tee write failed; raw log continues"),
            Ok(0) => bail!("console tee write made no progress; raw log continues"),
            Ok(count) => bytes = &bytes[count..],
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn supervise(
    child: &mut std::process::Child,
    out: File,
    err: File,
    signals: &mut Signals,
    dir: &Path,
    manifest: &mut Manifest,
    tee_fds: [i32; 2],
    terminal_pgid: Option<libc::pid_t>,
) -> Result<()> {
    let pid = child.id();
    save(dir, manifest)?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    let mut logs = [out, err];
    let readers: [&mut dyn Read; 2] = [&mut stdout, &mut stderr];
    let mut eof = [false, false];
    let mut exited = false;
    let mut dead_at = None;
    let mut errors = Vec::new();
    let mut tee = [true, true];
    loop {
        for signal in signals.pending() {
            kill_group(pid, signal);
        }
        if !exited {
            relay_job_control(pid, terminal_pgid)?;
        }
        if !exited && child_exited(pid)? {
            exited = true;
            dead_at = Some(Instant::now());
            kill_group(pid, SIGTERM);
        }
        if let Some(t) = dead_at {
            if t.elapsed() > Duration::from_millis(500) {
                kill_group(pid, libc::SIGKILL);
            }
            if t.elapsed() > Duration::from_secs(2) {
                if !eof.iter().all(|x| *x) {
                    errors.push("log pipes remained open after child shutdown".into());
                }
                break;
            }
        }
        for i in 0..2 {
            if eof[i] {
                continue;
            }
            let mut buf = [0u8; 4096];
            for _ in 0..64 {
                match readers[i].read(&mut buf) {
                    Ok(0) => {
                        eof[i] = true;
                        break;
                    }
                    Ok(n) => {
                        if let Err(e) = logs[i].write_all(&buf[..n]) {
                            errors.push(format!("log write failed: {e}"));
                            eof[i] = true;
                            kill_group(pid, libc::SIGKILL);
                            break;
                        }
                        if tee[i] {
                            if let Err(e) = tee_write(tee_fds[i], &buf[..n], signals, pid) {
                                errors.push(e.to_string());
                                tee[i] = false;
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        errors.push(format!("log read failed: {e}"));
                        eof[i] = true;
                        kill_group(pid, libc::SIGKILL);
                        break;
                    }
                }
            }
        }
        if exited && eof.iter().all(|x| *x) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    kill_group(pid, libc::SIGKILL);
    let status = child.wait()?;
    manifest.status.exit_code = status.code();
    manifest.status.signal = status.signal();
    manifest.logs_complete =
        eof.iter().all(|x| *x) && !errors.iter().any(|e| e.starts_with("log "));
    for f in &logs {
        if let Err(e) = f.sync_all() {
            errors.push(format!("log sync failed: {e}"));
            manifest.logs_complete = false;
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}
fn execute(tree: OsString, args: Vec<OsString>) -> Result<i32> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = run_dir(&id)?;
    let _lock = lock(&dir, true)?;
    let mut manifest = Manifest {
        format: "git-on-run-v1".into(),
        run_id: id.clone(),
        tree_oid: None,
        argv: args.iter().map(|a| a.to_string_lossy().into()).collect(),
        argv_hex: args
            .iter()
            .map(|a| a.as_bytes().iter().map(|b| format!("{b:02x}")).collect())
            .collect(),
        started_at_ms: now(),
        finished_at_ms: None,
        phase: "preparing".into(),
        status: Status::default(),
        logs_complete: false,
        git_dir: None,
        pin_ref: None,
        snapshot_metrics: None,
    };
    save(&dir, &manifest)?;
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    let mut observed_snapshot = None;
    let mut result = (|| -> Result<i32> {
        let snapshot = Snapshot::load_with_pin_record(
            &env::current_dir()?,
            &tree,
            &id,
            |git_dir, pin, oid| {
                manifest.tree_oid = Some(oid.to_string());
                manifest.git_dir = Some(git_dir.to_path_buf());
                manifest.pin_ref = Some(pin.to_owned());
                save(&dir, &manifest)
            },
        )?;
        manifest.tree_oid = Some(snapshot.tree_oid.to_string());
        manifest.git_dir = Some(snapshot.git_dir.clone());
        manifest.pin_ref = Some(snapshot.pin_ref.clone());
        save(&dir, &manifest)?;
        private_dir(&dir.join("mount"))?;
        private_dir(&dir.join("backing"))?;
        private_dir(&dir.join("output"))?;
        let out = file(&dir.join("stdout.log"))?;
        let err = file(&dir.join("stderr.log"))?;
        observed_snapshot = Some(snapshot.clone());
        let overlay = Overlay::new(snapshot, dir.join("backing"))?;
        if let Some(signal) = signals.pending().next() {
            bail!("cancelled by signal {signal} before command execution");
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
        };
        if !mount_is_ours(&dir, &id)? {
            bail!("FUSE mount did not become ready");
        }
        if let Some(signal) = signals.pending().next() {
            bail!("cancelled by signal {signal} before command execution");
        }
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .current_dir(dir.join("mount"))
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_ON_RUN_ID", &id)
            .env("GIT_ON_TREE_OID", manifest.tree_oid.as_ref().unwrap())
            .env("GIT_ON_RUN_DIR", &dir)
            .env("GIT_ON_OUTPUT_DIR", dir.join("output"));
        let mut terminal = Terminal::capture();
        let transfer_terminal = terminal.parent.is_some();
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if transfer_terminal {
                    foreground(libc::getpgrp())?;
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("spawn command")?;
        let pid = child.id();
        terminal.child = Some(pid as libc::pid_t);
        manifest.phase = "running".into();
        let outcome = supervise(
            &mut child,
            out,
            err,
            &mut signals,
            &dir,
            &mut manifest,
            [1, 2],
            terminal.parent,
        );
        if manifest.status.exit_code.is_none() && manifest.status.signal.is_none() {
            kill_group(pid, libc::SIGKILL);
            if let Ok(status) = child.wait() {
                manifest.status.exit_code = status.code();
                manifest.status.signal = status.signal();
            }
        }
        drop(terminal);
        let unmount = session.finish();
        match (outcome, unmount) {
            (Err(a), Err(b)) => bail!("{a:#}; cleanup: {b:#}"),
            (Err(e), _) | (_, Err(e)) => return Err(e),
            _ => {}
        }
        Ok(manifest
            .status
            .exit_code
            .unwrap_or_else(|| 128 + manifest.status.signal.unwrap_or(0)))
    })();
    if let Some(snapshot) = observed_snapshot.take() {
        manifest.snapshot_metrics = Some(serde_json::to_value(snapshot.metrics())?);
        drop(snapshot);
    }
    let cleanup_result = (|| -> Result<()> {
        if !mount_is_ours(&dir, &id)? {
            release_pin(&manifest)?;
            for name in ["mount", "backing"] {
                let p = dir.join(name);
                if p.exists() {
                    fs::remove_dir_all(p)?;
                }
            }
        }
        Ok(())
    })();
    if let Err(cleanup_error) = cleanup_result {
        result = Err(match result {
            Ok(_) => cleanup_error,
            Err(error) => anyhow::anyhow!("{error:#}; cleanup: {cleanup_error:#}"),
        });
    }
    if let Err(e) = &result {
        manifest.status.runner_error = Some(format!("{e:#}"));
    }
    manifest.phase = "finished".into();
    manifest.finished_at_ms = Some(now());
    save(&dir, &manifest)?;
    eprintln!("git-on: run {} ({})", id, dir.display());
    result
}
fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let result = (|| -> Result<i32> {
        if args.first().is_some_and(|a| a == "--help" || a == "-h") {
            println!("Usage: git on [--] <tree-ish> <command> [args...]\n       git on --cleanup <run-directory>\n\nRun argv directly in an immutable Git snapshot. New paths must be ignored by\nthe snapshot's .gitignore files. Linux with FUSE3 required.\nGIT_ON_RUN_DIR selects a fresh evidence directory; otherwise XDG data is used.\nExit: child status (128+signal); runner failure 125; usage 2.");
            return Ok(0);
        }
        if args.first().is_some_and(|a| a == "--version") {
            println!("git-on {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        if args.first().is_some_and(|a| a == "--cleanup") {
            if args.len() != 2 {
                bail!("usage: git on --cleanup <run-directory>");
            }
            cleanup(Path::new(&args[1]))?;
            return Ok(0);
        }
        let args = if args.first().is_some_and(|a| a == "--") {
            &args[1..]
        } else {
            &args[..]
        };
        if args.len() < 2 {
            eprintln!("usage: git on <tree-ish> <command> [args...]");
            return Ok(2);
        }
        execute(args[0].clone(), args[1..].to_vec())
    })();
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("git-on: {e:#}");
            std::process::exit(125);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        Manifest {
            format: "git-on-run-v1".into(),
            run_id: "test".into(),
            tree_oid: None,
            argv: vec![],
            argv_hex: vec![],
            started_at_ms: now(),
            finished_at_ms: None,
            phase: "running".into(),
            status: Status::default(),
            logs_complete: false,
            git_dir: None,
            pin_ref: None,
            snapshot_metrics: None,
        }
    }
    fn child(script: &str) -> std::process::Child {
        let mut command = Command::new("python3");
        command
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    }
    fn capture(script: &str, broken_log: bool) -> (tempfile::TempDir, Manifest, Result<()>) {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manifest();
        let mut c = child(script);
        let out = if broken_log {
            OpenOptions::new().write(true).open("/dev/full").unwrap()
        } else {
            file(&dir.path().join("stdout.log")).unwrap()
        };
        let err = file(&dir.path().join("stderr.log")).unwrap();
        let tee = OpenOptions::new().write(true).open("/dev/null").unwrap();
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT]).unwrap();
        let result = supervise(
            &mut c,
            out,
            err,
            &mut signals,
            dir.path(),
            &mut m,
            [tee.as_raw_fd(), tee.as_raw_fd()],
            None,
        );
        (dir, m, result)
    }
    #[test]
    fn captures_separate_raw_streams_and_exit() {
        let (dir, m, result) = capture(
            "import os,sys;os.write(1,b'out\\x00\\xff');os.write(2,b'err\\x00\\xfe');sys.exit(7)",
            false,
        );
        result.unwrap();
        assert_eq!(m.status.exit_code, Some(7));
        assert!(m.logs_complete);
        assert_eq!(
            fs::read(dir.path().join("stdout.log")).unwrap(),
            b"out\x00\xff"
        );
        assert_eq!(
            fs::read(dir.path().join("stderr.log")).unwrap(),
            b"err\x00\xfe"
        );
    }
    #[test]
    fn captures_large_streams_without_deadlock() {
        let (dir, m, result) = capture(
            "import os;[(os.write(1,b'o'*4096),os.write(2,b'e'*4096)) for _ in range(512)]",
            false,
        );
        result.unwrap();
        assert!(m.logs_complete);
        assert_eq!(
            fs::metadata(dir.path().join("stdout.log")).unwrap().len(),
            2097152
        );
        assert_eq!(
            fs::metadata(dir.path().join("stderr.log")).unwrap().len(),
            2097152
        );
    }
    #[test]
    fn child_signal_is_not_runner_failure() {
        let (_, m, result) = capture(
            "import os,signal;os.kill(os.getpid(),signal.SIGTERM)",
            false,
        );
        result.unwrap();
        assert_eq!(m.status.signal, Some(SIGTERM));
        assert_eq!(m.status.exit_code, None);
    }
    #[test]
    fn log_failure_is_incomplete_and_stops_child() {
        let start = Instant::now();
        let (_, m, result) = capture("import os,time;os.write(1,b'hello');time.sleep(60)", true);
        assert!(result.unwrap_err().to_string().contains("log write failed"));
        assert!(!m.logs_complete);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(m.status.signal, Some(libc::SIGKILL));
    }
    #[test]
    fn escaped_inherited_pipes_have_bounded_drain() {
        let start = Instant::now();
        let (_, m, result) = capture(
            "import os,time\nr,w=os.pipe()\np=os.fork()\nif p==0:\n os.close(r);os.setsid();os.write(w,b'x');os.close(w);time.sleep(3);os._exit(0)\nos.close(w);assert os.read(r,1)==b'x';os.close(r);os._exit(0)",
            false,
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("log pipes remained open"));
        assert!(!m.logs_complete);
        assert_eq!(m.status.exit_code, Some(0));
        assert!(start.elapsed() < Duration::from_secs(4));
    }
    #[test]
    fn atomic_manifest_and_mount_escape_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manifest();
        save(dir.path(), &m).unwrap();
        m.phase = "finished".into();
        save(dir.path(), &m).unwrap();
        let loaded: Manifest =
            serde_json::from_reader(File::open(dir.path().join("manifest.json")).unwrap()).unwrap();
        assert_eq!(loaded.phase, "finished");
        assert_eq!(decode_mount("/tmp/a\\040b\\134c"), b"/tmp/a b\\c");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn cleanup_releases_pin_preserves_evidence_and_refuses_active_lock() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("repo")).unwrap();
        let oid = repo.treebuilder(None).unwrap().write().unwrap();
        repo.reference("refs/git-on/test", oid, false, "test")
            .unwrap();
        let dir = root.path().join("run");
        private_dir(&dir).unwrap();
        let held = lock(&dir, true).unwrap();
        let mut m = manifest();
        m.git_dir = Some(repo.path().into());
        m.pin_ref = Some("refs/git-on/test".into());
        m.tree_oid = Some(oid.to_string());
        save(&dir, &m).unwrap();
        for name in ["mount", "backing", "output"] {
            private_dir(&dir.join(name)).unwrap();
        }
        fs::write(dir.join("output/keep"), b"output").unwrap();
        fs::write(dir.join("stdout.log"), b"evidence").unwrap();
        assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
        // Other parallel process tests may briefly inherit this descriptor
        // between fork and exec (CLOEXEC). Explicitly release the shared OFD
        // lock before asserting reacquisition, instead of racing that window.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        cleanup(&dir).unwrap();
        cleanup(&dir).unwrap();
        assert!(repo.find_reference("refs/git-on/test").is_err());
        assert!(!dir.join("mount").exists());
        assert!(!dir.join("backing").exists());
        assert_eq!(fs::read(dir.join("stdout.log")).unwrap(), b"evidence");
        assert_eq!(fs::read(dir.join("output/keep")).unwrap(), b"output");
        assert!(dir.join("manifest.json").exists());
    }
    #[test]
    fn terminal_stop_probe_does_not_misclassify_exited_child() {
        let mut c = child("pass");
        while !child_exited(c.id()).unwrap() {
            std::thread::sleep(Duration::from_millis(1));
        }
        relay_job_control(c.id(), Some(unsafe { libc::getpgrp() })).unwrap();
        assert!(c.wait().unwrap().success());
    }
}
