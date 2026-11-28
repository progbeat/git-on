use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

const GROUP_NAME: &str = "git-on";
const OWNERSHIP: &CStr = c".git-on-owner";

pub struct Group {
    path: PathBuf,
    metadata: fs::Metadata,
    unpublished: bool,
}

struct Created {
    group: Group,
    path: PathBuf,
    id: String,
    registration: Option<(PathBuf, File)>,
}

impl Group {
    pub fn create() -> io::Result<(Self, PathBuf, String, (PathBuf, File))> {
        let created = Self::create_at(&std::env::temp_dir(), None)?;
        Ok((
            created.group,
            created.path,
            created.id,
            created
                .registration
                .expect("generated run has a registration"),
        ))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn create_named(base: &Path, name: &str) -> io::Result<(Self, PathBuf)> {
        let created = Self::create_at(base, Some(name))?;
        Ok((created.group, created.path))
    }

    fn create_at(base: &Path, name: Option<&str>) -> io::Result<Created> {
        Self::create_with(base, name, open_group)
    }

    fn create_with(
        base: &Path,
        name: Option<&str>,
        open: impl Fn(&Path) -> io::Result<File>,
    ) -> io::Result<Created> {
        let base = fs::canonicalize(if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            base
        })?;
        let uid = unsafe { libc::geteuid() };
        for group_name in [GROUP_NAME.to_owned(), private_name(uid)] {
            let path = base.join(group_name);
            let mut retried_stale = false;
            let mut retried_marker = false;
            loop {
                let created = match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => true,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(error) => return Err(error),
                };
                // Dispose open descriptors before attempting to retire the group.
                let mut owned_group = if created {
                    Some(Self {
                        metadata: path.symlink_metadata()?,
                        path: path.clone(),
                        unpublished: true,
                    })
                } else {
                    None
                };
                let directory = match open(&path) {
                    Ok(directory) => directory,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::ENOTDIR | libc::EACCES | libc::ELOOP)
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error),
                };
                let metadata = directory.metadata()?;
                if !usable(&metadata, uid)
                    || owned_group
                        .as_ref()
                        .is_some_and(|group| !group.matches_directory(&metadata, uid))
                {
                    break;
                }
                if created {
                    match write_marker(&directory) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    }
                } else if !registered(&path, &metadata, uid) {
                    if !retried_marker {
                        // A launch can be between mkdir and publishing ownership.
                        retried_marker = true;
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    break;
                }
                let group = owned_group.get_or_insert_with(|| Self {
                    path: path.clone(),
                    metadata,
                    unpublished: false,
                });
                // Hold registration until mkdirat has made this run visible.
                // Shared system groups are never retired by a launch.
                let _registration = if group.metadata.mode() & 0o1777 == 0o1777 {
                    None
                } else {
                    match lock_marker(&directory, uid, libc::LOCK_SH) {
                        Ok(marker) => Some(marker),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    }
                };
                group.unpublished = false;
                let id = uuid::Uuid::new_v4().to_string();
                let child = name.unwrap_or(&id[..8]);
                let entry = CString::new(child).map_err(io::Error::other)?;
                let fd = directory.as_raw_fd();
                let child_path = path.join(child);
                let mut registration = None;
                if name.is_none() {
                    match child_path.symlink_metadata() {
                        Ok(_) => continue,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    let marker = CString::new(format!("{RUN_PREFIX}{id}")).unwrap();
                    match register_run(&directory, &marker) {
                        Ok(lock) => registration = Some((marker, lock)),
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::AlreadyExists | io::ErrorKind::WouldBlock
                            ) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if unsafe { libc::mkdirat(fd, entry.as_ptr(), 0o700) } == 0 {
                    let registration = if let Some((marker, lock)) = registration {
                        match publish_registration(&directory, &child_path, &marker, &id) {
                            Ok(path) => Some((path, lock)),
                            Err(error) => {
                                if unsafe { libc::unlinkat(fd, entry.as_ptr(), libc::AT_REMOVEDIR) }
                                    == 0
                                {
                                    let _ = unlink_entry(&directory, &marker);
                                }
                                return Err(error);
                            }
                        }
                    } else {
                        None
                    };
                    return Ok(Created {
                        group: owned_group.take().unwrap(),
                        path: child_path,
                        id,
                        registration,
                    });
                }
                let error = io::Error::last_os_error();
                if let Some((marker, lock)) = registration {
                    let removed = unlink_entry(&directory, &marker);
                    drop(lock);
                    removed?;
                }
                if name.is_some() && error.kind() == io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
                if error.raw_os_error() == Some(libc::ESTALE) && !retried_stale {
                    retried_stale = true;
                } else if !matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) {
                    return Err(error);
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no usable private git-on group in the system temporary directory",
        ))
    }

    pub fn for_run(run: &Path) -> Option<Self> {
        short_id(run)?;
        Self::for_child(run)
    }

    pub fn for_run_parent(run: &Path) -> Option<Self> {
        short_id(run)?;
        Self::parent_group(run)
    }

    pub(crate) fn for_child(run: &Path) -> Option<Self> {
        let group = Self::parent_group(run)?;
        let uid = unsafe { libc::geteuid() };
        run.symlink_metadata()
            .is_ok_and(|run| run.is_dir() && run.uid() == uid && run.mode() & 0o777 == 0o700)
            .then_some(group)
    }

    fn parent_group(run: &Path) -> Option<Self> {
        let path = run.parent()?;
        let name = path.file_name()?;
        let uid = unsafe { libc::geteuid() };
        if name != GROUP_NAME && name != private_name(uid).as_str() {
            return None;
        }
        let metadata = path.symlink_metadata().ok()?;
        registered(path, &metadata, uid).then(|| Self {
            path: path.to_owned(),
            metadata,
            unpublished: false,
        })
    }
}

