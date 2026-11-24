//! Host-specific backend discovery and checked mount ownership. No configuration,
//! extension loading, privilege changes, or lazy unmounts are performed here.
use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
use std::{
    ffi::{CStr, CString, OsStr},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const TEMP_GROUP: &str = "git-on";
const PRIVATE_GROUP_PREFIX: &str = ".go";
// The owner-only file whose exclusive lock protects a published run.
pub const RUN_LOCK_NAME: &CStr = c".git-on-lock-v2";

fn private_group_name(uid: u32) -> String {
    format!("{PRIVATE_GROUP_PREFIX}{uid:x}")
}

fn private_group(name: &[u8]) -> bool {
    name.split_at_checked(PRIVATE_GROUP_PREFIX.len())
        .is_some_and(|(prefix, uid)| {
            prefix.eq_ignore_ascii_case(PRIVATE_GROUP_PREFIX.as_bytes())
                && (1..=8).contains(&uid.len())
                && uid.iter().all(u8::is_ascii_hexdigit)
        })
}

fn temporary_group_handle(group: &Path) -> io::Result<(fs::File, fs::Metadata)> {
    #[cfg(test)]
    if let Some(error) = temporary_group_tests::OPEN_ERROR.take() {
        return Err(io::Error::from_raw_os_error(error));
    }
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(group)?;
    let metadata = directory.metadata()?;
    Ok((directory, metadata))
}

fn same_group(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    (left.dev(), left.ino(), left.uid()) == (right.dev(), right.ino(), right.uid())
}

fn group_changed(path: &Path, previous: &fs::Metadata) -> bool {
    match fs::symlink_metadata(path) {
        Ok(current) => !same_group(&current, previous),
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

fn usable_group(metadata: &fs::Metadata, uid: u32, owner_permissions: u32) -> bool {
    let mode = metadata.permissions().mode();
    metadata.is_dir()
        && (metadata.uid() == uid
            && mode & owner_permissions == owner_permissions
            && (mode & 0o022 == 0 || mode & 0o1000 != 0)
            || metadata.uid() == 0 && mode & 0o1777 == 0o1777)
}

fn removable_temporary_group(metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.permissions().mode() & 0o1777 == 0o700
}

pub fn create_temporary_entry(parent: &Path, name: &str) -> io::Result<PathBuf> {
    create_entry_in_group(parent, name, true, |_, directory, _, name| {
        make_temporary_directory(directory, name)
    })
    .map(|(path, ())| path)
}

pub fn create_temporary_entry_with<T>(
    parent: &Path,
    name: &str,
    create: impl FnMut(&Path, &fs::File, &fs::Metadata, &CStr) -> io::Result<T>,
) -> io::Result<(PathBuf, T)> {
    create_entry_in_group(parent, name, true, create)
}

pub fn reopen_temporary_group(
    group: &Path,
    expected: &fs::Metadata,
) -> io::Result<Option<fs::File>> {
    match temporary_group_handle(group) {
        Ok((directory, opened)) => Ok(same_group(&opened, expected).then_some(directory)),
        Err(_) if group_changed(group, expected) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn make_temporary_directory(directory: &fs::File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub struct DirectoryEntries(*mut libc::DIR);

/// Open a physical child without crossing a mount that appeared after the
/// caller checked the run. Bind mounts need more than a device-number check.
pub fn open_child_directory(
    parent: RawFd,
    name: &CStr,
    access: libc::c_int,
) -> io::Result<fs::File> {
    let flags = access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    {
        let mut options: libc::open_how = unsafe { std::mem::zeroed() };
        options.flags = flags as u64;
        options.resolve = libc::RESOLVE_NO_XDEV | libc::RESOLVE_BENEATH;
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                parent,
                name.as_ptr(),
                &options,
                std::mem::size_of_val(&options),
            )
        };
        if fd >= 0 {
            return Ok(unsafe { fs::File::from_raw_fd(fd as RawFd) });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS | libc::EINVAL | libc::EPERM)
        ) {
            return Err(error);
        }
    }
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if cleanup_mount(parent)? != cleanup_mount(fd)? {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn cleanup_mount(fd: RawFd) -> io::Result<u64> {
    fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| io::Error::other("missing mount ID for directory"))
}

#[cfg(target_os = "macos")]
fn cleanup_mount(fd: RawFd) -> io::Result<[libc::c_char; libc::MAXPATHLEN as usize]> {
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd, &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.f_mntonname)
}

impl DirectoryEntries {
    pub fn open(directory: &fs::File) -> io::Result<Self> {
        // Use a stream position independent of the caller's descriptor.
        Self::open_at(directory.as_raw_fd(), c".")
    }

    pub fn open_at(parent: RawFd, name: &CStr) -> io::Result<Self> {
        Self::from_file(open_child_directory(parent, name, libc::O_RDONLY)?)
    }

    pub(crate) fn from_file(directory: fs::File) -> io::Result<Self> {
        let stream = unsafe { libc::fdopendir(directory.as_raw_fd()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _ = directory.into_raw_fd(); // closedir owns the descriptor now.
        Ok(Self(stream))
    }

    pub fn next_name(&mut self) -> io::Result<Option<&CStr>> {
        Ok(self.next_entry()?.map(|(name, _)| name))
    }

    pub fn next_entry(&mut self) -> io::Result<Option<(&CStr, libc::c_uchar)>> {
        #[cfg(target_os = "linux")]
        let errno = unsafe { libc::__errno_location() };
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        loop {
            // readdir returns null at EOF and on failure.
            let entry = unsafe {
                *errno = 0;
                libc::readdir(self.0)
            };
            if entry.is_null() {
                let code = unsafe { *errno };
                return if code == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::from_raw_os_error(code))
                };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name != c"." && name != c".." {
                return Ok(Some((name, unsafe { (*entry).d_type })));
            }
        }
    }
}

impl AsRawFd for DirectoryEntries {
    fn as_raw_fd(&self) -> RawFd {
        unsafe { libc::dirfd(self.0) }
    }
}

impl Drop for DirectoryEntries {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn group_owner_matches(owner: u32, uid: u32, allow_shared_group: bool) -> bool {
    owner == uid || allow_shared_group && owner == 0
}

fn create_entry_in_group<T>(
    parent: &Path,
    name: &str,
    allow_shared_group: bool,
    mut create: impl FnMut(&Path, &fs::File, &fs::Metadata, &CStr) -> io::Result<T>,
) -> io::Result<(PathBuf, T)> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let uid = unsafe { libc::geteuid() };
    let name_c = CString::new(name).map_err(io::Error::other)?;
    let mut group = parent.join(TEMP_GROUP);
    let mut fallback = false;
    let mut refreshed_stale_handle = false;
    loop {
        let created = match fs::DirBuilder::new().mode(0o700).create(&group) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        let metadata = match fs::symlink_metadata(&group) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                if created {
                    let _ = remove_empty_temporary_group(&group);
                }
                return Err(error);
            }
        };
        if !group_owner_matches(metadata.uid(), uid, allow_shared_group)
            || !usable_group(&metadata, uid, 0o700)
        {
            if created && metadata.is_dir() && metadata.uid() == uid {
                let _ = fs::remove_dir(&group);
            }
            // Another user can move entries in their sticky directory.
            // A regular file also occupies the name without providing a group.
            if !fallback && (metadata.uid() != uid || metadata.is_file()) {
                fallback = true;
                group = parent.join(private_group_name(uid));
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary git-on group must be a real directory with trusted ownership and permissions",
            ));
        }
        let (directory, opened) = match temporary_group_handle(&group) {
            Ok(opened) => opened,
            Err(_) if group_changed(&group, &metadata) => continue,
            Err(error) => {
                if created {
                    let _ = remove_empty_temporary_group(&group);
                }
                return Err(error);
            }
        };
        if !same_group(&opened, &metadata) {
            continue;
        }
        // Operations relative to this handle cannot create an entry in a
        // replacement group if another invocation removed our group.
        match create(&group, &directory, &opened, &name_c) {
            Ok(reservation) => return Ok((group.join(name), reservation)),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && group_changed(&group, &opened) => {}
            Err(error)
                if error.raw_os_error() == Some(libc::ESTALE)
                    && (!refreshed_stale_handle || group_changed(&group, &opened)) =>
            {
                // Reopen the group path on NFS after a stale directory fd.
                refreshed_stale_handle = true;
            }
            Err(error) => {
                let _ = remove_empty_temporary_group(&group);
                return Err(error);
            }
        }
    }
}

pub fn remove_empty_temporary_group(group: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(group) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if removable_temporary_group(&metadata) {
        match fs::remove_dir(group) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::DirectoryNotEmpty
                        | io::ErrorKind::AlreadyExists
                ) => {}
            // Another finishing run may have removed our group and another
            // user may already own the new directory at this pathname.
            Err(_) if group_changed(group, &metadata) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn grouped_run(dir: &Path) -> bool {
    dir.parent().and_then(Path::file_name).is_some_and(|name| {
        let name = name.as_encoded_bytes();
        name.eq_ignore_ascii_case(TEMP_GROUP.as_bytes()) || private_group(name)
    }) && dir.file_name().is_some_and(|name| {
        let bytes = name.as_encoded_bytes();
        bytes.len() == 6 && bytes.iter().all(u8::is_ascii_alphanumeric)
    })
}

pub fn usable_run_group(group: &Path, metadata: &fs::Metadata) -> bool {
    let uid = unsafe { libc::geteuid() };
    group.file_name().is_some_and(|name| {
        let name = name.as_encoded_bytes();
        (name.eq_ignore_ascii_case(TEMP_GROUP.as_bytes())
            || name.eq_ignore_ascii_case(private_group_name(uid).as_bytes()))
            && usable_group(metadata, uid, 0o700)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunIdentity {
    FileHandle {
        handle: uuid::Uuid,
        creation_time: Option<uuid::Uuid>,
    },
    CreationTime(uuid::Uuid),
    Legacy(uuid::Uuid),
    Unverified(uuid::Uuid),
}

impl RunIdentity {
    pub fn primary(self) -> uuid::Uuid {
        match self {
            Self::FileHandle { handle, .. } => handle,
            Self::CreationTime(id) | Self::Legacy(id) | Self::Unverified(id) => id,
        }
    }

    pub fn matches(self, bound: Self) -> bool {
        if let (
            Self::FileHandle { handle: actual, .. },
            Self::FileHandle {
                handle: expected, ..
            },
        ) = (self, bound)
        {
            return actual == expected;
        }

        self.creation_time()
            .is_some_and(|actual| bound.creation_time() == Some(actual))
    }

    fn creation_time(self) -> Option<uuid::Uuid> {
        match self {
            Self::FileHandle { creation_time, .. } => creation_time,
            Self::CreationTime(id) => Some(id),
            Self::Legacy(_) | Self::Unverified(_) => None,
        }
    }
}

pub fn run_identity(dir: &Path) -> Result<Option<(fs::File, Option<RunIdentity>)>> {
    if !grouped_run(dir) {
        return Ok(None);
    }
    let (prefix, suffix) = dir.file_name().unwrap().as_encoded_bytes().split_at(1);
    if !matches!(prefix, b"g" | b"r") || !suffix.iter().all(u8::is_ascii_hexdigit) {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(dir)?;
    let mode = metadata.mode() & 0o1777;
    if mode != 0o700 {
        anyhow::ensure!(prefix != b"r", "temporary run has unexpected permissions");
        return Ok(None);
    }
    let uid = unsafe { libc::geteuid() };
    let group = dir.parent().unwrap();
    if !metadata.is_dir()
        || metadata.uid() != uid
        || !usable_run_group(group, &fs::symlink_metadata(group)?)
    {
        bail!("temporary run has unexpected group or owner");
    }
    let (directory, opened) = temporary_group_handle(dir)?;
    if !same_group(&metadata, &opened) || opened.mode() & 0o1777 != mode {
        bail!("temporary run changed while opening it");
    }
    let identity = run_ids(dir.file_name().unwrap(), &directory, &opened)?;
    Ok(Some((directory, identity)))
}

#[cfg(target_os = "linux")]
pub fn run_ids(
    name: &OsStr,
    directory: &fs::File,
    opened: &fs::Metadata,
) -> io::Result<Option<RunIdentity>> {
    let metadata = LinuxRunMetadata {
        inode: opened.ino(),
        uid: opened.uid(),
        device: opened.dev(),
        created: opened.created().ok(),
    };
    linux_run_ids(name, metadata, |identity| {
        add_linux_file_handle(identity, directory)
    })
}

#[cfg(target_os = "macos")]
pub fn run_ids(
    name: &OsStr,
    _directory: &fs::File,
    opened: &fs::Metadata,
) -> io::Result<Option<RunIdentity>> {
    use std::os::macos::fs::MetadataExt;
    Ok(darwin_run_id(
        name,
        opened.ino(),
        opened.uid(),
        (opened.st_birthtime(), opened.st_birthtime_nsec()),
    ))
}

#[cfg(target_os = "macos")]
fn darwin_run_id(name: &OsStr, inode: u64, uid: u32, birth: (i64, i64)) -> Option<RunIdentity> {
    // Darwin reports zero when the filesystem does not expose a birth time.
    if name.as_encoded_bytes().first() == Some(&b'r') && birth == (0, 0) {
        return None;
    }
    let mut identity = run_identity_hasher(name, inode, uid);
    // Creation time distinguishes equal inodes across different incarnations.
    identity.update(&birth.0.to_le_bytes());
    identity.update(&birth.1.to_le_bytes());
    Some(RunIdentity::CreationTime(finish_run_id(identity)))
}

fn run_identity_hasher(name: &OsStr, inode: u64, uid: u32) -> blake3::Hasher {
    let mut identity = blake3::Hasher::new_derive_key("git-on temporary run identity v2");
    // The name and inode survive an ancestor rename.
    identity.update(&inode.to_le_bytes());
    identity.update(&uid.to_le_bytes());
    identity.update(name.as_encoded_bytes());
    identity
}

fn finish_run_id(identity: blake3::Hasher) -> uuid::Uuid {
    let hash = identity.finalize();
    uuid::Uuid::from_bytes(hash.as_bytes()[..16].try_into().unwrap())
}

#[cfg(target_os = "linux")]
struct LinuxRunMetadata {
    inode: u64,
    uid: u32,
    device: u64,
    created: Option<std::time::SystemTime>,
}

#[cfg(target_os = "linux")]
fn linux_run_ids(
    name: &OsStr,
    metadata: LinuxRunMetadata,
    add_handle: impl FnOnce(&mut blake3::Hasher) -> io::Result<()>,
) -> io::Result<Option<RunIdentity>> {
    let mut identity = run_identity_hasher(name, metadata.inode, metadata.uid);
    if name.as_encoded_bytes().starts_with(b"g") {
        // Preserve the device field used by the published g-prefix protocol.
        identity.update(&metadata.device.to_le_bytes());
        return Ok(Some(RunIdentity::Legacy(finish_run_id(identity))));
    }
    let creation = metadata.created.map(|created| {
        let mut hash = identity.clone();
        add_linux_creation_time(&mut hash, created);
        finish_run_id(hash)
    });
    // The NFS client may present a different st_ino for the same handle.
    let mut identity = blake3::Hasher::new_derive_key("git-on temporary run file handle");
    identity.update(&metadata.uid.to_le_bytes());
    identity.update(name.as_encoded_bytes());
    match add_handle(&mut identity) {
        Ok(()) => Ok(Some(RunIdentity::FileHandle {
            handle: finish_run_id(identity),
            creation_time: creation,
        })),
        Err(error)
            if creation.is_some()
                || matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                )
                || matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EOVERFLOW)) =>
        {
            Ok(creation.map(RunIdentity::CreationTime))
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn add_linux_file_handle(identity: &mut blake3::Hasher, file: &fs::File) -> io::Result<()> {
    const MAX_HANDLE_SIZE: usize = 128; // Linux MAX_HANDLE_SZ.
    #[repr(C)]
    struct FileHandle {
        size: libc::c_uint,
        kind: libc::c_int,
        bytes: [u8; MAX_HANDLE_SIZE],
    }
    let mut handle = FileHandle {
        size: MAX_HANDLE_SIZE as libc::c_uint,
        kind: 0,
        bytes: [0; MAX_HANDLE_SIZE],
    };
    let mut mount_id: libc::c_int = 0;
    // The persistent handle distinguishes inode reuse and stays the same
    // across remounts of the same NFS export.
    if unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            file.as_raw_fd(),
            c"".as_ptr(),
            &mut handle as *mut FileHandle,
            &mut mount_id as *mut libc::c_int,
            libc::AT_EMPTY_PATH,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let bytes = handle
        .bytes
        .get(..handle.size as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid file handle length"))?;
    identity.update(b"handle");
    identity.update(&handle.kind.to_le_bytes());
    identity.update(bytes);
    Ok(())
}

#[cfg(target_os = "linux")]
fn add_linux_creation_time(identity: &mut blake3::Hasher, created: std::time::SystemTime) {
    let (before_epoch, timestamp) = match created.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => (false, duration),
        Err(error) => (true, error.duration()),
    };
    identity.update(b"created");
    identity.update(&[u8::from(before_epoch)]);
    identity.update(&timestamp.as_secs().to_le_bytes());
    identity.update(&timestamp.subsec_nanos().to_le_bytes());
}

pub fn same_run_directory(dir: &Path, locked: &fs::File) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(dir)?;
    Ok(metadata.is_dir() && same_group(&metadata, &locked.metadata()?))
}

pub fn run_directory_matches(dir: &Path, expected: &fs::Metadata) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(dir)?;
    Ok(metadata.is_dir() && same_group(&metadata, expected))
}

pub fn ssh_control_path(run: &Path) -> PathBuf {
    let private = run
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| private_group(name.as_encoded_bytes()));
    // Preserve room for OpenSSH's temporary socket suffix with any UID.
    run.join(if private && grouped_run(run) {
        "s"
    } else {
        "ssh-control"
    })
}

pub fn mount_unavailable() -> Result<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        match fs::metadata("/dev/fuse") {
            Ok(meta) if meta.file_type().is_char_device() => {}
            Ok(_) => return Ok(Some("/dev/fuse is not a character device".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some("/dev/fuse is absent".into()))
            }
            Err(e) => return Err(e).context("inspect /dev/fuse"),
        }
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
        {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EACCES | libc::EPERM | libc::ENODEV | libc::ENXIO)
                ) =>
            {
                return Ok(Some(format!("/dev/fuse cannot be opened: {e}")))
            }
            Err(e) => return Err(e).context("open /dev/fuse"),
        }
        use std::os::unix::fs::PermissionsExt;
        if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|p| {
            fs::metadata(p.join("fusermount3"))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }) {
            return Ok(Some("fusermount3 is unavailable on PATH".into()));
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        let mut raw = [0u8; 64];
        let mut len = raw.len();
        // Darwin 25 corresponds to macOS 26. uname/sysctl is available
        // on the full macOS deployment range and needs no subprocess.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.osrelease".as_ptr(),
                raw.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("read Darwin version");
        }
        let version = CStr::from_bytes_with_nul(&raw[..len])
            .context("decode Darwin version")?
            .to_str()?;
        let major = version
            .split('.')
            .next()
            .unwrap_or_default()
            .parse::<u32>()?;
        if major < 25 {
            return Ok(Some(
                "the NFSv4.1 backend requires macOS 26 or later".into(),
            ));
        }
        if !Path::new("/sbin/mount_nfs").is_file() {
            return Ok(Some("/sbin/mount_nfs is unavailable".into()));
        }
        Ok(None)
    }
    #[cfg(target_os = "linux")]
    Ok(None)
}

