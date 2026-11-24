use anyhow::{bail, Context, Result};
use git_on::platform::{
    create_temporary_entry_with, ensure_unmounted, grouped_run, make_temporary_directory,
    mount_is_ours, remove_empty_temporary_group, reopen_temporary_group, run_directory_matches,
    run_identity, run_ids, same_run_directory, ssh_control_path, usable_run_group,
    DirectoryEntries, RunIdentity, RUN_LOCK_NAME,
};
use serde::Deserialize;
use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::FromRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

mod legacy;
mod removal;

use legacy::{finish_retired_marker, lock_legacy_directory, ordinary_marker_name, park_run_marker};
use removal::remove_run_path;

pub(super) fn create(parent: &Path) -> std::io::Result<(PathBuf, LockedRun)> {
    loop {
        let entropy = uuid::Uuid::new_v4().to_string();
        let name = format!("r{}", &entropy[..5]);
        // Keep the same Unix socket path length as the previous flat layout.
        match create_temporary_entry_with(parent, &name, |parent, group, identity, basename| {
            reserve_run(parent, group, identity, basename, flock_exclusive)
        }) {
            Ok((_, run)) => return Ok(run),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn cleanup(dir: &Path) -> Result<()> {
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
    anyhow::ensure!(
        dir.file_name().is_some(),
        "expected a temporary run directory"
    );
    let owned = lock_existing_run(&dir)?;
    if owned.is_none() && legacy::resume_flat_run(&dir)? {
        return Ok(());
    }
    if owned.as_ref().is_some_and(LockedRun::unfinished) {
        let locked = owned.unwrap();
        locked.check_unmounted(&dir)?;
        anyhow::ensure!(locked.remove(&dir)?, "temporary run was already cleaned");
        remove_empty_temporary_group(dir.parent().unwrap())?;
        return Ok(());
    }
    let (legacy, id) = match &owned {
        Some(locked) => (None, locked.id.clone()),
        None => {
            legacy::check_flat_run(&dir)?;
            let lock = open_legacy_lock(&dir)?;
            let state = read_recovery(&dir)?.context("missing recovery record")?;
            anyhow::ensure!(
                state.format == "git-on-temp-v1",
                "unrecognized temporary directory"
            );
            let id = state.id.clone();
            (Some((lock, state)), id)
        }
    };
    #[cfg(target_os = "macos")]
    let nfs_socket = legacy
        .as_ref()
        .and_then(|(_, state)| state.nfs_socket_path());
    if mount_is_ours(
        &dir,
        &id,
        #[cfg(target_os = "macos")]
        nfs_socket.as_deref(),
    )? {
        #[cfg(target_os = "linux")]
        super::clean_unmount(&dir.join("mount"))?;
        #[cfg(target_os = "macos")]
        super::unmount_dead_server(&dir.join("mount"))?;
    }
    match &owned {
        Some(locked) => locked.check_unmounted(&dir)?,
        None => ensure_unmounted(&dir, None)?,
    }
    git_on::remote::stop_connection(&ssh_control_path(&dir))?;
    let memory = git_on::service_memory::stale_memory_workspace(&dir);
    #[cfg(target_os = "macos")]
    git_on::platform::cleanup_nfs_socket(&id, nfs_socket.as_deref())?;
    if let Some(locked) = owned {
        locked.prepare_cleanup()?;
        anyhow::ensure!(locked.remove(&dir)?, "temporary run was already cleaned");
    } else if let Some((lock, _)) = legacy {
        legacy::remove_flat_run(&dir, lock)?;
    }
    if grouped_run(&dir) {
        remove_empty_temporary_group(dir.parent().unwrap())
            .context("remove temporary git-on group")?;
    }
    if let Some(root) = memory {
        git_on::service_memory::wait_for_stale_memory(&root)?;
    }
    Ok(())
}

// Legacy invocations put mount identities in a separate file.
#[derive(Deserialize)]
#[cfg_attr(all(test, target_os = "linux"), derive(serde::Serialize))]
struct Recovery {
    format: String,
    id: String,
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

// Recover an interrupted publication left by the earlier startup protocol.
const PENDING_RUN_LOCK: &CStr = c".git-on-lock-new";
const RETRY_SUFFIX: &str = "-retry";

pub(super) fn private_dir(path: &Path) -> Result<()> {
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
fn open_run_marker(directory: libc::c_int, name: &CStr, owned: bool) -> Result<File> {
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
    Ok(f)
}
fn open_legacy_lock(dir: &Path) -> Result<File> {
    let name = CString::new(dir.join(".lock").as_os_str().as_encoded_bytes())?;
    let lock = open_run_marker(libc::AT_FDCWD, &name, false)?;
    let metadata = lock.metadata()?;
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.len() == 0,
        "invalid legacy recovery lock"
    );
    lock_file(&lock)?;
    Ok(lock)
}
fn flock_exclusive(file: &File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn lock_file(file: &File) -> Result<()> {
    flock_exclusive(file).map_err(|error| {
        let context = if error.kind() == std::io::ErrorKind::WouldBlock {
            "run is active or locked"
        } else {
            "lock temporary run"
        };
        anyhow::Error::new(error).context(context)
    })
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

fn entry_missing(parent: &File, name: &CStr) -> bool {
    matches!(marker_entry(parent.as_raw_fd(), name), Ok(None))
}
fn entry_matches(entry: &libc::stat, held: &fs::Metadata) -> bool {
    #[cfg(target_os = "linux")]
    let dev = entry.st_dev;
    #[cfg(target_os = "macos")]
    let dev = entry.st_dev as u64;
    dev == held.dev() && entry.st_ino == held.ino()
}

fn same_entry(first: &libc::stat, second: &libc::stat) -> bool {
    (first.st_dev, first.st_ino, first.st_uid) == (second.st_dev, second.st_ino, second.st_uid)
}

fn marker_matches(dir: libc::c_int, name: &CStr, lock: &File) -> std::io::Result<bool> {
    let Some(entry) = marker_entry(dir, name)? else {
        return Ok(false);
    };
    Ok(entry_matches(&entry, &lock.metadata()?))
}

fn owned_marker_reservation(entry: &libc::stat) -> bool {
    owned_empty_marker(entry) && entry.st_mode & 0o7777 == 0
}

fn owned_empty_marker(entry: &libc::stat) -> bool {
    entry.st_mode & libc::S_IFMT == libc::S_IFREG
        && entry.st_uid == unsafe { libc::geteuid() }
        && entry.st_nlink == 1
        && entry.st_size == 0
}

fn marker_in_directory_matches(
    path: &Path,
    directory: &File,
    name: &CStr,
    lock: &File,
) -> std::io::Result<bool> {
    Ok(same_run_directory(path, directory)? && marker_matches(directory.as_raw_fd(), name, lock)?)
}

fn try_owned_run_lock(
    parent: &File,
    name: &CStr,
    legacy_directory: Option<&File>,
) -> Result<Option<File>> {
    match open_run_marker(parent.as_raw_fd(), name, true) {
        Ok(lock) => {
            // Older starters lock the directory before creating their pending
            // marker. After opening it, take that same lock before claiming it.
            if let Some(directory) = legacy_directory {
                lock_legacy_directory(directory)?;
            }
            lock_file(&lock)?;
            Ok(Some(lock))
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => {
            // An interrupted portable rename can leave its mode-zero
            // reservation. The writable source marker still owns the run.
            match marker_entry(parent.as_raw_fd(), name) {
                Ok(Some(entry)) if owned_marker_reservation(&entry) => Ok(None),
                _ => Err(error),
            }
        }
    }
}

fn io_errno(error: &anyhow::Error) -> Option<i32> {
    error.downcast_ref::<std::io::Error>()?.raw_os_error()
}

pub(super) struct LockedRun {
    directory: File,
    lock: File,
    id: String,
    location: RunLock,
}

enum RunLock {
    Sibling(RunRecord),
    Grouped { marker: &'static CStr, parked: bool },
}

fn unfinished_marker(marker: &CStr) -> bool {
    marker == PENDING_RUN_LOCK
}

fn lock_existing_run(dir: &Path) -> Result<Option<LockedRun>> {
    let Some((directory, identity)) = run_identity(dir)? else {
        return Ok(None);
    };
    let identity = identity
        .context("temporary filesystem does not expose a persistent run directory identity")?;
    let group = open_removal_group(dir)?;
    if new_run_name(dir) {
        return lock_sibling_run(dir, directory, group, &identity).map(Some);
    }
    legacy::lock_grouped_run(dir, directory, group, identity.primary().to_string())
}

fn lock_sibling_run(
    dir: &Path,
    directory: File,
    group: File,
    identity: &RunIdentity,
) -> Result<LockedRun> {
    let name = run_record_name(dir.file_name().unwrap().to_str().unwrap());
    let folder =
        open_retry_record(&group, &name)?.context("missing temporary run ownership record")?;
    let marker = find_run_marker(&folder)?.context("missing temporary run ownership marker")?;
    // Either recorded identifier can prove ownership. The first one remains
    // the mount identity regardless of the metadata this client exposes.
    let (binding, marker_identity) = parse_bound_run_marker(&marker)
        .filter(|(bound, _)| identity.matches(*bound))
        .context("temporary run does not match its ownership record")?;
    let record = RunRecord {
        group,
        folder,
        name,
        marker,
        has_persistent_identity: true,
    };
    let lock = try_owned_run_lock(&record.folder, &record.marker, None)?
        .context("temporary run ownership marker disappeared")?;
    check_bound_marker(&lock, marker_identity)?;
    record.check(dir, &lock)?;
    anyhow::ensure!(
        same_run_directory(dir, &directory)?,
        "temporary run changed while locking it"
    );
    Ok(LockedRun {
        directory,
        lock,
        id: binding.primary().to_string(),
        location: RunLock::Sibling(record),
    })
}

fn open_removal_group(dir: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir.parent().unwrap())
        .context("open temporary run group")
}
fn run_record_name(basename: &str) -> CString {
    let uid = unsafe { libc::geteuid() };
    CString::new(format!(".git-on-run-{uid:x}-{basename}")).unwrap()
}

const BOUND_RUN_LOCK_PREFIX: &str = ".git-on-lock-v2-";
const RETIRED_RUN_LOCK: &CStr = c".git-on-retired";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerIdentity {
    Persistent(RunIdentity),
    Inode(u64),
}

fn marker_identity(file: &File, metadata: &fs::Metadata) -> std::io::Result<MarkerIdentity> {
    Ok(run_ids(OsStr::new("run-marker"), file, metadata)?
        .map(MarkerIdentity::Persistent)
        .unwrap_or_else(|| MarkerIdentity::Inode(metadata.ino())))
}

fn encode_identity(identity: &RunIdentity) -> String {
    let (kind, primary, alternate) = match *identity {
        RunIdentity::FileHandle {
            handle,
            creation_time,
        } => ('h', handle, creation_time),
        RunIdentity::CreationTime(id) => ('t', id, None),
        RunIdentity::Unverified(id) => ('u', id, None),
        RunIdentity::Legacy(_) => unreachable!("legacy runs do not have sibling records"),
    };
    let mut name = format!("{kind}{primary}");
    if let Some(alternate) = alternate {
        name.push('.');
        name.push_str(&alternate.to_string());
    }
    name
}

fn bound_run_marker(identity: &RunIdentity, marker: MarkerIdentity) -> CString {
    // Recovery verifies the run directory and the file startup actually locked.
    let marker = match marker {
        MarkerIdentity::Persistent(identity) => encode_identity(&identity),
        MarkerIdentity::Inode(inode) => format!("i{inode:016x}"),
    };
    CString::new(format!(
        "{BOUND_RUN_LOCK_PREFIX}{}+{marker}",
        encode_identity(identity)
    ))
    .unwrap()
}

fn parse_bound_run_marker(marker: &CStr) -> Option<(RunIdentity, MarkerIdentity)> {
    let suffix = marker.to_str().ok()?.strip_prefix(BOUND_RUN_LOCK_PREFIX)?;
    let (run, marker) = suffix.split_once('+')?;
    let identity = if let Some(inode) = marker.strip_prefix('i') {
        if inode.len() != 16 {
            return None;
        }
        MarkerIdentity::Inode(u64::from_str_radix(inode, 16).ok()?)
    } else {
        MarkerIdentity::Persistent(decode_identity(marker)?)
    };
    Some((decode_identity(run)?, identity))
}

fn decode_identity(suffix: &str) -> Option<RunIdentity> {
    let (primary, alternate) = match suffix.split_once('.') {
        Some((primary, alternate)) => (primary, Some(uuid::Uuid::parse_str(alternate).ok()?)),
        None => (suffix, None),
    };
    let id = uuid::Uuid::parse_str(primary.get(1..)?).ok()?;
    match primary.as_bytes().first()? {
        b'h' => Some(RunIdentity::FileHandle {
            handle: id,
            creation_time: alternate,
        }),
        b't' if alternate.is_none() => Some(RunIdentity::CreationTime(id)),
        b'u' if alternate.is_none() => Some(RunIdentity::Unverified(id)),
        _ => None,
    }
}

fn check_bound_marker(lock: &File, expected: MarkerIdentity) -> Result<()> {
    let metadata = lock.metadata()?;
    let matches = match expected {
        MarkerIdentity::Persistent(expected) => {
            matches!(marker_identity(lock, &metadata)?, MarkerIdentity::Persistent(actual)
                if actual.matches(expected))
        }
        MarkerIdentity::Inode(expected) => metadata.ino() == expected,
    };
    anyhow::ensure!(
        matches,
        "temporary run ownership marker does not match its recorded file"
    );
    Ok(())
}

fn find_run_marker(folder: &File) -> Result<Option<CString>> {
    let mut marker = None;
    let mut entries = DirectoryEntries::open(folder)?;
    while let Some(name) = entries.next_name()? {
        if name == RUN_LOCK_NAME
            || name == RETIRED_RUN_LOCK
            || parse_bound_run_marker(name).is_some()
        {
            anyhow::ensure!(
                marker.is_none(),
                "multiple writable markers in an owned run record"
            );
            marker = Some(name.to_owned());
        }
    }
    Ok(marker)
}
fn new_run_name(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"r"))
}
fn unlink_entry(fd: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    if unsafe { libc::unlinkat(fd, name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn rename_writable_marker(
    folder: &File,
    from: &CStr,
    to: &CStr,
    expected: &fs::Metadata,
) -> Result<()> {
    let fd = folder.as_raw_fd();
    // This record is private and the sole source marker is locked.
    anyhow::ensure!(
        marker_entry(fd, to)?.is_none(),
        "destination already has a writable marker"
    );
    #[cfg(test)]
    if to != RETIRED_RUN_LOCK && RUN_BIND_REPLACE_MARKER.replace(false) {
        unlink_entry(fd, from, 0)?;
        drop(create_run_marker(fd, from)?);
    }
    let renamed = if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    };
    #[cfg(test)]
    let renamed = simulate_lost_reply(renamed, &MARKER_RENAME_LOST_REPLY);
    #[cfg(test)]
    if let Some(errno) = MARKER_RENAME_VERIFY_ERROR.take() {
        return Err(std::io::Error::from_raw_os_error(errno))
            .context("check renamed writable marker");
    }
    // Verify the destination even on success. NFS can also publish the
    // original marker and lose its reply.
    if !marker_entry(fd, to)
        .context("check renamed writable marker")?
        .is_some_and(|entry| entry_matches(&entry, expected))
    {
        renamed.context("rename writable marker")?;
        bail!("temporary run marker changed during rename");
    }
    Ok(())
}

fn retire_marker(directory: &File, marker: &CStr, run: &Path, lock: File) -> Result<()> {
    let expected = lock.metadata()?;
    // The run is gone or never started. Closing our handle first avoids
    // making the NFS client rename an open marker into a .nfs placeholder.
    drop(lock);
    #[cfg(test)]
    if let Some(after_unlock) = AFTER_MARKER_UNLOCK.take() {
        after_unlock()?;
    }
    let Some(entry) = marker_entry(directory.as_raw_fd(), marker)? else {
        return Ok(());
    };
    anyhow::ensure!(
        entry_matches(&entry, &expected),
        "temporary run ownership marker changed before retirement"
    );
    let result = unlink_entry(directory.as_raw_fd(), marker, 0);
    #[cfg(test)]
    if ASSERT_MARKER_UNLINK_IS_IMMEDIATE.replace(false) {
        assert!(result.is_ok());
        assert!(DirectoryEntries::open(directory)?.next_name()?.is_none());
    }
    #[cfg(test)]
    let result = simulate_lost_reply(result, &RUN_RETIRE_UNLINK_LOST_REPLY);
    match result {
        // An NFS client can rename a still-open file while unlinking it. A
        // failed reply does not prove the marker remains under its old name.
        Err(_) if entry_missing(directory, marker) => Ok(()),
        result => result.with_context(|| format!("unlink retired marker for {}", run.display())),
    }
}

fn checked_deferred_marker(entry: &libc::stat) -> bool {
    let mode_without_setgid = entry.st_mode & !libc::S_ISGID;
    owned_directory_entry(entry) && mode_without_setgid & 0o7777 == 0o700
}

fn owned_directory_entry(entry: &libc::stat) -> bool {
    entry.st_mode & libc::S_IFMT == libc::S_IFDIR && entry.st_uid == unsafe { libc::geteuid() }
}

fn is_nfs(group: &File) -> std::io::Result<bool> {
    #[cfg(test)]
    if let Some(errno) = RUN_FILESYSTEM_PROBE_ERROR.take() {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(group.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    #[cfg(target_os = "linux")]
    {
        Ok(i128::from(stat.f_type) == i128::from(libc::NFS_SUPER_MAGIC))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) == c"nfs" })
    }
}

fn may_defer_unlink(group: &File) -> bool {
    is_nfs(group).unwrap_or(true)
}

fn valid_run_basename(name: &[u8]) -> bool {
    name.len() == 6 && matches!(name[0], b'g' | b'r') && name[1..].iter().all(u8::is_ascii_hexdigit)
}

#[derive(Debug, PartialEq)]
enum RetryRemoval {
    Removed,
    Missing,
    Busy,
}

fn remove_retry_record(group: &File, name: &CStr) -> std::io::Result<RetryRemoval> {
    #[cfg(test)]
    if let Some(errno) = RETRY_UNLINK_ERROR.take() {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    let result = unlink_entry(group.as_raw_fd(), name, libc::AT_REMOVEDIR);
    #[cfg(test)]
    let result = simulate_lost_reply(result, &RETRY_UNLINK_LOST_REPLY);
    match result {
        Ok(()) => Ok(RetryRemoval::Removed),
        // A replay of a successful rmdir can return ENOENT. The caller
        // checked that the record was present and owned before the attempt.
        Err(_) if entry_missing(group, name) => Ok(RetryRemoval::Removed),
        Err(error) => match error.kind() {
            std::io::ErrorKind::NotFound => Ok(RetryRemoval::Missing),
            std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists => {
                Ok(RetryRemoval::Busy)
            }
            _ => Err(error),
        },
    }
}

fn rollback_empty_directory(
    group: &File,
    name: &CStr,
    expected: &libc::stat,
) -> std::io::Result<RetryRemoval> {
    let Some(current) = marker_entry(group.as_raw_fd(), name)? else {
        return Ok(RetryRemoval::Missing);
    };
    if !owned_directory_entry(&current) || !same_entry(expected, &current) {
        return Err(std::io::Error::other(
            "temporary directory changed before rollback",
        ));
    }
    remove_retry_record(group, name)
}

fn rollback_unreadable_record(group: &File, name: &CStr) -> std::io::Result<RetryRemoval> {
    #[cfg(test)]
    if let Some(errno) = RUN_RECORD_ROLLBACK_ERROR.take() {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    let fd = group.as_raw_fd();
    let folder = match open_marker(fd, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
        Ok(folder) => folder,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RetryRemoval::Missing);
        }
        Err(error) => return Err(error),
    };
    let Some(entry) = marker_entry(fd, name)? else {
        return Ok(RetryRemoval::Missing);
    };
    if !entry_matches(&entry, &folder.metadata()?) {
        return Err(std::io::Error::other(
            "temporary record changed before rollback",
        ));
    }
    rollback_empty_directory(group, name, &entry)
}

fn rollback_reserved_record(
    group: &File,
    identity: &fs::Metadata,
    dir: &Path,
    rollback: impl Fn(&File) -> std::io::Result<RetryRemoval>,
) -> std::io::Result<RetryRemoval> {
    match rollback(group) {
        Err(error) if error.raw_os_error() == Some(libc::ESTALE) => {
            // A transient NFS error can leave the reservation in the original
            // group. Reopen that group before giving up on its empty record.
            let Some(reopened) = reopen_temporary_group(dir.parent().unwrap(), identity)? else {
                return Ok(RetryRemoval::Missing);
            };
            rollback(&reopened)
        }
        result => result,
    }
}

fn record_setup_error(
    error: std::io::Error,
    rollback: std::io::Result<RetryRemoval>,
    dir: &Path,
) -> std::io::Error {
    match rollback {
        Ok(RetryRemoval::Removed | RetryRemoval::Missing) => error,
        Ok(RetryRemoval::Busy) => {
            std::io::Error::other(format!("{error}; {}", PendingGroupCleanup::retry(dir)))
        }
        Err(rollback) => std::io::Error::other(format!(
            "{error}; rollback temporary run {}: {rollback}",
            dir.display()
        )),
    }
}

fn wait_for_retry_record(
    group: &File,
    name: &CStr,
    folder: &File,
    mut wait: impl FnMut() -> bool,
) -> std::io::Result<RetryRemoval> {
    let metadata = folder.metadata();
    #[cfg(test)]
    let metadata = match RETRY_METADATA_ERROR.take() {
        Some(errno) => Err(std::io::Error::from_raw_os_error(errno)),
        None => metadata,
    };
    let expected = metadata?;
    loop {
        if !marker_entry(group.as_raw_fd(), name)?
            .is_some_and(|entry| entry.st_uid == expected.uid() && entry_matches(&entry, &expected))
        {
            return Ok(RetryRemoval::Missing);
        }
        let outcome = remove_retry_record(group, name)?;
        if outcome != RetryRemoval::Busy || !wait() {
            return Ok(outcome);
        }
    }
}

fn open_retry_record(group: &File, name: &CStr) -> std::io::Result<Option<File>> {
    marker_entry(group.as_raw_fd(), name)?
        .map(|entry| open_checked_record(group, name, &entry))
        .transpose()
}

fn open_checked_record(group: &File, name: &CStr, entry: &libc::stat) -> std::io::Result<File> {
    if !checked_deferred_marker(entry) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid deferred temporary group record",
        ));
    }
    let folder = open_marker(
        group.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    if !entry_matches(entry, &folder.metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "deferred temporary group record changed",
        ));
    }
    Ok(folder)
}

fn record_disappeared(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESTALE)
}

fn open_reserved_run_record(
    group: &File,
    identity: &fs::Metadata,
    name: &CStr,
    dir: &Path,
) -> std::io::Result<(File, File)> {
    let collision = || std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    let fd = group.as_raw_fd();
    #[cfg(test)]
    let initial = match RUN_RECORD_STAT_ERROR.take() {
        Some(errno) => Err(std::io::Error::from_raw_os_error(errno)),
        None => marker_entry(fd, name),
    };
    #[cfg(not(test))]
    let initial = marker_entry(fd, name);
    let entry = match initial {
        Ok(Some(entry)) => entry,
        Ok(None) => return Err(collision()),
        Err(error) => {
            let rollback = rollback_reserved_record(group, identity, dir, |group| {
                rollback_unreadable_record(group, name)
            });
            return Err(record_setup_error(error, rollback, dir));
        }
    };
    let folder = open_checked_record(group, name, &entry).map_err(|error| {
        // Orphan cleanup can remove the empty record before the marker exists.
        if error.kind() == std::io::ErrorKind::NotFound {
            collision()
        } else if error.kind() == std::io::ErrorKind::AlreadyExists {
            error
        } else {
            let rollback = rollback_reserved_record(group, identity, dir, |group| {
                rollback_empty_directory(group, name, &entry)
            });
            record_setup_error(error, rollback, dir)
        }
    })?;
    let lock = create_run_marker(folder.as_raw_fd(), RUN_LOCK_NAME);
    #[cfg(test)]
    let lock = simulate_lost_reply(lock, &RUN_MARKER_CREATE_LOST_REPLY);
    match lock {
        Ok(lock) => Ok((folder, lock)),
        Err(error) => {
            let rollback = rollback_reserved_record(group, identity, dir, |group| {
                let removed = rollback_empty_directory(group, name, &entry)?;
                if removed != RetryRemoval::Busy {
                    return Ok(removed);
                }
                // An exclusive open can have installed the writable marker
                // even when the client received an error. Use the same
                // ownership checks and marker lock as explicit recovery.
                cleanup_orphaned_marker(dir, group, identity, name, true, Some(&entry))
                    .map(|_| RetryRemoval::Removed)
                    .map_err(run_setup_error)
            });
            if rollback
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
            {
                return Err(collision());
            }
            let error = if error.kind() == io::ErrorKind::NotFound {
                collision()
            } else {
                error
            };
            Err(record_setup_error(error, rollback, dir))
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static RETRY_RECORD_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RETRY_UNLINK_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RETRY_UNLINK_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static LEGACY_DIR_FLOCK_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RETRY_METADATA_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_MKDIR_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_RECORD_MKDIR_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_MARKER_CREATE_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_MKDIR_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_MKDIR_COLLISION_OWNER: std::cell::Cell<Option<libc::uid_t>> = const {
        std::cell::Cell::new(None)
    };
    static LAST_RETIREMENT_RMDIR_WAS_BUSY: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static ASSERT_MARKER_UNLINK_IS_IMMEDIATE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static RUN_FILESYSTEM_PROBE_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_CANONICALIZE_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_STAT_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_OPEN_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_RECORD_STAT_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_RECORD_ROLLBACK_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static MARKER_RENAME_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static MARKER_RENAME_VERIFY_ERROR: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_BIND_REPLACE_MARKER: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static AFTER_RUN_BIND: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_RETIRE_UNLINK_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static AFTER_MARKER_UNLOCK: std::cell::Cell<Option<MarkerUnlockHook>> = const {
        std::cell::Cell::new(None)
    };
    static RUN_NO_PERSISTENT_IDENTITY: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
type MarkerUnlockHook = Box<dyn FnOnce() -> io::Result<()>>;

#[derive(Debug)]
pub(super) struct PendingGroupCleanup {
    dir: PathBuf,
    can_retry: bool,
}

impl PendingGroupCleanup {
    fn retry(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            can_retry: true,
        }
    }
}

impl std::fmt::Display for PendingGroupCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.can_retry {
            write!(
                f,
                "temporary group cleanup is pending. After open files close, pass the following argument to git on --cleanup.\n{}",
                CleanupPath(&self.dir)
            )
        } else {
            write!(
                f,
                "temporary group cleanup is pending at {}. Manual recovery is required after open files close because the run has no persistent directory identity",
                CleanupPath(&self.dir)
            )
        }
    }
}

impl std::error::Error for PendingGroupCleanup {}

struct CleanupPath<'a>(&'a Path);

