//! Recovery for the older grouped run records.

use super::{
    bound_run_marker, check_bound_marker, cleanup_orphaned_marker, entry_matches,
    finish_retry_record, io_errno, is_nfs, lock_file, marker_entry, marker_identity,
    marker_in_directory_matches, marker_matches, may_defer_unlink, open_checked_record,
    open_marker, open_removal_group, owned_empty_marker, owned_marker_reservation,
    parse_bound_run_marker, record_disappeared, record_path, record_setup_error,
    removal_group_gone, remove_run_path, retire_marker, rollback_empty_directory,
    rollback_unreadable_record, same_entry, try_owned_run_lock, unfinished_marker, unlink_entry,
    valid_run_basename, LockedRun, MarkerIdentity, RunLock, PENDING_RUN_LOCK, RETRY_SUFFIX,
};
#[cfg(test)]
use super::{LEGACY_DIR_FLOCK_ERROR, RETRY_RECORD_ERROR};
use anyhow::{bail, Context, Result};
use git_on::platform::{
    ensure_unmounted, make_temporary_directory, open_child_directory, run_ids, same_run_directory,
    usable_run_group, DirectoryEntries, RunIdentity, RUN_LOCK_NAME,
};
use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File, Permissions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
};

const RETIRING_LOCK_PREFIX: &str = ".git-on-remove-";

fn flat_marker_prefix(dir: &Path) -> String {
    let name = dir.file_name().unwrap().as_encoded_bytes();
    format!(".git-on-flat-{}-", &blake3::hash(name).to_hex()[..32])
}

fn flat_directory(group: &File, dir: &Path) -> Result<File> {
    let name = CString::new(dir.file_name().unwrap().as_encoded_bytes())?;
    let directory = open_child_directory(group.as_raw_fd(), &name, libc::O_RDONLY)?;
    let metadata = directory.metadata()?;
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
        "legacy run has unexpected ownership or permissions"
    );
    let has_entry = |name| marker_entry(directory.as_raw_fd(), name).map(|entry| entry.is_some());
    anyhow::ensure!(
        !has_entry(c".git")?
            && !(has_entry(c"HEAD")? && (has_entry(c"objects")? || has_entry(c"commondir")?)),
        "legacy cleanup refuses a Git repository directory"
    );
    Ok(directory)
}

pub(super) fn check_flat_run(dir: &Path) -> Result<()> {
    flat_directory(&open_removal_group(dir)?, dir)?;
    Ok(())
}

fn flat_identity(directory: &File) -> Result<Option<RunIdentity>> {
    #[cfg(test)]
    if super::RUN_NO_PERSISTENT_IDENTITY.replace(false) {
        return Ok(None);
    }
    // Use a fixed new-format name to get persistent metadata even for runs
    // whose original basename predates the grouped layout.
    Ok(run_ids(
        OsStr::new("r-flat"),
        directory,
        &directory.metadata()?,
    )?)
}

fn flat_markers(group: &File, dir: &Path) -> Result<Vec<(CString, RunIdentity, MarkerIdentity)>> {
    let prefix = flat_marker_prefix(dir);
    let mut entries = DirectoryEntries::open(group)?;
    let mut markers = Vec::new();
    while let Some(name) = entries.next_name()? {
        if let Some(binding) = name
            .to_str()
            .ok()
            .and_then(|name| name.strip_prefix(&prefix))
        {
            let binding = CString::new(binding.strip_suffix(RETRY_SUFFIX).unwrap_or(binding))?;
            if let Some((run, lock)) = parse_bound_run_marker(&binding) {
                markers.push((name.to_owned(), run, lock));
            }
        }
    }
    Ok(markers)
}