pub fn mount_is_ours(
    dir: &Path,
    id: &str,
    #[cfg(target_os = "macos")] recorded_socket: Option<&Path>,
) -> Result<bool> {
    let point = dir.join("mount");
    #[cfg(target_os = "macos")]
    let expected = if recorded_socket.is_some() {
        selected_nfs_socket(Path::new(NFS_SOCKET_BASE), id, recorded_socket)?
            .into_iter()
            .collect::<Vec<_>>()
    } else if valid_nfs_socket_id(id) {
        nfs_socket_paths(Path::new(NFS_SOCKET_BASE), id).into()
    } else {
        Vec::new()
    };
    #[cfg(target_os = "linux")]
    let expected = format!("git-on:{id}");
    let mut found = false;
    for Mount { path, source, .. } in mounts()? {
        if path == point {
            #[cfg(target_os = "macos")]
            let matches = expected
                .iter()
                .any(|socket| nfs_mount_source(socket) == source);
            #[cfg(target_os = "linux")]
            let matches = source == expected;
            if !matches || found {
                bail!("mountpoint is occupied by a different or stacked filesystem");
            }
            found = true;
        }
    }
    Ok(found)
}

#[cfg(target_os = "macos")]
const NFS_SOCKET_BASE: &str = "/private/tmp";

