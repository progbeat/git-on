#[cfg(all(test, target_os = "linux"))]
use super::remaining_entries;
#[cfg(test)]
use super::simulate_lost_reply;
use super::{
    entry_missing, is_nfs, marker_entry, nfs_placeholder_name, same_entry, unlink_entry,
    CleanupPath, PendingGroupCleanup,
};
use anyhow::Result;
use git_on::platform::{ensure_unmounted, nfs_mount, same_run_directory, DirectoryEntries};
#[cfg(test)]
use std::fs;
use std::{
    ffi::{CStr, CString},
    fs::File,
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::Path,
};

pub(super) fn pending_nfs_entry(name: &CStr, entry: &libc::stat) -> bool {
    nfs_placeholder_name(name.to_bytes()) && entry.st_mode & libc::S_IFMT != libc::S_IFDIR
}

fn descriptor_stat(fd: libc::c_int) -> io::Result<libc::stat> {
    let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, entry.as_mut_ptr()) } == 0 {
        Ok(unsafe { entry.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn check_removal_entry(parent: libc::c_int, name: &CStr, expected: &libc::stat) -> io::Result<()> {
    let current = marker_entry(parent, name)?.ok_or(io::ErrorKind::NotFound)?;
    if current.st_mode & libc::S_IFMT != libc::S_IFDIR || !same_entry(&current, expected) {
        return Err(io::Error::other(
            "temporary directory changed before removal",
        ));
    }
    Ok(())
}

fn removed_directory(parent: &File, name: &CStr, fd: libc::c_int) -> bool {
    // The open inode's link count survives rmdir even on filesystems that
    // can no longer retrieve the removed directory's creation time.
    #[cfg(test)]
    if RUN_RMDIR_ATTRIBUTES_UNAVAILABLE.get() {
        return false;
    }
    entry_missing(parent, name) && descriptor_stat(fd).is_ok_and(|entry| entry.st_nlink == 0)
}

fn entry_is_directory(fd: libc::c_int, name: &CStr, kind: libc::c_uchar) -> io::Result<bool> {
    // Most filesystems supply d_type, so ordinary files need only unlinkat.
    Ok(match kind {
        libc::DT_DIR => true,
        libc::DT_UNKNOWN => marker_entry(fd, name)?
            .is_some_and(|entry| entry.st_mode & libc::S_IFMT == libc::S_IFDIR),
        _ => false,
    })
}

fn unlink_descendant(fd: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    #[cfg(test)]
    if DESCENDANT_BUSY.get() == Some(name) {
        return Err(io::Error::from_raw_os_error(libc::EBUSY));
    }
    let unlinked = unlink_entry(fd, name, flags);
    #[cfg(test)]
    let unlinked = simulate_lost_reply(unlinked, &DESCENDANT_UNLINK_LOST_REPLY);
    match unlinked {
        // The server can remove the entry and lose its reply.
        Err(_) if matches!(marker_entry(fd, name), Ok(None)) => Ok(()),
        result => result,
    }
}

fn remove_entry(
    fd: libc::c_int,
    name: &CStr,
    kind: libc::c_uchar,
) -> io::Result<Option<DirectoryEntries>> {
    if entry_is_directory(fd, name, kind)? {
        #[cfg(all(test, target_os = "linux"))]
        match BEFORE_CHILD_OPEN.take() {
            Some(BeforeChildOpen::ReplaceWithSymlink) => {
                unlink_entry(fd, name, libc::AT_REMOVEDIR)?;
                if unsafe { libc::symlinkat(c"../outside".as_ptr(), fd, name.as_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Some(BeforeChildOpen::Stale { removed }) => {
                if removed {
                    unlink_entry(fd, name, libc::AT_REMOVEDIR)?;
                }
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            None => {}
        }
        match DirectoryEntries::open_at(fd, name) {
            Ok(child) => return Ok(Some(child)),
            // A child can turn into a symlink or file after readdir. Unlink
            // the current entry without following it.
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {}
            Err(error) => return Err(error),
        }
    }
    match unlink_descendant(fd, name, 0) {
        Ok(()) => Ok(None),
        Err(error)
            if matches!(error.raw_os_error(), Some(libc::EISDIR | libc::EPERM))
                && entry_is_directory(fd, name, libc::DT_UNKNOWN)? =>
        {
            DirectoryEntries::open_at(fd, name).map(Some)
        }
        Err(error) => Err(error),
    }
}

struct DirectoryRemoval {
    entries: DirectoryEntries,
    name: Option<CString>,
    // The parent's directory stream stays on the stack while this frame exists.
    parent: libc::c_int,
    retry: DirectoryRetry,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectoryRetry {
    Initial,
    Rescan,
    Empty,
}

fn directory_disappeared(parent: libc::c_int, name: Option<&CStr>, error: &io::Error) -> bool {
    (error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESTALE))
        && name.is_some_and(|name| matches!(marker_entry(parent, name), Ok(None)))
}

struct RunRemoval<'a> {
    path: &'a Path,
    group: &'a File,
    directory: &'a File,
    can_retry: bool,
}

impl RunRemoval<'_> {
    fn is_nfs(&self) -> bool {
        #[cfg(test)]
        if FORCE_NFS_REMOVAL.get() {
            return true;
        }
        is_nfs(self.group)
            .unwrap_or_else(|_| nfs_mount(self.group, self.path.parent().unwrap()).unwrap_or(false))
    }

    fn retained_record_error(
        &self,
        error: io::Error,
        reason: &str,
        can_cleanup: bool,
    ) -> anyhow::Error {
        let guidance = if can_cleanup {
            "Verify the run path before passing it to git on --cleanup."
        } else {
            "Manual recovery is required because the run has no persistent directory identity."
        };
        anyhow::anyhow!(
            "{error}. {reason}. The ownership record was retained. {guidance}\n{}",
            CleanupPath(self.path)
        )
    }

    fn pending(&self) -> anyhow::Error {
        PendingGroupCleanup {
            dir: self.path.to_owned(),
            can_retry: self.can_retry,
        }
        .into()
    }

    fn inspect_remaining(&self, scope: libc::c_int) -> Result<bool> {
        let mut entries = DirectoryEntries::open_at(scope, c".")?;
        let mut empty = true;
        while let Some(name) = entries.next_name()? {
            let Some(entry) = marker_entry(scope, name)? else {
                continue;
            };
            if !pending_nfs_entry(name, &entry) {
                empty = false;
                continue;
            }
            match unlink_descendant(scope, name, 0) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
                    return Err(self.pending());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(empty)
    }

    fn rescan_after_busy(
        &self,
        error: io::Error,
        scope: libc::c_int,
        leaf: Option<&CStr>,
        retry: &mut DirectoryRetry,
    ) -> Result<bool> {
        let busy = matches!(
            error.kind(),
            io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
        ) || error.raw_os_error() == Some(libc::EBUSY);
        if !busy || !self.is_nfs() {
            return Err(error.into());
        }
        #[cfg(test)]
        AFTER_NFS_REMOVAL_FAILURE.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(scope);
            }
        });
        if let Some(name) = leaf {
            match marker_entry(scope, name) {
                // A locally held placeholder can disappear after unlink failed.
                Ok(None) => return Ok(false),
                Ok(Some(entry))
                    if error.raw_os_error() == Some(libc::EBUSY)
                        && pending_nfs_entry(name, &entry) =>
                {
                    return Err(self.pending());
                }
                _ => return Err(error.into()),
            }
        }
        // A failed rmdir gives no leaf name. Require an actual busy unlink
        // before treating one of its remaining entries as a held NFS file.
        if error.raw_os_error() == Some(libc::EBUSY) {
            if let Err(pending) = self.inspect_remaining(scope) {
                if pending.is::<PendingGroupCleanup>() {
                    return Err(pending);
                }
            }
            return Err(error.into());
        }
        *retry = match *retry {
            DirectoryRetry::Initial => DirectoryRetry::Rescan,
            // A deferred entry can disappear after rmdir returned ENOTEMPTY.
            // The final attempt only tries rmdir, even if a writer adds files.
            DirectoryRetry::Rescan if self.inspect_remaining(scope)? => DirectoryRetry::Empty,
            _ => {
                return Err(self.retained_record_error(
                    error,
                    "Temporary directory remained busy after a cleanup retry",
                    self.can_retry,
                ))
            }
        };
        anyhow::ensure!(
            same_run_directory(self.path.parent().unwrap(), self.group)?
                && same_run_directory(self.path, self.directory)?,
            "temporary run or group changed before retrying cleanup"
        );
        ensure_unmounted(self.path, None)?;
        Ok(*retry == DirectoryRetry::Rescan)
    }

    fn retry_directory(
        &self,
        frame: &mut DirectoryRemoval,
        error: io::Error,
        leaf: Option<&CStr>,
    ) -> Result<bool> {
        let fd = frame.entries.as_raw_fd();
        let result = (|| -> Result<()> {
            if self.rescan_after_busy(error, fd, leaf, &mut frame.retry)? {
                if let Some(name) = &frame.name {
                    check_removal_entry(frame.parent, name, &descriptor_stat(fd)?)?;
                }
                frame.entries = DirectoryEntries::open_at(fd, c".")?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(true),
            Err(error)
                if error.downcast_ref().is_some_and(|error| {
                    directory_disappeared(frame.parent, frame.name.as_deref(), error)
                }) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn remove_contents(&self, retain: &[&CStr]) -> Result<()> {
        let mut stack = vec![DirectoryRemoval {
            entries: DirectoryEntries::open(self.directory)?,
            name: None,
            parent: self.group.as_raw_fd(),
            retry: DirectoryRetry::Initial,
        }];
        while let Some(frame) = stack.last_mut() {
            let fd = frame.entries.as_raw_fd();
            let next = if frame.retry == DirectoryRetry::Empty {
                None
            } else {
                match frame.entries.next_entry() {
                    Ok(next) => next,
                    Err(error)
                        if directory_disappeared(frame.parent, frame.name.as_deref(), &error) =>
                    {
                        stack.pop();
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            if let Some((name, kind)) = next {
                if frame.name.is_none() && retain.contains(&name) {
                    continue;
                }
                match remove_entry(fd, name, kind) {
                    Ok(Some(entries)) => {
                        let name = Some(name.to_owned());
                        stack.push(DirectoryRemoval {
                            entries,
                            name,
                            parent: fd,
                            retry: DirectoryRetry::Initial,
                        });
                    }
                    Ok(None) => {}
                    Err(error) if directory_disappeared(fd, Some(name), &error) => {}
                    Err(error) => {
                        let leaf = name.to_owned();
                        if !self.retry_directory(frame, error, Some(&leaf))? {
                            stack.pop();
                        }
                    }
                }
                continue;
            }
            let mut child = stack.pop().unwrap();
            let Some(name) = &child.name else {
                return Ok(());
            };
            let parent_fd = child.parent;
            #[cfg(test)]
            if REMOVE_OPEN_CHILD.replace(false) {
                unlink_entry(parent_fd, name, libc::AT_REMOVEDIR)?;
            }
            let result = descriptor_stat(fd).and_then(|expected| {
                check_removal_entry(parent_fd, name, &expected)?;
                unlink_descendant(parent_fd, name, libc::AT_REMOVEDIR)
            });
            match result {
                Ok(()) => {}
                Err(error) if directory_disappeared(parent_fd, Some(name), &error) => {}
                Err(error) => {
                    if self.retry_directory(&mut child, error, None)? {
                        stack.push(child);
                    }
                }
            }
        }
        unreachable!("root frame returns its result");
    }
}

pub(super) fn reclaim_run_contents(
    dir: &Path,
    group: &File,
    directory: &File,
    retain: &[&CStr],
) -> Result<()> {
    RunRemoval {
        path: dir,
        group,
        directory,
        can_retry: true,
    }
    .remove_contents(retain)
}

pub(super) fn remove_run_path(
    dir: &Path,
    group: &File,
    directory: &File,
    unfinished: bool,
    can_retry: bool,
) -> Result<()> {
    let name = CString::new(dir.file_name().unwrap().as_bytes())?;
    let group_fd = group.as_raw_fd();
    let root_fd = directory.as_raw_fd();
    let expected = descriptor_stat(root_fd)?;
    let target = RunRemoval {
        path: dir,
        group,
        directory,
        can_retry,
    };
    let removed = || removed_directory(group, &name, root_fd);
    let mut retry = DirectoryRetry::Initial;
    loop {
        #[cfg(test)]
        if RUN_RMDIR_WAS_BUSY.replace(false) {
            target.rescan_after_busy(
                io::Error::from_raw_os_error(libc::ENOTEMPTY),
                root_fd,
                None,
                &mut retry,
            )?;
            continue;
        }
        #[cfg(test)]
        if let Some(replace) = MOVE_RUN_BEFORE_REMOVAL.take() {
            fs::rename(dir, dir.with_extension("moved"))?;
            if replace {
                fs::create_dir(dir)?;
                fs::write(dir.join("replacement"), b"preserve")?;
            }
        }
        let prepared = (|| -> Result<()> {
            check_removal_entry(group_fd, &name, &expected)?;
            if !unfinished && retry != DirectoryRetry::Empty {
                target.remove_contents(&[])?;
                check_removal_entry(group_fd, &name, &expected)?;
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            return if removed() { Ok(()) } else { Err(error) };
        }

        #[cfg(test)]
        if let remaining @ 1.. = EXTRA_ROOT_ENTRIES.get() {
            EXTRA_ROOT_ENTRIES.set(remaining - 1);
            File::create(dir.join(EXTRA_ROOT_ENTRY_NAME.get()))?;
        }
        let removal = unlink_entry(group_fd, &name, libc::AT_REMOVEDIR);
        #[cfg(test)]
        let removal = simulate_lost_reply(removal, &RUN_RMDIR_LOST_REPLY);
        let Err(error) = removal else {
            return Ok(());
        };
        if removed() {
            return Ok(());
        }
        if entry_missing(group, &name) && target.is_nfs() {
            return Err(target.retained_record_error(
                error,
                "Temporary run removal could not be confirmed",
                true,
            ));
        }
        if let Err(error) = target.rescan_after_busy(error, root_fd, None, &mut retry) {
            return if removed() { Ok(()) } else { Err(error) };
        }
    }
}

#[cfg(test)]
type NfsRemovalHook = Box<dyn FnMut(libc::c_int)>;

#[cfg(all(test, target_os = "linux"))]
#[derive(Clone, Copy)]
pub(super) enum BeforeChildOpen {
    ReplaceWithSymlink,
    Stale { removed: bool },
}

#[cfg(test)]
std::thread_local! {
    pub(super) static FORCE_NFS_REMOVAL: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    pub(super) static RUN_RMDIR_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    pub(super) static DESCENDANT_UNLINK_LOST_REPLY: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    pub(super) static DESCENDANT_BUSY: std::cell::Cell<Option<&'static CStr>> = const {
        std::cell::Cell::new(None)
    };
    pub(super) static EXTRA_ROOT_ENTRIES: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    pub(super) static EXTRA_ROOT_ENTRY_NAME: std::cell::Cell<&'static str> = const {
        std::cell::Cell::new("late-entry")
    };
    pub(super) static RUN_RMDIR_ATTRIBUTES_UNAVAILABLE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    pub(super) static REMOVE_OPEN_CHILD: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    #[cfg(target_os = "linux")]
    pub(super) static BEFORE_CHILD_OPEN: std::cell::Cell<Option<BeforeChildOpen>> = const {
        std::cell::Cell::new(None)
    };
    pub(super) static RUN_RMDIR_WAS_BUSY: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    pub(super) static AFTER_NFS_REMOVAL_FAILURE: std::cell::RefCell<Option<NfsRemovalHook>> = const {
        std::cell::RefCell::new(None)
    };
    pub(super) static MOVE_RUN_BEFORE_REMOVAL: std::cell::Cell<Option<bool>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn nfs_entry_can_disappear_after_the_first_retry_check() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("run");
    fs::create_dir(&dir).unwrap();
    let group = File::open(root.path()).unwrap();
    let directory = File::open(&dir).unwrap();
    let fd = directory.as_raw_fd();
    let name = c".nfs0123456789abcdef00000001";
    let placeholder = dir.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
    fs::write(&placeholder, []).unwrap();
    assert!(remaining_entries(fd, pending_nfs_entry).unwrap().deferred);
    let target = RunRemoval {
        path: &dir,
        group: &group,
        directory: &directory,
        can_retry: true,
    };
    FORCE_NFS_REMOVAL.set(true);
    let busy = || io::Error::from_raw_os_error(libc::EBUSY);
    fs::write(dir.join("unrelated"), []).unwrap();
    let error = target
        .rescan_after_busy(busy(), fd, Some(c"unrelated"), &mut DirectoryRetry::Initial)
        .unwrap_err();
    assert_eq!(super::io_errno(&error), Some(libc::EBUSY));

    let path = placeholder.clone();
    AFTER_NFS_REMOVAL_FAILURE.replace(Some(Box::new(move |_| {
        fs::remove_file(&path).unwrap();
    })));
    let outcome = target.rescan_after_busy(busy(), fd, Some(name), &mut DirectoryRetry::Rescan);
    AFTER_NFS_REMOVAL_FAILURE.replace(None);
    FORCE_NFS_REMOVAL.set(false);
    assert!(!placeholder.exists());
    assert!(!outcome.unwrap());
}
