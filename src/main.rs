use anyhow::{bail, Context, Result};
use git_on::{
    backend::Backend,
    filesystem::Overlay,
    git::set_foreground as foreground,
    platform::{
        create_temporary_entry, ensure_unmounted, grouped_run, mount_is_ours,
        remove_empty_temporary_group, run_identity, same_run_directory, ssh_control_path,
        usable_run_group, RUN_LOCK_NAME,
    },
    policy::WritePolicy,
    snapshot::Snapshot,
};
use serde::Deserialize;
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};
use std::{
    env,
    ffi::{CStr, CString, OsString},
    fs::{self, File, OpenOptions},
    os::fd::FromRawFd,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

mod cli;

// Legacy invocations put mount and pin identities in a separate file.
#[derive(Deserialize)]
#[cfg_attr(all(test, target_os = "linux"), derive(serde::Serialize))]
struct Recovery {
    format: String,
    id: String,
    git_dir: Option<PathBuf>,
    tree_oid: Option<String>,
    pin_ref: Option<String>,
    #[cfg(target_os = "macos")]
    nfs_socket: Option<PathBuf>,
}
#[cfg(target_os = "macos")]
impl Recovery {
    fn nfs_socket_path(&self) -> Option<PathBuf> {
        self.nfs_socket
            .clone()
            .or_else(|| git_on::platform::legacy_nfs_socket(&self.id))
    }
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn legacy_recovery_without_a_socket_record_uses_the_flat_path() {
    let id = uuid::Uuid::new_v4().to_string();
    let state: Recovery = serde_json::from_value(serde_json::json!({
        "format": "git-on-temp-v1",
        "id": id,
    }))
    .unwrap();
    assert!(state.nfs_socket.is_none());
    assert_eq!(
        state.nfs_socket_path(),
        Some(PathBuf::from(format!("/private/tmp/git-on-nfs-{id}/s")))
    );
}

const PENDING_RUN_LOCK: &CStr = c".git-on-lock-new";
const RETIRING_LOCK_PREFIX: &str = ".git-on-remove-";

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}
fn open_marker(
    directory: libc::c_int,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::c_int,
) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn open_run_lock(directory: libc::c_int, name: &CStr, owned: bool) -> Result<File> {
    let f = open_marker(directory, name, libc::O_RDWR, 0)?;
    let metadata = f.metadata()?;
    anyhow::ensure!(metadata.is_file(), "invalid recovery lock type");
    if owned {
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1
                && metadata.len() == 0,
            "invalid private run lock"
        );
    }
    lock_file(&f)?;
    Ok(f)
}
fn open_legacy_lock(dir: &Path) -> Result<File> {
    let name = CString::new(dir.join(".lock").as_os_str().as_encoded_bytes())?;
    open_run_lock(libc::AT_FDCWD, &name, false)
}
fn lock_file(file: &File) -> Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Err(error).context("run is active or locked");
    }
    Err(error).context("lock temporary run")
}
fn create_run_marker(dir: libc::c_int, path: &CStr) -> std::io::Result<File> {
    open_marker(
        dir,
        path,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )
}
fn marker_entry(dir: libc::c_int, name: &CStr) -> std::io::Result<Option<libc::stat>> {
    let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            entry.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    Ok(Some(unsafe { entry.assume_init() }))
}
fn marker_matches(dir: libc::c_int, name: &CStr, lock: &File) -> std::io::Result<bool> {
    let Some(entry) = marker_entry(dir, name)? else {
        return Ok(false);
    };
    let held = lock.metadata()?;
    #[cfg(target_os = "linux")]
    let dev = entry.st_dev;
    #[cfg(target_os = "macos")]
    let dev = entry.st_dev as u64;
    Ok(dev == held.dev() && entry.st_ino == held.ino())
}
fn new_run_lock(dir: &File, acquire: impl FnOnce(&File) -> Result<()>) -> Result<File> {
    let dir = dir.as_raw_fd();
    let path = PENDING_RUN_LOCK;
    let lock = create_run_marker(dir, path).context("create temporary run lock")?;
    let published = acquire(&lock).and_then(|()| {
        // Publish the ownership marker only after obtaining its lock.
        anyhow::ensure!(
            marker_matches(dir, path, &lock)?,
            "temporary run marker changed before publication"
        );
        let result = unsafe { libc::renameat(dir, path.as_ptr(), dir, RUN_LOCK_NAME.as_ptr()) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).context("publish temporary run lock")
        }
    });
    if let Err(error) = published {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
        {
            // A recovery process already holds the unfinished marker.
            return Err(error);
        }
        if marker_matches(dir, path, &lock)?
            && unsafe { libc::unlinkat(dir, path.as_ptr(), 0) } != 0
        {
            return Err(error).context(format!(
                "remove unfinished run lock: {}",
                std::io::Error::last_os_error()
            ));
        }
        return Err(error);
    }
    Ok(lock)
}
fn try_owned_run_lock(directory: &File, name: &CStr) -> Result<Option<File>> {
    match open_run_lock(directory.as_raw_fd(), name, true) {
        Ok(lock) => Ok(Some(lock)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

struct LockedRun {
    directory: File,
    lock: File,
    id: String,
    marker: &'static CStr,
    parked: bool,
}

fn lock_existing_run(dir: &Path) -> Result<Option<LockedRun>> {
    let Some((directory, id)) = run_identity(dir)? else {
        return Ok(None);
    };
    let group = open_removal_group(dir)?;
    let ready = retiring_lock_name(dir, &id, RUN_LOCK_NAME);
    let pending = retiring_lock_name(dir, &id, PENDING_RUN_LOCK);
    let paths = [
        (&directory, RUN_LOCK_NAME, RUN_LOCK_NAME, false),
        (&directory, PENDING_RUN_LOCK, PENDING_RUN_LOCK, false),
        (&group, ready.as_c_str(), RUN_LOCK_NAME, true),
        (&group, pending.as_c_str(), PENDING_RUN_LOCK, true),
    ];
    // The directory exists briefly before startup can acquire its lock and
    // create the pending marker. Leave a run without an ownership claim alone.
    let mut claimed = false;
    for &(parent, path, _, _) in &paths {
        if marker_entry(parent.as_raw_fd(), path)?.is_some() {
            claimed = true;
            break;
        }
    }
    if !claimed {
        return Ok(None);
    }
    lock_file(&directory)?;
    anyhow::ensure!(
        same_run_directory(dir, &directory)?,
        "temporary run changed while locking it"
    );
    for &(parent, path, marker, parked) in &paths {
        if let Some(lock) = try_owned_run_lock(parent, path)? {
            return Ok(Some(LockedRun {
                directory,
                lock,
                id,
                marker,
                parked,
            }));
        }
    }
    Ok(None)
}

fn open_removal_group(dir: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir.parent().unwrap())
        .context("open temporary run group")
}
fn retiring_lock_name(dir: &Path, id: &str, marker: &CStr) -> CString {
    let basename = dir
        .file_name()
        .unwrap()
        .to_str()
        .expect("validated run basename");
    let kind = if marker == PENDING_RUN_LOCK {
        "pending"
    } else {
        "ready"
    };
    CString::new(format!("{RETIRING_LOCK_PREFIX}{basename}-{kind}-{id}")).unwrap()
}
fn unlink_marker(directory: &File, marker: &CStr) -> std::io::Result<()> {
    if unsafe { libc::unlinkat(directory.as_raw_fd(), marker.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn park_run_marker(from: libc::c_int, marker: &CStr, to: libc::c_int, parked: &CStr) -> Result<()> {
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            from,
            marker.as_ptr(),
            to,
            parked.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from,
            marker.as_ptr(),
            to,
            parked.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if !matches!(
        error.kind(),
        std::io::ErrorKind::Unsupported
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::AlreadyExists
    ) && error.raw_os_error() != Some(libc::ENOSYS)
    {
        return Err(error.into());
    }
    park_run_marker_with_reservation(from, marker, to, parked)
}

fn park_run_marker_with_reservation(
    from: libc::c_int,
    marker: &CStr,
    to: libc::c_int,
    parked: &CStr,
) -> Result<()> {
    // If the filesystem lacks exclusive rename, reserve the destination
    // ourselves under the held directory flock. Mode zero is independent
    // of the caller's umask and identifies an interrupted reservation.
    if let Err(error) = open_marker(to, parked, libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL, 0) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let reservation = marker_entry(to, parked)?.context("parking reservation disappeared")?;
    anyhow::ensure!(
        reservation.st_mode & (libc::S_IFMT | 0o7777) == libc::S_IFREG
            && reservation.st_uid == unsafe { libc::geteuid() }
            && reservation.st_nlink == 1
            && reservation.st_size == 0,
        "parking path is not an owned reservation"
    );
    if unsafe { libc::renameat(from, marker.as_ptr(), to, parked.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

impl LockedRun {
    fn remove(&self, dir: &Path) -> Result<()> {
        self.remove_with_parking(dir, park_run_marker)
    }

    fn remove_with_parking(
        &self,
        dir: &Path,
        mut park: impl FnMut(libc::c_int, &CStr, libc::c_int, &CStr) -> Result<()>,
    ) -> Result<()> {
        let unfinished = self.marker == PENDING_RUN_LOCK;
        if unfinished {
            for entry in fs::read_dir(dir)? {
                anyhow::ensure!(
                    !self.parked && entry?.file_name().as_encoded_bytes() == self.marker.to_bytes(),
                    "unfinished run contains other files"
                );
            }
        }
        let group = open_removal_group(dir)?;
        anyhow::ensure!(
            usable_run_group(dir.parent().unwrap(), &group.metadata()?),
            "temporary run group changed during cleanup"
        );
        let parked_name = retiring_lock_name(dir, &self.id, self.marker);
        anyhow::ensure!(
            same_run_directory(dir, &self.directory)?,
            "temporary run changed during cleanup"
        );
        if !self.parked {
            anyhow::ensure!(
                marker_matches(self.directory.as_raw_fd(), self.marker, &self.lock)?,
                "temporary run marker changed during cleanup"
            );
            let mut park_marker = || {
                park(
                    self.directory.as_raw_fd(),
                    self.marker,
                    group.as_raw_fd(),
                    &parked_name,
                )
            };
            if let Err(error) = park_marker() {
                let capacity_error = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(error.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
                });
                if unfinished || !capacity_error {
                    return Err(error).context("park temporary run marker");
                }

                // Filesystems without exclusive rename may need an inode for
                // the parking reservation. Reclaim run contents while the
                // original ownership marker is still in place and locked.
                anyhow::ensure!(
                    same_run_directory(dir, &self.directory)?
                        && marker_matches(self.directory.as_raw_fd(), self.marker, &self.lock)?,
                    "temporary run or marker changed before reclaiming space"
                );
                for entry in fs::read_dir(dir)? {
                    let entry = entry?;
                    if entry.file_name().as_encoded_bytes() == self.marker.to_bytes() {
                        continue;
                    }
                    if entry.file_type()?.is_dir() {
                        fs::remove_dir_all(entry.path())?;
                    } else {
                        fs::remove_file(entry.path())?;
                    }
                }
                anyhow::ensure!(
                    same_run_directory(dir, &self.directory)?
                        && marker_matches(self.directory.as_raw_fd(), self.marker, &self.lock)?,
                    "temporary run or marker changed while reclaiming space"
                );
                park_marker().context("park temporary run marker after reclaiming space")?;
            }
        }
        anyhow::ensure!(
            same_run_directory(dir, &self.directory)?
                && marker_matches(group.as_raw_fd(), &parked_name, &self.lock)?,
            "temporary run or parked marker changed during cleanup"
        );
        if unfinished {
            fs::remove_dir(dir)?;
        } else {
            fs::remove_dir_all(dir)?;
        }
        anyhow::ensure!(
            marker_matches(group.as_raw_fd(), &parked_name, &self.lock)?,
            "parked temporary run marker changed during cleanup"
        );
        unlink_marker(&group, &parked_name).context("remove parked temporary run marker")?;
        Ok(())
    }
}
fn cleanup_orphaned_run_marker(dir: &Path) -> Result<bool> {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let parent = fs::canonicalize(
        dir.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let dir = parent.join(name);
    if !grouped_run(&dir)
        || !name
            .strip_prefix('g')
            .is_some_and(|tail| tail.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Ok(false);
    }
    let group = open_removal_group(&dir)?;
    if !usable_run_group(&parent, &group.metadata()?) {
        return Ok(false);
    }
    let prefix = format!("{RETIRING_LOCK_PREFIX}{name}-");
    let mut removed = false;
    for entry in fs::read_dir(&parent)? {
        let entry = entry?.file_name();
        let Some(identity) = entry
            .to_str()
            .and_then(|entry| entry.strip_prefix(&prefix))
            .and_then(|tail| {
                tail.strip_prefix("ready-")
                    .or_else(|| tail.strip_prefix("pending-"))
            })
        else {
            continue;
        };
        if uuid::Uuid::parse_str(identity).is_err() {
            continue;
        }
        let marker = CString::new(entry.as_encoded_bytes())?;
        if let Some(lock) = try_owned_run_lock(&group, &marker)? {
            anyhow::ensure!(
                matches!(fs::symlink_metadata(&dir), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                    && marker_matches(group.as_raw_fd(), &marker, &lock)?,
                "temporary run or marker changed during cleanup"
            );
            unlink_marker(&group, &marker)?;
            removed = true;
        }
    }
    if removed {
        remove_empty_temporary_group(&parent)?;
    }
    Ok(removed)
}
fn read_recovery(dir: &Path) -> Result<Option<Recovery>> {
    let file = match File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(".state"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open recovery record"),
    };
    anyhow::ensure!(file.metadata()?.is_file(), "invalid recovery record type");
    Ok(Some(serde_json::from_reader(file)?))
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
            bail!("mount is busy or unmount failed; recovery state preserved for --cleanup");
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
            ensure_unmounted(dir)?;
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
    let dir = match fs::canonicalize(dir) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if cleanup_orphaned_run_marker(dir)? {
                return Ok(());
            }
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    let owned = lock_existing_run(&dir)?;
    let (_legacy_lock, legacy, id) = match &owned {
        Some(locked) if locked.marker == PENDING_RUN_LOCK => {
            locked.remove(&dir)?;
            remove_empty_temporary_group(dir.parent().unwrap())?;
            return Ok(());
        }
        Some(locked) => (None, None, locked.id.clone()),
        None => {
            let lock = open_legacy_lock(&dir)?;
            let state = read_recovery(&dir)?.context("missing recovery record")?;
            anyhow::ensure!(
                state.format == "git-on-temp-v1",
                "unrecognized temporary directory"
            );
            let id = state.id.clone();
            (Some(lock), Some(state), id)
        }
    };
    #[cfg(target_os = "macos")]
    let nfs_socket = legacy.as_ref().and_then(Recovery::nfs_socket_path);
    if mount_is_ours(
        &dir,
        &id,
        #[cfg(target_os = "macos")]
        nfs_socket.as_deref(),
    )? {
        #[cfg(target_os = "linux")]
        clean_unmount(&dir.join("mount"))?;
        #[cfg(target_os = "macos")]
        unmount_dead_server(&dir.join("mount"))?;
    }
    ensure_unmounted(&dir)?;
    git_on::remote::stop_connection(&ssh_control_path(&dir))?;
    #[cfg(target_os = "macos")]
    git_on::platform::cleanup_nfs_socket(&id, nfs_socket.as_deref())?;
    git_on::materialize::prepare_cleanup(&dir.join("mount"))?;
    if let Some(legacy) = &legacy {
        release_pin(legacy)?;
    }
    if let Some(locked) = &owned {
        locked.remove(&dir)?;
    } else {
        fs::remove_dir_all(&dir)?;
    }
    if grouped_run(&dir) {
        remove_empty_temporary_group(dir.parent().unwrap())
            .context("remove temporary git-on group")?;
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
    use std::io::Write;
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
    let mut signals = subscribe_signals()?;
    #[cfg(target_os = "macos")]
    let _child_exit = UnblockedChildExit::new().context("unblock child exit notification")?;
    if let Some(signal) = git_on::git::cancelled_signal() {
        return Ok(ExitStatus::from_raw(signal));
    }

    let temporary = env::temp_dir();
    let temporary = if temporary.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &temporary
    };
    let explain_temp_error = || {
        format!(
            "cannot create a private snapshot in {}. Check the directory's write/search permissions and available space, or choose another TMPDIR",
            temporary.display()
        )
    };
    let parent = fs::canonicalize(temporary).with_context(explain_temp_error)?;
    let temp = loop {
        let entropy = uuid::Uuid::new_v4().to_string();
        let name = format!("g{}", &entropy[..5]);
        // Keep the same Unix socket path length as the previous flat layout.
        match create_temporary_entry(&parent, &name) {
            Ok(temp) => break temp,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(explain_temp_error),
        }
    };
    let group = temp.parent().unwrap();
    let initialized = (|| -> Result<_> {
        let dir = fs::canonicalize(&temp)?;
        let (directory, id) = run_identity(&dir)?.context("invalid temporary run attributes")?;
        lock_file(&directory)?;
        let lock = new_run_lock(&directory, lock_file)?;
        anyhow::ensure!(
            same_run_directory(&dir, &directory)?,
            "temporary run changed while locking it"
        );
        Ok((
            dir,
            LockedRun {
                directory,
                lock,
                id,
                marker: RUN_LOCK_NAME,
                parked: false,
            },
        ))
    })();
    let result = match initialized {
        Ok((dir, locked)) => execute_in_group(invoked, run, dir, &locked, &mut signals),
        Err(error) => {
            let _ = fs::remove_dir(&temp);
            Err(error)
        }
    };
    let cleanup = remove_empty_temporary_group(group)
        .with_context(|| format!("remove temporary git-on group {}", group.display()));
    let status = result?;
    cleanup?;
    Ok(status)
}

fn execute_in_group(
    invoked: Instant,
    run: cli::RunArgs<'_>,
    dir: PathBuf,
    locked: &LockedRun,
    signals: &mut Signals,
) -> Result<ExitStatus> {
    let id = locked.id.as_str();
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
        let outcome = outcome.and_then(|status| {
            if status.success() && git_on::git::cancelled_signal().is_none() {
                snapshot.verify_used_packs()?;
            }
            Ok(status)
        });
        // Escaped or terminated readers can leave a FUSE callback inside a
        // native fetch. Stop it before unmounting or inspecting snapshot state.
        git_on::git::stop_operations();

        let unmount = session.as_mut().map_or(Ok(()), Mounted::finish);

        match (outcome, unmount) {
            (Ok(status), Ok(())) => Ok(status),
            (Err(a), Err(b)) => bail!("{a:#}; cleanup: {b:#}"),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    })();
    if let Some(progress) = &mut progress {
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
        if mount_is_ours(
            &dir,
            id,
            #[cfg(target_os = "macos")]
            nfs_socket.as_deref(),
        )? {
            bail!(
                "temporary mount remains at {}; use --cleanup after stopping its users",
                dir.display()
            );
        }
        ensure_unmounted(&dir)?;
        // Also cover errors before an observed Snapshot could be returned.
        git_on::remote::stop_connection(&ssh_control_path(&dir))?;
        #[cfg(target_os = "macos")]
        if let Some(socket) = nfs_socket.as_deref() {
            git_on::platform::cleanup_nfs_socket(id, Some(socket))?;
        }
        git_on::materialize::prepare_cleanup(&dir.join("mount"))?;
        if dir.join("remote-repository").exists() {
            fs::remove_dir_all(dir.join("remote-repository"))?;
        }
        match fs::remove_file(dir.join(git_on::remote::SSH_CONTROL_OWNER_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove SSH recovery identity"),
        }
        locked.remove(&dir)?;
        Ok(())
    })();

    if let Err(e) = cleanup_result {
        // Keep the private object files available to a later checked cleanup.
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
fn main() {
    let invoked = Instant::now();
    if let Some(status) = git_on::terminal_ui::invoke_askpass() {
        std::process::exit(status);
    }
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let result = (|| -> Result<ExitStatus> {
        // main has not created signal handlers, worker threads or Git handles.
        unsafe { git_on::object::configure_pack_descriptors() }?;
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
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
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
        File::create(dir.join(".lock")).unwrap();
        let held = open_legacy_lock(&dir).unwrap();
        fs::write(dir.join(".state"), serde_json::to_vec(&recovery()).unwrap()).unwrap();
        assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        cleanup(&dir).unwrap();
        assert!(!dir.exists());
    }
    #[test]
    fn recovery_does_not_compete_for_a_run_without_a_marker() {
        let root = tempfile::tempdir().unwrap();
        let dir = create_temporary_entry(root.path(), "g123ab").unwrap();
        let (directory, _) = run_identity(&dir).unwrap().unwrap();
        lock_file(&directory).unwrap();
        // Startup has obtained the directory lock but has not published
        // even a pending marker. Recovery has no ownership claim yet.
        assert!(lock_existing_run(&dir).unwrap().is_none());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }
    #[test]
    fn portable_marker_parking_retries_an_owned_reservation() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = create_temporary_entry(root.path(), "g123ab").unwrap();
        let (directory, id) = run_identity(&dir).unwrap().unwrap();
        lock_file(&directory).unwrap();
        let lock = new_run_lock(&directory, lock_file).unwrap();
        let group = open_removal_group(&dir).unwrap();
        let parked = retiring_lock_name(&dir, &id, RUN_LOCK_NAME);
        let path = dir.parent().unwrap().join(parked.to_str().unwrap());
        let from = directory.as_raw_fd();
        let to = group.as_raw_fd();

        fs::write(&path, b"fixture content").unwrap();
        assert!(park_run_marker_with_reservation(from, RUN_LOCK_NAME, to, &parked).is_err());
        assert!(marker_matches(from, RUN_LOCK_NAME, &lock).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"fixture content");
        fs::remove_file(&path).unwrap();
        let reservation = open_marker(
            to,
            &parked,
            libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL,
            0,
        )
        .unwrap();
        assert_eq!(reservation.metadata().unwrap().mode() & 0o777, 0);
        drop(reservation);

        park_run_marker(from, RUN_LOCK_NAME, to, &parked).unwrap();
        assert!(marker_matches(to, &parked, &lock).unwrap());
        assert!(!marker_matches(from, RUN_LOCK_NAME, &lock).unwrap());
        drop((directory, group, lock));
        cleanup(&dir).unwrap();
        assert!(!dir.exists());
        assert!(!path.exists());
    }

    #[test]
    fn removal_reclaims_inodes_before_retrying_marker_parking() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = create_temporary_entry(root.path(), "g123ab").unwrap();
        let (directory, id) = run_identity(&dir).unwrap().unwrap();
        lock_file(&directory).unwrap();
        let run = LockedRun {
            lock: new_run_lock(&directory, lock_file).unwrap(),
            directory,
            id,
            marker: RUN_LOCK_NAME,
            parked: false,
        };
        let contents = dir.join("files");
        fs::create_dir(&contents).unwrap();
        fs::write(contents.join("checkout"), b"temporary contents").unwrap();
        let mut attempts = 0;
        run.remove_with_parking(&dir, |from, marker, to, parked| {
            attempts += 1;
            assert!(marker_matches(from, marker, &run.lock).unwrap());
            if contents.exists() {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
            }
            park_run_marker_with_reservation(from, marker, to, parked)
        })
        .unwrap();
        assert_eq!(attempts, 2);
        assert!(!dir.exists());
        assert_eq!(fs::read_dir(dir.parent().unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn new_run_is_recoverable_only_once_its_lock_is_held() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = create_temporary_entry(root.path(), "g123ab").unwrap();
        let (directory, _) = run_identity(&dir).unwrap().unwrap();
        assert!(cleanup(&dir).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

        let failed = new_run_lock(&directory, |_| {
            Err(std::io::Error::from_raw_os_error(libc::ENOLCK).into())
        })
        .unwrap_err();
        assert_eq!(
            failed
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOLCK)
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

        let held = new_run_lock(&directory, lock_file).unwrap();
        assert!(cleanup(&dir).unwrap_err().to_string().contains("active"));
        drop(held);
        cleanup(&dir).unwrap();
        assert!(!dir.exists());
    }
    #[test]
    fn startup_cannot_publish_a_replacement_pending_marker() {
        let root = tempfile::tempdir().unwrap();
        let dir = create_temporary_entry(root.path(), "g123ab").unwrap();
        let (directory, _) = run_identity(&dir).unwrap().unwrap();
        let replaced = new_run_lock(&directory, |original| {
            unlink_marker(&directory, PENDING_RUN_LOCK)?;
            let mut replacement = create_run_marker(directory.as_raw_fd(), PENDING_RUN_LOCK)?;
            use std::io::Write;
            replacement.write_all(b"replacement")?;
            lock_file(original)
        })
        .unwrap_err();
        assert!(format!("{replaced:#}").contains("marker changed"));
        assert_eq!(
            fs::read(dir.join(".git-on-lock-new")).unwrap(),
            b"replacement"
        );
        assert!(!dir.join(".git-on-lock-v2").exists());
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