#[cfg(target_os = "macos")]
pub fn reserve_nfs_socket(id: &str) -> io::Result<PathBuf> {
    // A new run derives its endpoint during cleanup. Reserve only in a
    // caller-owned group so an unrelated, inaccessible shared group cannot
    // be mistaken for one holding the endpoint.
    create_entry_in_group(
        Path::new(NFS_SOCKET_BASE),
        &format!("nfs-{id}"),
        false,
        |_, directory, _, name| make_temporary_directory(directory, name),
    )
    .map(|(path, ())| path)
    .map(|dir| dir.join("s"))
}

#[cfg(target_os = "macos")]
fn valid_nfs_socket_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

#[cfg(target_os = "macos")]
fn nfs_socket_groups(base: &Path, uid: u32) -> [PathBuf; 2] {
    [base.join(TEMP_GROUP), base.join(private_group_name(uid))]
}

#[cfg(target_os = "macos")]
fn nfs_socket_paths(base: &Path, id: &str) -> [PathBuf; 3] {
    let directory = format!("nfs-{id}");
    let [primary, private] = nfs_socket_groups(base, unsafe { libc::geteuid() })
        .map(|group| group.join(&directory).join("s"));
    [
        primary,
        private,
        base.join(format!("git-on-nfs-{id}")).join("s"),
    ]
}

