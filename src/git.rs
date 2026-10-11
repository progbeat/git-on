//! Native Git configuration and signature policy. No replacement objects.
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug)]
pub struct Git {
    pub cwd: PathBuf,
    config: Vec<(OsString, Option<OsString>)>,
    original_dir: Option<PathBuf>,
}
impl Git {
    pub fn capture(cwd: &Path) -> Result<Self> {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(["config", "--null", "--list", "--includes"])
            .output()
            .context("read effective Git configuration")?;
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
            original_dir: Command::new("git")
                .current_dir(cwd)
                .args(["rev-parse", "--absolute-git-dir"])
                .output()
                .ok()
                .filter(|x| x.status.success())
                .map(|x| {
                    PathBuf::from(OsString::from_vec(
                        x.stdout.strip_suffix(b"\n").unwrap_or(&x.stdout).to_vec(),
                    ))
                }),
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
static CANCELLED: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicI32>> =
    std::sync::OnceLock::new();
pub fn cancelled_signal() -> Option<i32> {
    let signal = CANCELLED
        .get()
        .map(|x| x.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);
    (signal != 0).then_some(signal)
}
/// Stop owned native operations when the foreground command is interrupted.
pub fn cancel_by_signal(signal: i32) {
    if matches!(
        signal,
        libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
    ) {
        if let Some(value) = CANCELLED.get() {
            value.store(signal, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
fn cancellation() -> Result<()> {
    if CANCELLED.get().is_none() {
        let value = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let value = CANCELLED.get_or_init(|| value).clone();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            let value = value.clone();
            unsafe {
                signal_hook::low_level::register(signal, move || {
                    value.store(signal, std::sync::atomic::Ordering::Relaxed);
                })?;
            }
        }
    }
    Ok(())
}
/// Wait for owned transport/verifier groups without trapping cancellation in
/// blocking pipe reads. Temporary capture files have no application quota.
pub fn run(mut command: Command, capture: bool) -> Result<std::process::Output> {
    use std::{
        io::{Read, Seek},
        os::unix::{io::AsRawFd, process::CommandExt},
        time::Duration,
    };
    cancellation()?;
    if let Some(signal) = cancelled_signal() {
        bail!("Git operation cancelled by signal {signal}");
    }
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
        std::thread::sleep(Duration::from_millis(10));
    };
    if transfer {
        let _ = set_foreground(tty_fd.unwrap(), parent_group);
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