fn short_id(run: &Path) -> Option<&str> {
    run.file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.len() == 8 && name.as_bytes().iter().all(u8::is_ascii_hexdigit))
}

const RUN_PREFIX: &str = ".run-";

fn directory_stamp(directory: &fs::Metadata) -> String {
    format!("{:x}.{:x}", directory.dev(), directory.ino())
}

pub fn run_registration(run: &Path) -> io::Result<Option<(PathBuf, uuid::Uuid)>> {
    let stamp = match run.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => Some(directory_stamp(&metadata)),
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    find_registration(run, stamp.as_deref())
}

pub fn pending_registration(run: &Path) -> io::Result<Option<(PathBuf, uuid::Uuid)>> {
    find_registration(run, Some(""))
}

fn find_registration(run: &Path, stamp: Option<&str>) -> io::Result<Option<(PathBuf, uuid::Uuid)>> {
    let Some(short) = short_id(run) else {
        return Ok(None);
    };
    let uid = unsafe { libc::geteuid() };
    let mut registration = None;
    let parent = run
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(record) = name.to_str().and_then(|name| name.strip_prefix(RUN_PREFIX)) else {
            continue;
        };
        let (id, registered) = record.split_once('.').unwrap_or((record, ""));
        if !id.starts_with(short) || stamp.is_some_and(|stamp| stamp != registered) {
            continue;
        }
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            continue;
        };
        if id != uuid.to_string()
            || stamp.is_none()
                && !registered.is_empty()
                && !registered.split_once('.').is_some_and(|(device, inode)| {
                    u64::from_str_radix(device, 16).is_ok()
                        && u64::from_str_radix(inode, 16).is_ok()
                })
        {
            continue;
        }
        let metadata = entry.path().symlink_metadata()?;
        if metadata.is_file()
            && metadata.uid() == uid
            && metadata.mode() & 0o777 == 0o600
            && metadata.len() == 0
            && registration.replace((entry.path(), uuid)).is_some()
        {
            return Err(io::Error::other("ambiguous git-on run registration"));
        }
    }
    Ok(registration)
}

fn register_run(directory: &File, marker: &CStr) -> io::Result<File> {
    let lock = open_entry(
        directory,
        marker,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
    )?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            let _ = unlink_entry(directory, marker);
        }
        return Err(error);
    }
    Ok(lock)
}