#[cfg(target_os = "macos")]
pub fn legacy_nfs_socket(id: &str) -> Option<PathBuf> {
    valid_nfs_socket_id(id).then(|| {
        let [_, _, legacy] = nfs_socket_paths(Path::new(NFS_SOCKET_BASE), id);
        legacy
    })
}

#[cfg(target_os = "macos")]
fn selected_nfs_socket(base: &Path, id: &str, recorded: Option<&Path>) -> Result<Option<PathBuf>> {
    if !valid_nfs_socket_id(id) {
        if recorded.is_some() {
            bail!("invalid recorded NFS socket identity");
        }
        return Ok(None);
    }
    let uid = unsafe { libc::geteuid() };
    let candidates = nfs_socket_paths(base, id);
    if let Some(recorded) = recorded {
        return candidates
            .into_iter()
            .find(|path| path == recorded)
            .map(Some)
            .context("invalid recorded NFS socket path");
    }
    let mut selected = None;
    let mut inaccessible_group = None;
    for socket in candidates {
        let directory = socket.parent().unwrap();
        let group = directory.parent().unwrap();
        let group_usable = if group == base {
            true
        } else {
            match fs::symlink_metadata(group) {
                Ok(metadata)
                    if metadata.is_dir() && group_owner_matches(metadata.uid(), uid, false) =>
                {
                    usable_group(&metadata, uid, 0o100)
                }
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("inspect NFS socket group"),
            }
        };
        match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                anyhow::ensure!(
                    group_usable,
                    "NFS socket group {} is inaccessible or changed",
                    group.display()
                );
                anyhow::ensure!(
                    owned_nfs_socket_directory(&metadata, uid),
                    "NFS socket directory {} has unexpected type, ownership or permissions",
                    directory.display()
                );
                if selected.replace(socket).is_some() {
                    bail!("multiple private NFS socket directories match this run");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) if !group_usable && error.kind() == io::ErrorKind::PermissionDenied => {
                // This group may have lost access since allocation. Another
                // layout can still identify the run's endpoint.
                inaccessible_group.get_or_insert_with(|| group.to_path_buf());
            }
            Err(error) => return Err(error).context("inspect NFS socket directory"),
        }
    }
    if let (None, Some(group)) = (&selected, inaccessible_group) {
        bail!(
            "NFS socket group {} is inaccessible or changed",
            group.display()
        );
    }
    Ok(selected)
}