pub(super) fn remove_flat_run(dir: &Path, lock: File) -> Result<()> {
    let group = open_removal_group(dir)?;
    let directory = flat_directory(&group, dir)?;
    let fd = directory.as_raw_fd();
    let group_fd = group.as_raw_fd();
    anyhow::ensure!(
        marker_matches(fd, c".lock", &lock)?,
        "legacy run lock changed"
    );
    let Some(identity) = flat_identity(&directory)? else {
        return remove_flat_run_in_place(dir, &group, &directory, lock);
    };
    lock.set_permissions(Permissions::from_mode(0o600))?;
    let metadata = lock.metadata()?;
    let bound = bound_run_marker(&identity, marker_identity(&lock, &metadata)?);
    let marker = CString::new([flat_marker_prefix(dir).as_bytes(), bound.to_bytes()].concat())?;
    ensure_unmounted(dir, Some(&record_path(dir, &marker)))?;
    // The locked file stays available at the sibling name until the run
    // directory is gone, including after an interruption during rmdir.
    let parked = park_run_marker(fd, c".lock", group_fd, &marker);
    if !marker_matches(group_fd, &marker, &lock)? {
        parked?;
        bail!("legacy run lock changed while parking it");
    }
    finish_flat_run(dir, &group, &directory, &marker, lock)
}

fn remove_flat_run_in_place(dir: &Path, group: &File, directory: &File, lock: File) -> Result<()> {
    let check = || -> Result<()> {
        anyhow::ensure!(
            same_run_directory(dir.parent().unwrap(), group)?
                && marker_in_directory_matches(dir, directory, c".lock", &lock)?,
            "legacy run or ownership record changed during cleanup"
        );
        Ok(())
    };
    ensure_unmounted(dir, None)?;
    check()?;
    git_on::materialize::prepare_cleanup(directory)?;
    // Keep the original recovery records in place while removing payloads.
    // An interrupted payload removal can then be resumed under the same lock.
    super::removal::reclaim_run_contents(dir, group, directory, &[c".lock", c".state"])?;
    check()?;
    unlink_entry(directory.as_raw_fd(), c".state", 0)?;
    retire_marker(directory, c".lock", dir, lock)?;
    // Only remove the empty directory after releasing the original lock.
    remove_run_path(dir, group, directory, true, false)
}