fn publish_registration(
    directory: &File,
    run: &Path,
    marker: &CStr,
    id: &str,
) -> io::Result<PathBuf> {
    let name = format!(
        "{RUN_PREFIX}{id}.{}",
        directory_stamp(&run.symlink_metadata()?)
    );
    let ready = CString::new(name.as_str()).unwrap();
    let fd = directory.as_raw_fd();
    if unsafe { libc::renameat(fd, marker.as_ptr(), fd, ready.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(run.with_file_name(name))
}

#[cfg(target_os = "macos")]
pub(crate) fn child_locations(base: &Path, name: &str) -> [PathBuf; 2] {
    let uid = unsafe { libc::geteuid() };
    [
        base.join(GROUP_NAME).join(name),
        base.join(private_name(uid)).join(name),
    ]
}

#[cfg(target_os = "macos")]
pub(crate) fn nfs_cleanup_group(path: &Path, legacy: bool) -> io::Result<Option<Group>> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let uid = unsafe { libc::geteuid() };
    let mode = metadata.mode();
    let registered = registered(path, &metadata, uid);
    // Older NFS records name the group explicitly and predate ownership markers.
    let legacy_owner = legacy
        && metadata.is_dir()
        && metadata.uid() == uid
        && mode & libc::S_IXUSR as u32 != 0
        && (mode & 0o022 == 0 || mode & 0o1000 != 0);
    if !registered && !legacy_owner {
        if legacy {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recorded NFS socket group is inaccessible or changed",
            ));
        }
        return Ok(None);
    }
    Ok(Some(Group {
        path: path.to_owned(),
        metadata,
        unpublished: !registered && mode & 0o1777 == 0o700,
    }))
}

impl Drop for Group {
    fn drop(&mut self) {
        self.remove_when_empty(|path| fs::remove_dir(path));
    }
}

impl Group {
    fn remove_when_empty(&self, remove: impl FnOnce(&Path) -> io::Result<()>) {
        let uid = unsafe { libc::geteuid() };
        if self.unpublished {
            if self
                .path
                .symlink_metadata()
                .is_ok_and(|current| self.matches_directory(&current, uid))
            {
                let _ = remove(&self.path);
            }
            return;
        }
        if self.metadata.uid() != uid || self.metadata.mode() & 0o1777 != 0o700 {
            return;
        }
        let Some(directory) = self.open_private_group(uid) else {
            return;
        };
        let Ok(marker) = lock_marker(&directory, uid, libc::LOCK_EX) else {
            return;
        };
        // Keep the marker locked while checking for children, but free the
        // directory descriptor for read_dir under a tight descriptor limit.
        drop(directory);
        if !self.same_private_directory(uid)
            || !fs::read_dir(&self.path).is_ok_and(|mut entries| {
                entries.all(|entry| {
                    entry.is_ok_and(|entry| entry.file_name().as_bytes() == OWNERSHIP.to_bytes())
                })
            })
        {
            return;
        }
        let Some(directory) = self.open_private_group(uid) else {
            return;
        };
        if remove_marker(&directory).is_err() {
            return;
        }
        // A waiter holding the old inode must find the name gone after flock.
        // Close our handle before rmdir so NFS can finish unlinking it.
        drop(marker);
        if self.same_private_directory(uid)
            && remove(&self.path).is_err()
            && self.same_private_directory(uid)
        {
            let _ = write_marker(&directory);
        }
    }

    fn open_private_group(&self, uid: u32) -> Option<File> {
        let directory = open_group(&self.path).ok()?;
        directory
            .metadata()
            .is_ok_and(|opened| {
                opened.mode() & 0o1777 == 0o700 && self.matches_directory(&opened, uid)
            })
            .then_some(directory)
    }

    fn same_private_directory(&self, uid: u32) -> bool {
        self.path.symlink_metadata().is_ok_and(|current| {
            current.mode() & 0o1777 == 0o700 && self.matches_directory(&current, uid)
        })
    }

    fn matches_directory(&self, current: &fs::Metadata, uid: u32) -> bool {
        current.is_dir()
            && current.uid() == uid
            && current.ino() == self.metadata.ino()
            && current.dev() == self.metadata.dev()
    }
}