#[cfg(target_os = "macos")]
fn owned_nfs_socket_directory(metadata: &fs::Metadata, uid: u32) -> bool {
    metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
}

#[cfg(target_os = "macos")]
pub fn nfs_mount_source(socket: &Path) -> String {
    format!("<{}>:/", socket.display())
}

#[cfg(target_os = "macos")]
pub fn cleanup_nfs_socket(id: &str, recorded: Option<&Path>) -> Result<()> {
    cleanup_nfs_socket_under(Path::new(NFS_SOCKET_BASE), id, recorded)
}

#[cfg(target_os = "macos")]
fn cleanup_nfs_socket_under(base: &Path, id: &str, recorded: Option<&Path>) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let uid = unsafe { libc::geteuid() };
    let Some(socket) = selected_nfs_socket(base, id, recorded)? else {
        if valid_nfs_socket_id(id) {
            for group in nfs_socket_groups(base, uid) {
                remove_empty_temporary_group(&group)
                    .context("remove temporary NFS socket group")?;
            }
        }
        return Ok(());
    };
    let directory = socket.parent().unwrap();
    let group = directory.parent().unwrap();
    let grouped = group != base;
    if grouped {
        match fs::symlink_metadata(group) {
            Ok(metadata) if usable_group(&metadata, uid, 0o100) => {}
            Ok(_) => bail!("recorded NFS socket group is inaccessible or changed"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect NFS socket group"),
        }
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if grouped {
                remove_empty_temporary_group(group).context("remove temporary NFS socket group")?;
            }
            return Ok(());
        }
        Err(error) => return Err(error).context("inspect NFS socket directory"),
    };
    if !owned_nfs_socket_directory(&metadata, uid) {
        bail!("NFS socket directory has unexpected type or ownership");
    }
    let source = nfs_mount_source(&socket);
    if mounts()?.iter().any(|mount| mount.source == source) {
        bail!("NFS socket still belongs to a system mount");
    }
    match fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
            fs::remove_file(&socket).context("remove private NFS socket")?;
        }
        Ok(_) => bail!("NFS socket has unexpected type or ownership"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("inspect private NFS socket"),
    }
    fs::remove_dir(directory).context("remove private NFS socket directory")?;
    if grouped {
        remove_empty_temporary_group(group).context("remove temporary NFS socket group")?;
    }
    Ok(())
}