impl std::fmt::Display for CleanupPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write as _;
        let bytes = self.0.as_os_str().as_bytes();
        if bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(byte))
        {
            return f.write_str(std::str::from_utf8(bytes).unwrap());
        }
        // The run basename is ASCII, so command substitution never strips path
        // bytes as trailing newlines. POSIX printf decodes each octal byte.
        f.write_str("\"$(printf '%b' '")?;
        for &byte in bytes {
            if byte.is_ascii_graphic() && !matches!(byte, b'\'' | b'\\') {
                f.write_char(char::from(byte))?;
            } else {
                write!(f, "\\0{byte:03o}")?;
            }
        }
        f.write_str("')\"")
    }
}

fn remaining_mount_error(dir: &Path, can_retry: bool) -> anyhow::Error {
    let guidance = if can_retry {
        "After stopping its users, pass the path below to git on --cleanup."
    } else {
        "Manual recovery is required because the run has no persistent directory identity."
    };
    anyhow::anyhow!("temporary mount remains. {guidance}\n{}", CleanupPath(dir))
}

fn removal_group_gone(path: &Path, identity: &fs::Metadata) -> bool {
    match run_directory_matches(path, identity) {
        Ok(matches) => !matches,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

fn finish_retry_record(
    group: &File,
    identity: &fs::Metadata,
    dir: &Path,
    record: &CStr,
    folder: &File,
    retire: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let removed = retire().and_then(|()| {
        // The NFS client can finish an unlink shortly after closing the last
        // descriptor. Wait only for this run's retired directory.
        let deadline = Instant::now() + Duration::from_millis(100);
        let wait = || {
            wait_for_retry_record(group, record, folder, || {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
                true
            })
        };
        #[cfg(test)]
        let result = if LAST_RETIREMENT_RMDIR_WAS_BUSY.replace(false) {
            Ok(RetryRemoval::Busy)
        } else {
            wait()
        };
        #[cfg(not(test))]
        let result = wait();
        result.and_then(|state| {
            if state != RetryRemoval::Busy {
                return Ok(state);
            }
            let contents = remaining_entries(folder.as_raw_fd(), |name, entry| {
                nfs_placeholder_name(name.to_bytes()) && owned_retired_placeholder(entry)
            })?;
            match (contents.ordinary, contents.deferred) {
                (false, false) => wait_for_retry_record(group, record, folder, || false),
                (false, true) => Ok(state),
                (true, _) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "temporary ownership record contains unexpected files. Manual recovery is required.\n{}",
                        record_path(dir, record).display()
                    ),
                )),
            }
        }).context("remove deferred group record")
    });
    match removed {
        Ok(RetryRemoval::Removed) => Ok(true),
        Ok(RetryRemoval::Missing) => Ok(false),
        Ok(RetryRemoval::Busy) => Err(PendingGroupCleanup::retry(dir).into()),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(record_disappeared)
                && (entry_missing(group, record)
                    || removal_group_gone(dir.parent().unwrap(), identity)) =>
        {
            // A peer can remove the record as soon as the marker unlocks.
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn nfs_placeholder_name(name: &[u8]) -> bool {
    let Some(tail) = name.strip_prefix(b".nfs") else {
        return false;
    };
    // Darwin inserts dots around the eight-digit counter and four-digit PID.
    if let Some(darwin) = tail.strip_prefix(b".") {
        return darwin.len() == 13
            && darwin[8] == b'.'
            && darwin[..8].iter().all(u8::is_ascii_hexdigit)
            && darwin[9..].iter().all(u8::is_ascii_hexdigit);
    }
    // Linux uses a 16-digit file ID followed by an eight-digit counter.
    tail.len() == 24 && tail.iter().all(u8::is_ascii_hexdigit)
}

fn owned_retired_placeholder(entry: &libc::stat) -> bool {
    owned_empty_marker(entry) && entry.st_mode & 0o7777 == 0o600
}

#[derive(Default)]
struct RemainingEntries {
    ordinary: bool,
    deferred: bool,
}

fn remaining_entries(
    folder: libc::c_int,
    is_deferred: impl Fn(&CStr, &libc::stat) -> bool,
) -> io::Result<RemainingEntries> {
    let mut contents = RemainingEntries::default();
    let mut entries = DirectoryEntries::open_at(folder, c".")?;
    while let Some(name) = entries.next_name()? {
        if let Some(entry) = marker_entry(folder, name)? {
            if is_deferred(name, &entry) {
                contents.deferred = true;
            } else {
                contents.ordinary = true;
            }
        }
        if contents.deferred && contents.ordinary {
            break;
        }
    }
    Ok(contents)
}

fn reclaim_retired_placeholders(folder: &File) -> Result<()> {
    let mut entries = DirectoryEntries::open(folder)?;
    while let Some(name) = entries.next_name()? {
        if !nfs_placeholder_name(name.to_bytes()) {
            continue;
        }
        let Some(entry) = marker_entry(folder.as_raw_fd(), name)? else {
            continue;
        };
        // This isolated directory contained only our empty marker. A client
        // crash may leave its renamed placeholder even after all opens close.
        if !owned_retired_placeholder(&entry) {
            continue;
        }
        if let Err(error) = unlink_entry(folder.as_raw_fd(), name, 0) {
            // A locally deferred unlink remains busy until its holder closes.
            if error.kind() != std::io::ErrorKind::NotFound
                && error.raw_os_error() != Some(libc::EBUSY)
            {
                return Err(error).context("remove retired run placeholder");
            }
        }
    }
    Ok(())
}

fn record_path(dir: &Path, marker: &CStr) -> PathBuf {
    dir.with_file_name(OsStr::from_bytes(marker.to_bytes()))
}

fn cleanup_orphaned_marker(
    dir: &Path,
    group: &File,
    identity: &fs::Metadata,
    marker: &CStr,
    current: bool,
    expected: Option<&libc::stat>,
) -> Result<bool> {
    let uid = unsafe { libc::geteuid() };
    let Some(entry) = marker_entry(group.as_raw_fd(), marker)?.filter(|entry| entry.st_uid == uid)
    else {
        return Ok(false);
    };
    if expected.is_some_and(|expected| !same_entry(expected, &entry)) {
        return Ok(false);
    }
    let parent = dir.parent().unwrap();
    if current || marker.to_bytes().ends_with(RETRY_SUFFIX.as_bytes()) {
        anyhow::ensure!(
            matches!(fs::symlink_metadata(dir), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                && same_run_directory(parent, group)?,
            "temporary run or group changed during deferred cleanup"
        );
        if !current
            && marker_entry(group.as_raw_fd(), &ordinary_marker_name(marker))?.is_some_and(
                |entry| owned_empty_marker(&entry) && matches!(entry.st_mode & 0o7777, 0 | 0o600),
            )
        {
            return Ok(false);
        }
        let folder = match open_checked_record(group, marker, &entry) {
            Ok(folder) => folder,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        ensure_unmounted(&record_path(dir, marker), None)?;
        return finish_retry_record(group, identity, dir, marker, &folder, || {
            let writable = if current {
                find_run_marker(&folder)?
            } else {
                Some(RUN_LOCK_NAME.to_owned())
            };
            if let Some(writable) = writable {
                if let Some(lock) = try_owned_run_lock(&folder, &writable, None)? {
                    if let Some((_, marker_identity)) = parse_bound_run_marker(&writable) {
                        check_bound_marker(&lock, marker_identity)?;
                    }
                    anyhow::ensure!(
                        matches!(fs::symlink_metadata(dir), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                            && marker_matches(folder.as_raw_fd(), &writable, &lock)?,
                        "temporary run or retired marker changed while locking it"
                    );
                    let retired = if current && writable.as_c_str() == RUN_LOCK_NAME {
                        // A starter holding an open descriptor must see the
                        // startup name disappear before the lock is released.
                        rename_writable_marker(
                            &folder,
                            &writable,
                            RETIRED_RUN_LOCK,
                            &lock.metadata()?,
                        )?;
                        RETIRED_RUN_LOCK
                    } else {
                        &writable
                    };
                    retire_marker(&folder, retired, dir, lock)?;
                }
            }
            reclaim_retired_placeholders(&folder)
        });
    }
    let Some(lock) = try_owned_run_lock(group, marker, None)? else {
        return Ok(false);
    };
    anyhow::ensure!(
        matches!(fs::symlink_metadata(dir), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            && marker_matches(group.as_raw_fd(), marker, &lock)?,
        "temporary run or marker changed during cleanup"
    );
    finish_retired_marker(group, dir, marker, lock)
}

struct RunRecord {
    group: File,
    folder: File,
    name: CString,
    marker: CString,
    has_persistent_identity: bool,
}

impl RunRecord {
    fn check_group(&self, dir: &Path) -> Result<()> {
        let group = dir.parent().unwrap();
        let held = self.group.metadata()?;
        anyhow::ensure!(
            usable_run_group(group, &held) && run_directory_matches(group, &held)?,
            "temporary run group changed"
        );
        Ok(())
    }

    fn check(&self, dir: &Path, lock: &File) -> Result<()> {
        self.check_group(dir)?;
        anyhow::ensure!(
            marker_matches(self.group.as_raw_fd(), &self.name, &self.folder)?
                && marker_matches(self.folder.as_raw_fd(), &self.marker, lock)?,
            "temporary run ownership record changed"
        );
        Ok(())
    }

    fn finish(self, dir: &Path, lock: File) -> Result<bool> {
        self.check(dir, &lock)?;
        let identity = self.group.metadata()?;
        finish_retry_record(
            &self.group,
            &identity,
            dir,
            &self.name,
            &self.folder,
            || retire_marker(&self.folder, &self.marker, dir, lock),
        )
    }
}

fn run_setup_error(error: anyhow::Error) -> std::io::Error {
    error
        .downcast::<std::io::Error>()
        .unwrap_or_else(std::io::Error::other)
}

#[derive(PartialEq, Eq)]
enum RunCreation {
    NotAttempted,
    Unconfirmed,
    Created,
}

fn reserve_run(
    parent: &Path,
    group: &File,
    identity: &fs::Metadata,
    basename: &CStr,
    acquire: impl FnOnce(&File) -> std::io::Result<()>,
) -> std::io::Result<(PathBuf, LockedRun)> {
    let group = group.try_clone()?;
    let fd = group.as_raw_fd();
    let basename_str = basename.to_str().expect("validated run basename");
    let name = run_record_name(basename_str);
    let dir = parent.join(basename_str);

    // The writable lock stays in this private directory until teardown.
    // Reserve it before the lock so rollback can unlink without allocating.
    let reserved = make_temporary_directory(&group, &name);
    #[cfg(test)]
    let reserved = simulate_lost_reply(reserved, &RUN_RECORD_MKDIR_LOST_REPLY);
    if let Err(error) = reserved {
        let collision = error.kind() == std::io::ErrorKind::AlreadyExists;
        let rollback = rollback_reserved_record(&group, identity, &dir, |group| {
            if !collision {
                return rollback_unreadable_record(group, &name);
            }
            let Some(entry) = marker_entry(group.as_raw_fd(), &name)? else {
                return Ok(RetryRemoval::Missing);
            };
            // An EEXIST reply can follow a successful mkdir. Reclaim only an
            // empty private reservation. An existing marker blocks the rmdir.
            if !checked_deferred_marker(&entry) {
                return Ok(RetryRemoval::Busy);
            }
            rollback_empty_directory(group, &name, &entry)
        });
        if collision && rollback.is_ok() {
            return Err(error);
        }
        return Err(record_setup_error(error, rollback, &dir));
    }
    let (folder, lock) = open_reserved_run_record(&group, identity, &name, &dir)?;
    let mut record = RunRecord {
        group,
        folder,
        name,
        marker: RUN_LOCK_NAME.to_owned(),
        has_persistent_identity: false,
    };

    let mut creation_state = RunCreation::NotAttempted;
    let mut directory = None;
    let mut candidate_marker = None;
    let acquired = acquire(&lock);
    if acquired
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    {
        // Orphan cleanup won the lock before the run was visible.
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    let prepared = (|| -> Result<(PathBuf, String)> {
        acquired?;
        record.check(&dir, &lock)?;
        if marker_entry(fd, basename)?.is_some() {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into());
        }
        #[cfg(test)]
        if let Some(errno) = RUN_MKDIR_ERROR.take() {
            return Err(std::io::Error::from_raw_os_error(errno).into());
        }
        creation_state = RunCreation::Unconfirmed;
        let creation = make_temporary_directory(&record.group, basename);
        #[cfg(test)]
        let creation = creation.and_then(|()| {
            if let Some(uid) = RUN_MKDIR_COLLISION_OWNER.take() {
                if unsafe {
                    libc::fchownat(fd, basename.as_ptr(), uid, !0, libc::AT_SYMLINK_NOFOLLOW)
                } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            Ok(())
        });
        #[cfg(test)]
        let creation = simulate_lost_reply(creation, &RUN_MKDIR_LOST_REPLY);
        creation?;
        creation_state = RunCreation::Created;
        // Pin the new directory before any metadata query that might fail.
        #[cfg(test)]
        if let Some(errno) = RUN_OPEN_ERROR.take() {
            return Err(std::io::Error::from_raw_os_error(errno).into());
        }
        directory = Some(open_marker(
            fd,
            basename,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?);
        #[cfg(test)]
        if let Some(errno) = RUN_STAT_ERROR.take() {
            return Err(std::io::Error::from_raw_os_error(errno).into());
        }
        let opened = directory.as_ref().unwrap();
        let metadata = opened.metadata()?;
        #[cfg(test)]
        if let Some(errno) = RUN_CANONICALIZE_ERROR.take() {
            return Err(std::io::Error::from_raw_os_error(errno).into());
        }
        let canonical = fs::canonicalize(&dir)?;
        anyhow::ensure!(
            grouped_run(&canonical)
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o1777 == 0o700,
            "temporary run changed while opening it"
        );
        record.check(&canonical, &lock)?;
        // Live cleanup holds the original directory open even on a filesystem
        // that cannot supply evidence for stale recovery.
        let identity = run_ids(canonical.file_name().unwrap(), opened, &metadata)?;
        #[cfg(test)]
        let identity = if RUN_NO_PERSISTENT_IDENTITY.replace(false) {
            None
        } else {
            identity
        };
        record.has_persistent_identity = identity.is_some();
        let identity = identity.unwrap_or_else(|| RunIdentity::Unverified(uuid::Uuid::new_v4()));
        let lock_metadata = lock.metadata()?;
        let bound = bound_run_marker(&identity, marker_identity(&lock, &lock_metadata)?);
        if let Err(error) =
            rename_writable_marker(&record.folder, &record.marker, &bound, &lock_metadata)
        {
            candidate_marker = Some(bound);
            return Err(error);
        }
        record.marker = bound;
        #[cfg(test)]
        if let Some(after_bind) = AFTER_RUN_BIND.take() {
            after_bind();
        }
        // The caller will create its checkout and backing files by path.
        anyhow::ensure!(
            run_directory_matches(&canonical, &metadata)
                .context("check temporary run after binding")?,
            "temporary run changed after binding"
        );
        Ok((canonical, identity.primary().to_string()))
    })();
    match prepared {
        Ok((canonical, id)) => Ok((
            canonical,
            LockedRun {
                directory: directory.unwrap(),
                lock,
                id,
                location: RunLock::Sibling(record),
            },
        )),
        Err(error) => {
            // A retried mkdir can report EEXIST after creating the directory.
            if creation_state == RunCreation::Unconfirmed {
                match marker_entry(fd, basename) {
                    Ok(None) => {}
                    // A different user can reserve the same basename in a
                    // shared group. Their directory proves we lost the race.
                    Ok(Some(entry)) if entry.st_uid != unsafe { libc::geteuid() } => {}
                    _ => {
                        return Err(std::io::Error::other(format!(
                            "{error:#}. Startup could not confirm creation of this run. The ownership record was retained for manual recovery.\n{}",
                            dir.display()
                        )));
                    }
                }
            }
            let created = creation_state == RunCreation::Created;
            if !created
                && matches!(
                    marker_matches(record.folder.as_raw_fd(), &record.marker, &lock),
                    Ok(false)
                )
            {
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            let rollback = (|| -> Result<()> {
                let sibling = record_path(&dir, &record.name);
                ensure_unmounted(&sibling, created.then_some(dir.as_path()))?;
                if created {
                    if let Some(bound) = candidate_marker {
                        if marker_matches(record.folder.as_raw_fd(), &bound, &lock)? {
                            record.marker = bound;
                        }
                    }
                    record.check(&dir, &lock)?;
                    if let Some(current) = marker_entry(fd, basename)? {
                        if let Some(held) = &directory {
                            anyhow::ensure!(
                                entry_matches(&current, &held.metadata()?),
                                "temporary run changed before rollback"
                            );
                        }
                        // When the initial open fails, our record still owns
                        // this newly reserved name. Only rmdir a checked empty
                        // directory, using the original group descriptor.
                        anyhow::ensure!(
                            rollback_empty_directory(&record.group, basename, &current)?
                                != RetryRemoval::Busy,
                            "uninitialized run contains files"
                        );
                    } else if let Some(held) = &directory {
                        anyhow::ensure!(
                            held.metadata()?.nlink() == 0,
                            "temporary run moved before rollback"
                        );
                    }
                }
                record.finish(&dir, lock)?;
                Ok(())
            })();
            match rollback {
                Ok(()) => Err(run_setup_error(error)),
                Err(rollback) => Err(std::io::Error::other(format!(
                    "{error:#}; rollback temporary run {}: {rollback:#}",
                    dir.display()
                ))),
            }
        }
    }
}

impl LockedRun {
    pub(super) fn id(&self) -> &str {
        &self.id
    }

    fn unfinished(&self) -> bool {
        matches!(&self.location, RunLock::Grouped { marker, .. } if unfinished_marker(marker))
    }

    pub(super) fn remaining_mount_error(&self, dir: &Path) -> anyhow::Error {
        let can_retry = match &self.location {
            RunLock::Sibling(record) => record.has_persistent_identity,
            RunLock::Grouped { .. } => true,
        };
        remaining_mount_error(dir, can_retry)
    }

    pub(super) fn check_unmounted(&self, dir: &Path) -> Result<()> {
        let sibling = match &self.location {
            RunLock::Sibling(record) => {
                record.check_group(dir)?;
                Some(record_path(dir, &record.name))
            }
            RunLock::Grouped { .. } => None,
        };
        ensure_unmounted(dir, sibling.as_deref())
    }

    pub(super) fn prepare_cleanup(&self) -> Result<()> {
        git_on::materialize::prepare_cleanup(&self.directory)
    }

    pub(super) fn check_directory(&self, dir: &Path) -> Result<()> {
        anyhow::ensure!(
            same_run_directory(dir, &self.directory)?,
            "temporary run changed during cleanup"
        );
        Ok(())
    }

    pub(super) fn remove(self, dir: &Path) -> Result<bool> {
        self.remove_with_parking(dir, park_run_marker)
    }

    fn remove_with_parking(
        self,
        dir: &Path,
        park: impl FnMut(libc::c_int, &CStr, libc::c_int, &CStr) -> Result<()>,
    ) -> Result<bool> {
        self.check_directory(dir)?;
        match self.location {
            RunLock::Sibling(record) => {
                record.check(dir, &self.lock)?;
                remove_run_path(
                    dir,
                    &record.group,
                    &self.directory,
                    false,
                    record.has_persistent_identity,
                )?;
                record.finish(dir, self.lock)
            }
            RunLock::Grouped { marker, parked } => {
                legacy::remove_grouped(self, dir, marker, parked, park)
            }
        }
    }
}

fn cleanup_orphaned_run_marker(dir: &Path) -> Result<bool> {
    let Some(name) = dir.file_name() else {
        return Ok(false);
    };
    let parent = fs::canonicalize(
        dir.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let dir = parent.join(name);
    let group = open_removal_group(&dir)?;
    if !grouped_run(&dir) || !valid_run_basename(name.as_bytes()) {
        return legacy::cleanup_flat_orphans(&dir, &group);
    }
    let name = name.to_str().unwrap();
    let identity = group.metadata()?;
    if !usable_run_group(&parent, &identity) {
        return Ok(false);
    }
    let removed = if new_run_name(&dir) {
        cleanup_orphaned_marker(&dir, &group, &identity, &run_record_name(name), true, None)?
    } else {
        legacy::cleanup_grouped_orphans(&dir, &group, &identity, name)?
    };
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

#[cfg(test)]
fn simulate_lost_reply<T>(
    result: std::io::Result<T>,
    fault: &'static std::thread::LocalKey<std::cell::Cell<Option<i32>>>,
) -> std::io::Result<T> {
    result.and_then(|value| match fault.take() {
        Some(errno) => Err(std::io::Error::from_raw_os_error(errno)),
        None => Ok(value),
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