fn open_entry(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let flags = flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn write_marker(directory: &File) -> io::Result<()> {
    open_entry(
        directory,
        OWNERSHIP,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
    )
    .map(drop)
}

fn lock_marker(directory: &File, uid: u32, mode: libc::c_int) -> io::Result<File> {
    // NFS implements flock with byte range locks and needs a writable fd.
    let marker = open_entry(directory, OWNERSHIP, libc::O_RDWR)?;
    let metadata = marker.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o600 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    while unsafe { libc::flock(marker.as_raw_fd(), mode) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            OWNERSHIP.as_ptr(),
            current.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let current = unsafe { current.assume_init() };
    #[cfg(target_os = "linux")]
    let device = current.st_dev;
    #[cfg(target_os = "macos")]
    let device = current.st_dev as u64;
    if current.st_ino != metadata.ino() || device != metadata.dev() {
        return Err(io::ErrorKind::NotFound.into());
    }
    Ok(marker)
}

fn remove_marker(directory: &File) -> io::Result<()> {
    unlink_entry(directory, OWNERSHIP)
}

fn unlink_entry(directory: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn has_marker(path: &Path, uid: u32) -> bool {
    path.join(OsStr::from_bytes(OWNERSHIP.to_bytes()))
        .symlink_metadata()
        .is_ok_and(|marker| {
            marker.is_file() && marker.uid() == uid && marker.mode() & 0o777 == 0o600
        })
}

fn private_name(uid: u32) -> String {
    format!(".go{uid:x}")
}

fn open_group(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

fn usable(group: &fs::Metadata, uid: u32) -> bool {
    let mode = group.mode();
    group.is_dir()
        && (group.uid() == uid && mode & 0o722 == 0o700
            || group.uid() == 0 && mode & 0o1777 == 0o1777)
}

fn registered(path: &Path, group: &fs::Metadata, uid: u32) -> bool {
    usable(group, uid) && (group.mode() & 0o1777 == 0o1777 || has_marker(path, uid))
}

#[cfg(test)]
#[test]
fn opening_a_new_group_can_fail_without_leaving_a_directory() {
    let base = tempfile::tempdir().unwrap();
    let error = Group::create_with(base.path(), None, |path| {
        assert!(path.is_dir());
        Err(io::Error::from_raw_os_error(libc::EMFILE))
    })
    .err()
    .unwrap();
    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
    assert_eq!(fs::read_dir(base.path()).unwrap().count(), 0);
}

#[cfg(test)]
#[test]
fn group_cleanup_preserves_the_marker_at_a_reused_path() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(GROUP_NAME);
    let retired = root.path().join("retired");
    fs::create_dir(&path).unwrap();
    let original = open_group(&path).unwrap();
    write_marker(&original).unwrap();
    fs::rename(&path, &retired).unwrap();
    fs::create_dir(&path).unwrap();
    write_marker(&open_group(&path).unwrap()).unwrap();
    remove_marker(&original).unwrap();
    let marker = OsStr::from_bytes(OWNERSHIP.to_bytes());
    assert!(path.join(marker).is_file());
    assert!(!retired.join(marker).exists());
}

#[cfg(test)]
#[test]
fn launch_keeps_a_recoverable_group_if_removal_is_interrupted() {
    use std::{sync::mpsc, time::Duration};

    let base = tempfile::tempdir().unwrap();
    let Created {
        group: retiring,
        path: old_run,
        ..
    } = Group::create_at(base.path(), Some("0123abcd")).unwrap();
    fs::remove_dir(old_run).unwrap();
    let directory = open_group(&retiring.path).unwrap();
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .open(retiring.path.join(OsStr::from_bytes(OWNERSHIP.to_bytes())))
        .unwrap();
    assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX) }, 0);

    let path = base.path().to_owned();
    let (opened, entry) = mpsc::channel();
    let (created, run) = mpsc::channel();
    let launch = std::thread::spawn(move || {
        let result = Group::create_with(&path, Some("7654abcd"), |path| {
            let directory = open_group(path)?;
            let _ = opened.send(());
            Ok(directory)
        });
        created.send(result).unwrap();
    });
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let early = run.recv_timeout(Duration::from_millis(300)).ok();
    // Simulate death after removing the marker, before removing the directory.
    remove_marker(&directory).unwrap();
    drop(owner);
    let Created {
        group, path: child, ..
    } = early
        .unwrap_or_else(|| run.recv_timeout(Duration::from_secs(5)).unwrap())
        .unwrap();
    launch.join().unwrap();
    assert!(
        Group::for_run(&child).is_some(),
        "run lost its ownership group"
    );
    fs::remove_dir(child).unwrap();
    drop(group);
}

#[cfg(test)]
#[test]
fn failed_group_removal_preserves_registration_for_the_next_launch() {
    let base = tempfile::tempdir().unwrap();
    let Created {
        group, path: run, ..
    } = Group::create_at(base.path(), Some("0123abcd")).unwrap();
    fs::remove_dir(run).unwrap();
    group.remove_when_empty(|path| {
        assert_eq!(path, group.path);
        assert!(fs::read_dir(path).unwrap().next().is_none());
        Err(io::Error::from_raw_os_error(libc::EIO))
    });
    let Created {
        group: next,
        path: run,
        ..
    } = Group::create_at(base.path(), Some("7654abcd")).unwrap();
    assert_eq!(next.path, group.path);
    fs::remove_dir(run).unwrap();
    drop(next);
    assert!(!group.path.exists());
}