pub fn ensure_unmounted(dir: &Path, sibling: Option<&Path>) -> Result<()> {
    for Mount { path, .. } in mounts()? {
        for root in std::iter::once(dir).chain(sibling) {
            if path.starts_with(root) {
                bail!(
                    "a mount remains below {}; refusing deletion",
                    root.display()
                );
            }
        }
    }
    Ok(())
}

struct Mount {
    path: PathBuf,
    source: String,
    nfs: bool,
    #[cfg(target_os = "linux")]
    id: u64,
}

/// Read mount information without asking the backing filesystem for statistics.
pub fn nfs_mount(_directory: &fs::File, _path: &Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let id = cleanup_mount(_directory.as_raw_fd())?;
        Ok(mounts()?
            .into_iter()
            .find(|mount| mount.id == id)
            .is_some_and(|mount| mount.nfs))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(mounts()?
            .into_iter()
            .filter(|mount| _path.starts_with(&mount.path))
            .max_by_key(|mount| mount.path.as_os_str().len())
            .is_some_and(|mount| mount.nfs))
    }
}

#[cfg(target_os = "linux")]
fn mounts() -> Result<Vec<Mount>> {
    let mut out = Vec::new();
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let sep = fields
            .iter()
            .position(|x| *x == "-")
            .context("invalid mountinfo")?;
        let path = fields.get(4).context("invalid mountinfo path")?;
        let source = fields.get(sep + 2).context("invalid mountinfo source")?;
        let kind = fields
            .get(sep + 1)
            .context("invalid mountinfo filesystem type")?;
        out.push(Mount {
            path: Path::new(std::ffi::OsStr::from_bytes(&decode_mount(path))).to_path_buf(),
            source: source.to_string(),
            nfs: matches!(*kind, "nfs" | "nfs4"),
            id: fields[0].parse()?,
        });
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
fn mounts() -> Result<Vec<Mount>> {
    use std::{
        ffi::{CStr, OsStr},
        os::unix::ffi::OsStrExt,
        sync::Mutex,
    };
    // getmntinfo owns a static buffer. Copy it while serialized.
    static MOUNTS: Mutex<()> = Mutex::new(());
    let _guard = MOUNTS
        .lock()
        .map_err(|_| anyhow::anyhow!("mount inventory lock poisoned"))?;
    let mut entries: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut entries, libc::MNT_NOWAIT) };
    if count <= 0 || entries.is_null() {
        return Err(std::io::Error::last_os_error()).context("read macOS mount inventory");
    }
    Ok(
        unsafe { std::slice::from_raw_parts(entries, count as usize) }
            .iter()
            .map(|entry| {
                let path = unsafe { CStr::from_ptr(entry.f_mntonname.as_ptr()) };
                let kind = unsafe { CStr::from_ptr(entry.f_fstypename.as_ptr()) };
                let nfs = kind.to_bytes() == b"nfs";
                let source = if nfs {
                    let from = unsafe { CStr::from_ptr(entry.f_mntfromname.as_ptr()) };
                    from.to_string_lossy().into_owned()
                } else {
                    String::new()
                };
                Mount {
                    path: Path::new(OsStr::from_bytes(path.to_bytes())).to_path_buf(),
                    source,
                    nfs,
                }
            })
            .collect(),
    )
}