pub(super) fn resume_flat_run(dir: &Path) -> Result<bool> {
    let group = open_removal_group(dir)?;
    let mut markers = flat_markers(&group, dir)?
        .into_iter()
        .filter(|(name, _, _)| !name.to_bytes().ends_with(RETRY_SUFFIX.as_bytes()))
        .peekable();
    if markers.peek().is_none() {
        return Ok(false);
    }
    let directory = flat_directory(&group, dir)?;
    let identity = flat_identity(&directory)?
        .context("temporary filesystem cannot identify the legacy run for cleanup")?;
    for (marker, run, marker_id) in markers {
        if !identity.matches(run) {
            continue;
        }
        if let Some(lock) = try_owned_run_lock(&group, &marker, None)? {
            check_bound_marker(&lock, marker_id)?;
            finish_flat_run(dir, &group, &directory, &marker, lock)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn finish_flat_run(
    dir: &Path,
    group: &File,
    directory: &File,
    marker: &CStr,
    lock: File,
) -> Result<()> {
    ensure_unmounted(dir, Some(&record_path(dir, marker)))?;
    anyhow::ensure!(
        same_run_directory(dir.parent().unwrap(), group)?
            && same_run_directory(dir, directory)?
            && marker_matches(group.as_raw_fd(), marker, &lock)?,
        "legacy run or ownership record changed during cleanup"
    );
    git_on::materialize::prepare_cleanup(directory)?;
    remove_run_path(dir, group, directory, false, true)?;
    anyhow::ensure!(
        finish_retired_marker(group, dir, marker, lock)?,
        "temporary run was already cleaned"
    );
    Ok(())
}

pub(super) fn cleanup_flat_orphans(dir: &Path, group: &File) -> Result<bool> {
    let mut removed = false;
    let identity = group.metadata()?;
    let mut markers = flat_markers(group, dir)?;
    markers.sort_unstable_by_key(|(marker, _, _)| {
        marker.to_bytes().ends_with(RETRY_SUFFIX.as_bytes())
    });
    for (marker, _, marker_id) in markers {
        if marker_entry(group.as_raw_fd(), &marker)?
            .is_some_and(|entry| owned_empty_marker(&entry) && entry.st_mode & 0o7777 == 0)
        {
            // This was an exclusive-rename reservation without a committed move.
            continue;
        }
        if marker.to_bytes().ends_with(RETRY_SUFFIX.as_bytes()) {
            removed |= cleanup_orphaned_marker(dir, group, &identity, &marker, false, None)?;
        } else if let Some(lock) = try_owned_run_lock(group, &marker, None)? {
            check_bound_marker(&lock, marker_id)?;
            anyhow::ensure!(
                matches!(fs::symlink_metadata(dir), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                    && same_run_directory(dir.parent().unwrap(), group)?,
                "legacy run or group changed before marker retirement"
            );
            removed |= finish_retired_marker(group, dir, &marker, lock)?;
        }
    }
    Ok(removed)
}

pub(super) fn retiring_lock_name(dir: &Path, id: &str, marker: &CStr) -> CString {
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

fn retired_marker_run(name: &str) -> Option<&str> {
    let (basename, tail) = name
        .strip_suffix(RETRY_SUFFIX)
        .unwrap_or(name)
        .strip_prefix(RETIRING_LOCK_PREFIX)
        .and_then(|rest| rest.split_once('-'))?;
    let identity = tail
        .strip_prefix("ready-")
        .or_else(|| tail.strip_prefix("pending-"))?;
    (valid_run_basename(basename.as_bytes()) && uuid::Uuid::parse_str(identity).is_ok())
        .then_some(basename)
}

pub(super) fn retry_marker_name(mark: &CStr) -> CString {
    CString::new([mark.to_bytes(), RETRY_SUFFIX.as_bytes()].concat()).unwrap()
}

pub(super) fn ordinary_marker_name(retry: &CStr) -> CString {
    CString::new(
        retry
            .to_bytes()
            .strip_suffix(RETRY_SUFFIX.as_bytes())
            .unwrap(),
    )
    .unwrap()
}

pub(super) fn prepare_retry_record(group: &File, name: &CStr, dir: &Path) -> Result<File> {
    #[cfg(test)]
    if let Some(errno) = RETRY_RECORD_ERROR.take() {
        return Err(std::io::Error::from_raw_os_error(errno))
            .context("reserve deferred temporary group record");
    }
    let created = match make_temporary_directory(group, name) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(error).context("reserve deferred temporary group record");
        }
    };
    let entry = match marker_entry(group.as_raw_fd(), name) {
        Err(error) if created && !record_disappeared(&error) => {
            return Err(
                record_setup_error(error, rollback_unreadable_record(group, name), dir).into(),
            );
        }
        result => result?.context("deferred group record disappeared")?,
    };
    match open_checked_record(group, name, &entry) {
        Err(error) if created => {
            Err(
                record_setup_error(error, rollback_empty_directory(group, name, &entry), dir)
                    .into(),
            )
        }
        opened => opened.map_err(Into::into),
    }
}

pub(super) fn finish_retired_marker(
    group: &File,
    dir: &Path,
    name: &CStr,
    lock: File,
) -> Result<bool> {
    let path = dir.parent().unwrap();
    let entry = marker_entry(group.as_raw_fd(), name)?.context("retired run marker disappeared")?;
    anyhow::ensure!(
        entry_matches(&entry, &lock.metadata()?) && same_run_directory(path, group)?,
        "retired temporary run marker or group changed"
    );
    let defer = may_defer_unlink(group);
    #[cfg(test)]
    let defer = defer || RETRY_RECORD_ERROR.get().is_some();
    if defer {
        let identity = group.metadata()?;
        let record = retry_marker_name(name);
        ensure_unmounted(&record_path(dir, &record), None)?;
        // Keep the original marker if allocating the retirement directory
        // fails. NFS puts a deferred unlink inside this directory.
        let folder = prepare_retry_record(group, &record, dir)
            .with_context(|| format!("retain recovery marker for {}", dir.display()))?;
        park_run_marker(group.as_raw_fd(), name, folder.as_raw_fd(), RUN_LOCK_NAME)?;
        finish_retry_record(group, &identity, dir, &record, &folder, || {
            retire_marker(&folder, RUN_LOCK_NAME, dir, lock)
        })
    } else {
        retire_marker(group, name, dir, lock)?;
        Ok(true)
    }
}

pub(super) fn park_run_marker(
    from: libc::c_int,
    marker: &CStr,
    to: libc::c_int,
    parked: &CStr,
) -> Result<()> {
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
    let unavailable = matches!(
        error.kind(),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
    ) || matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::ENOTSUP));
    let retry = error.kind() == std::io::ErrorKind::AlreadyExists;
    if !unavailable && !retry {
        return Err(error.into());
    }
    park_run_marker_with_reservation(from, marker, to, parked)
}

pub(super) fn park_run_marker_with_reservation(
    from: libc::c_int,
    marker: &CStr,
    to: libc::c_int,
    parked: &CStr,
) -> Result<()> {
    // If the filesystem lacks exclusive rename, reserve the destination
    // ourselves under the held marker flock. Mode zero is independent
    // of the caller's umask and identifies an interrupted reservation.
    let created = match open_marker(to, parked, libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL, 0) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let reservation = marker_entry(to, parked)?.context("parking reservation disappeared")?;
    anyhow::ensure!(
        owned_marker_reservation(&reservation),
        "parking path is not an owned reservation"
    );
    if unsafe { libc::renameat(from, marker.as_ptr(), to, parked.as_ptr()) } != 0 {
        let error = std::io::Error::last_os_error();
        if created
            && marker_entry(to, parked)
                .is_ok_and(|entry| entry.is_some_and(|entry| same_entry(&entry, &reservation)))
        {
            unsafe { libc::unlinkat(to, parked.as_ptr(), 0) };
        }
        return Err(error.into());
    }
    Ok(())
}

pub(super) fn cleanup_legacy_marker_records(
    dir: &Path,
    group: &File,
    identity: &fs::Metadata,
    mut records: Vec<CString>,
) -> Result<bool> {
    // A retry directory can only be retired once its parked marker is gone.
    records.sort_unstable_by_key(|record| record.to_bytes().ends_with(RETRY_SUFFIX.as_bytes()));
    let mut removed = false;
    for record in records {
        match cleanup_orphaned_marker(dir, group, identity, &record, false, None) {
            Ok(current) => removed |= current,
            Err(_) if removed && removal_group_gone(dir.parent().unwrap(), identity) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

pub(super) fn lock_legacy_directory(directory: &File) -> Result<()> {
    #[cfg(test)]
    let result = match LEGACY_DIR_FLOCK_ERROR.take() {
        Some(errno) => Err(std::io::Error::from_raw_os_error(errno)).context("lock temporary run"),
        None => lock_file(directory),
    };
    #[cfg(not(test))]
    let result = lock_file(directory);
    match result {
        Err(error)
            if io_errno(&error) == Some(libc::EBADF) && matches!(is_nfs(directory), Ok(true)) =>
        {
            // Some NFS clients reject exclusive locks on read-only directories.
            // The caller still acquires the exclusive lock on the writable marker.
            Ok(())
        }
        result => result,
    }
}

pub(super) fn lock_grouped_run(
    dir: &Path,
    directory: File,
    group: File,
    id: String,
) -> Result<Option<LockedRun>> {
    let ready = retiring_lock_name(dir, &id, RUN_LOCK_NAME);
    let pending = retiring_lock_name(dir, &id, PENDING_RUN_LOCK);
    let paths = [
        (&directory, RUN_LOCK_NAME, RUN_LOCK_NAME, false),
        (&directory, PENDING_RUN_LOCK, PENDING_RUN_LOCK, false),
        (&group, ready.as_c_str(), RUN_LOCK_NAME, true),
        (&group, pending.as_c_str(), PENDING_RUN_LOCK, true),
    ];
    for &(parent, path, marker, parked) in &paths {
        let legacy = (path == PENDING_RUN_LOCK || parked).then_some(&directory);
        if let Some(lock) = try_owned_run_lock(parent, path, legacy)? {
            // The previous holder could have moved the marker or removed the
            // run after we opened the marker but before acquiring its lock.
            anyhow::ensure!(
                same_run_directory(dir, &directory)?
                    && marker_matches(parent.as_raw_fd(), path, &lock)?,
                "temporary run or marker changed while locking it"
            );
            return Ok(Some(LockedRun {
                directory,
                lock,
                id,
                location: RunLock::Grouped { marker, parked },
            }));
        }
    }
    Ok(None)
}

pub(super) fn remove_grouped(
    run: LockedRun,
    dir: &Path,
    marker: &'static CStr,
    parked: bool,
    mut park: impl FnMut(libc::c_int, &CStr, libc::c_int, &CStr) -> Result<()>,
) -> Result<bool> {
    let unfinished = unfinished_marker(marker);
    if unfinished {
        let mut entries = DirectoryEntries::open(&run.directory)?;
        while let Some(name) = entries.next_name()? {
            if !parked && name == marker {
                continue;
            }
            bail!("unfinished run contains other files");
        }
    }
    let group = open_removal_group(dir)?;
    anyhow::ensure!(
        usable_run_group(dir.parent().unwrap(), &group.metadata()?),
        "temporary run group changed during cleanup"
    );
    let parked_name = retiring_lock_name(dir, &run.id, marker);
    if !parked {
        anyhow::ensure!(
            marker_in_directory_matches(dir, &run.directory, marker, &run.lock)?,
            "temporary run or its marker changed during cleanup"
        );
        let mut park_marker = || {
            park(
                run.directory.as_raw_fd(),
                marker,
                group.as_raw_fd(),
                &parked_name,
            )
        };
        if let Err(error) = park_marker() {
            let capacity_error = matches!(io_errno(&error), Some(libc::ENOSPC | libc::EDQUOT));
            if unfinished || !capacity_error {
                return Err(error).context("park temporary run marker");
            }

            // Filesystems without exclusive rename may need an inode for
            // the parking reservation. Reclaim run contents while the
            // original ownership marker is still in place and locked.
            anyhow::ensure!(
                marker_in_directory_matches(dir, &run.directory, marker, &run.lock)?,
                "temporary run or marker changed before reclaiming space"
            );
            super::removal::reclaim_run_contents(dir, &group, &run.directory, &[marker])?;
            anyhow::ensure!(
                marker_in_directory_matches(dir, &run.directory, marker, &run.lock)?,
                "temporary run or marker changed while reclaiming space"
            );
            park_marker().context("park temporary run marker after reclaiming space")?;
        }
    }
    anyhow::ensure!(
        same_run_directory(dir, &run.directory)?
            && marker_matches(group.as_raw_fd(), &parked_name, &run.lock)?,
        "temporary run or parked marker changed during cleanup"
    );
    remove_run_path(dir, &group, &run.directory, unfinished, true)?;
    finish_retired_marker(&group, dir, &parked_name, run.lock)
}

pub(super) fn cleanup_grouped_orphans(
    dir: &Path,
    group: &File,
    identity: &fs::Metadata,
    name: &str,
) -> Result<bool> {
    let mut records = Vec::new();
    let mut entries = DirectoryEntries::open(group)?;
    while let Some(entry) = entries.next_name()? {
        if entry.to_str().ok().and_then(retired_marker_run) == Some(name) {
            records.push(entry.to_owned());
        }
    }
    drop(entries);
    cleanup_legacy_marker_records(dir, group, identity, records)
}