#[cfg(target_os = "linux")]
pub fn decode_mount(s: &str) -> Vec<u8> {
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

pub fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    #[cfg(target_os = "linux")]
    {
        unsafe { info.si_pid() }
    }
    #[cfg(target_os = "macos")]
    {
        info.si_pid
    }
}
pub fn siginfo_status(info: &libc::siginfo_t) -> i32 {
    #[cfg(target_os = "linux")]
    {
        unsafe { info.si_status() }
    }
    #[cfg(target_os = "macos")]
    {
        info.si_status
    }
}

#[cfg(target_os = "linux")]
pub fn has_sys_admin() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("CapEff:")
                    .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
            })
        })
        .is_some_and(|bits| bits & (1 << 21) != 0)
}

#[cfg(test)]
mod temporary_group_tests {
    use super::*;

    thread_local! {
        pub(super) static OPEN_ERROR: std::cell::Cell<Option<i32>> = const {
            std::cell::Cell::new(None)
        };
    }

    #[test]
    fn shared_root_is_eligible_for_runs_but_not_derived_socket_groups() {
        let user = 501;
        assert!(group_owner_matches(0, user, true));
        assert!(!group_owner_matches(0, user, false));
        assert!(group_owner_matches(user, user, false));
    }

    #[test]
    fn group_open_error_removes_the_new_empty_group() {
        let temporary = tempfile::tempdir().unwrap();
        OPEN_ERROR.set(Some(libc::EIO));
        let error = create_temporary_entry(temporary.path(), "123456").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }

    #[test]
    fn create_reopens_a_group_after_its_handle_goes_stale() {
        let temporary = tempfile::tempdir().unwrap();
        let mut replace = true;
        let (path, ()) =
            create_temporary_entry_with(temporary.path(), "r12345", |group, directory, _, name| {
                if std::mem::take(&mut replace) {
                    fs::remove_dir(group)?;
                    return Err(io::Error::from_raw_os_error(libc::ESTALE));
                }
                make_temporary_directory(directory, name)
            })
            .unwrap();
        assert!(path.is_dir());
    }

    #[test]
    fn reopening_a_stale_group_skips_a_replacement_file() {
        let temporary = tempfile::tempdir().unwrap();
        let group = temporary.path().join("git-on");
        fs::create_dir(&group).unwrap();
        let expected = fs::metadata(&group).unwrap();
        fs::rename(&group, temporary.path().join("old-group")).unwrap();
        fs::write(&group, b"unrelated").unwrap();
        assert!(reopen_temporary_group(&group, &expected).unwrap().is_none());
        assert_eq!(fs::read(&group).unwrap(), b"unrelated");
    }

    #[test]
    fn unchanged_group_does_not_retry_missing_run_setup() {
        let temporary = tempfile::tempdir().unwrap();
        let mut attempts = 0;
        let result = create_temporary_entry_with(temporary.path(), "r12345", |_, _, _, _| {
            attempts += 1;
            if attempts == 1 {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOENT));
        assert_eq!(attempts, 1);
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_zero_birth_time_cannot_identify_a_new_run() {
        let new = OsStr::new("r123ab");
        assert_eq!(darwin_run_id(new, 42, 501, (0, 0)), None);
        assert!(darwin_run_id(new, 42, 501, (0, 1)).is_some());
        assert!(darwin_run_id(OsStr::new("g123ab"), 42, 501, (0, 0)).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_handle_survives_nfs_inode_and_device_number_changes() {
        let ids = |name, inode, device| {
            let metadata = LinuxRunMetadata {
                inode,
                uid: 501,
                device,
                created: Some(std::time::UNIX_EPOCH),
            };
            let identity = linux_run_ids(OsStr::new(name), metadata, |hash| {
                hash.update(b"fixture directory handle");
                Ok(())
            })
            .unwrap()
            .unwrap();
            identity
        };
        for name in ["r12345", "run-marker"] {
            let before = ids(name, 42, 19);
            let after = ids(name, 43, 20);
            assert_eq!(before.primary(), after.primary());
            assert!(before.matches(after));
        }
        assert_ne!(ids("g12345", 42, 19), ids("g12345", 42, 20));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conflicting_directory_handles_override_matching_creation_time() {
        let creation = uuid::Uuid::from_u128(1);
        let bound = RunIdentity::FileHandle {
            handle: uuid::Uuid::from_u128(2),
            creation_time: Some(creation),
        };
        let replacement = RunIdentity::FileHandle {
            handle: uuid::Uuid::from_u128(3),
            creation_time: Some(creation),
        };
        assert!(!replacement.matches(bound));
        assert!(RunIdentity::CreationTime(creation).matches(bound));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn denied_handle_lookup_without_creation_time_leaves_identity_unavailable() {
        let metadata = LinuxRunMetadata {
            inode: 42,
            uid: 501,
            device: 19,
            created: None,
        };
        let identity = linux_run_ids(OsStr::new("r12345"), metadata, |_| {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap();
        assert!(identity.is_none());
    }

    #[test]
    fn a_sticky_group_is_shared_only_if_system_owned() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let metadata = fs::symlink_metadata(directory.path()).unwrap();
        let owner = metadata.uid();
        let other = if owner == 1 { 2 } else { 1 };
        assert!(usable_group(&metadata, owner, 0o700));
        assert_eq!(usable_group(&metadata, other, 0o700), owner == 0);
    }

    #[test]
    fn a_replacement_is_distinct_from_the_original_group() {
        let temporary = tempfile::tempdir().unwrap();
        let group = temporary.path().join(TEMP_GROUP);
        fs::create_dir(&group).unwrap();
        let (_original, metadata) = temporary_group_handle(&group).unwrap();
        assert!(!group_changed(&group, &metadata));
        fs::remove_dir(&group).unwrap();
        assert!(group_changed(&group, &metadata));
        fs::create_dir(&group).unwrap();
        assert!(group_changed(&group, &metadata));
    }

    #[test]
    fn grouped_ssh_paths_fit_the_existing_path_budget() {
        let temporary = Path::new("/tmp");
        let previous = temporary.join("git-on-123456/ssh-control");
        let grouped = temporary.join(TEMP_GROUP).join("123456");
        let fallback = temporary.join(private_group_name(u32::MAX)).join("123456");
        let legacy = fallback.with_file_name("git-on-123456");
        assert_eq!(
            ssh_control_path(&grouped).as_os_str().len(),
            previous.as_os_str().len()
        );
        assert!(ssh_control_path(&fallback).as_os_str().len() <= previous.as_os_str().len());
        assert_eq!(ssh_control_path(&legacy), legacy.join("ssh-control"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_unsearchable_group_does_not_prevent_derived_socket_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let group = directory.path().join(TEMP_GROUP);
        fs::DirBuilder::new().mode(0o600).create(&group).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let [_, _, legacy_socket] = nfs_socket_paths(directory.path(), &id);
        let absent = cleanup_nfs_socket_under(directory.path(), &id, Some(&legacy_socket));
        let legacy = legacy_socket.parent().unwrap();
        fs::DirBuilder::new().mode(0o700).create(legacy).unwrap();
        let result = cleanup_nfs_socket_under(directory.path(), &id, None);
        fs::set_permissions(&group, fs::Permissions::from_mode(0o700)).unwrap();
        absent.unwrap();
        result.unwrap();
        assert!(group.is_dir());
        assert!(!legacy.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn derived_cleanup_can_retry_after_permissions_are_restored() {
        let temporary = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let group = temporary.path().join(TEMP_GROUP);
        let endpoint = group.join(format!("nfs-{id}"));
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&endpoint)
            .unwrap();
        for (path, mode) in [(&group, 0o600), (&endpoint, 0o750)] {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            let result = cleanup_nfs_socket_under(temporary.path(), &id, None);
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(result.unwrap_err().to_string().contains("NFS socket"));
            assert!(endpoint.is_dir());
        }
        cleanup_nfs_socket_under(temporary.path(), &id, None).unwrap();
        assert!(!group.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn derived_cleanup_removes_empty_groups_when_the_endpoint_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let uid = unsafe { libc::geteuid() };
        let group = directory.path().join(TEMP_GROUP);
        let private = directory.path().join(private_group_name(uid));
        for path in [&group, &private] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let another_run = private.join("another-run");
        fs::create_dir(&another_run).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        cleanup_nfs_socket_under(directory.path(), &id, None).unwrap();
        assert!(!group.exists());
        assert!(another_run.is_dir());
        fs::remove_dir(&another_run).unwrap();
        cleanup_nfs_socket_under(directory.path(), &id, None).unwrap();
        assert!(!private.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cleanup_reports_an_inaccessible_recorded_socket_group() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let group = temporary.path().join(TEMP_GROUP);
        let id = uuid::Uuid::new_v4().to_string();
        let socket_directory = group.join(format!("nfs-{id}"));
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&socket_directory)
            .unwrap();
        let socket = socket_directory.join("s");
        for (mode, reason) in [
            (0o500, "remove private NFS socket directory"),
            (0o600, "recorded NFS socket group is inaccessible"),
        ] {
            fs::set_permissions(&group, fs::Permissions::from_mode(mode)).unwrap();
            let result = cleanup_nfs_socket_under(temporary.path(), &id, Some(&socket));
            fs::set_permissions(&group, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(result.unwrap_err().to_string().contains(reason));
            assert!(socket_directory.exists());
        }
        fs::remove_dir(socket_directory).unwrap();
        cleanup_nfs_socket_under(temporary.path(), &id, Some(&socket)).unwrap();
        assert!(!group.exists());
    }
}
