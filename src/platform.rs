//! Host-specific backend discovery and checked mount ownership. No configuration,
//! extension loading, privilege changes, or lazy unmounts are performed here.
use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
use std::{
    ffi::CString,
    fs, io,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

const TEMP_GROUP: &str = "git-on";
const PRIVATE_GROUP_PREFIX: &str = ".go";

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

fn removable_group(metadata: &fs::Metadata, uid: u32) -> bool {
    metadata.uid() == uid && metadata.permissions().mode() & 0o1777 == 0o700
}

pub fn create_temporary_entry(parent: &Path, name: &str) -> io::Result<PathBuf> {
    create_temporary_entry_before(parent, name, |_| Ok(()))
}

fn create_temporary_entry_before(
    parent: &Path,
    name: &str,
    mut prepare: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
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
        if !usable_group(&metadata, uid, 0o700) {
            if created && metadata.is_dir() && metadata.uid() == uid {
                let _ = fs::remove_dir(&group);
            }
            // A sticky directory's owner can move its entries. Keep a separate
            // private group when another user already owns the usual name.
            if !fallback && metadata.uid() != uid {
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
        let path = group.join(name);
        if let Err(error) = prepare(&path) {
            let _ = remove_empty_temporary_group(&group);
            return Err(error);
        }
        // If the last invocation removed the empty group, mkdirat fails
        // instead of creating an entry in a replacement at the same path.
        if unsafe { libc::mkdirat(directory.as_raw_fd(), name_c.as_ptr(), 0o700) } == 0 {
            return Ok(path);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            let _ = remove_empty_temporary_group(&group);
            return Err(error);
        }
    }
}

pub fn remove_empty_temporary_group(group: &Path) -> io::Result<()> {
    let uid = unsafe { libc::geteuid() };
    let metadata = match fs::symlink_metadata(group) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && removable_group(&metadata, uid) {
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
    let expected = selected_nfs_socket(Path::new(NFS_SOCKET_BASE), id, recorded_socket)?
        .map(|socket| nfs_mount_source(&socket));
    #[cfg(target_os = "linux")]
    let expected = Some(format!("git-on:{id}"));
    let mut found = false;
    for (path, source) in mounts()? {
        if path == point {
            if expected.as_deref() != Some(source.as_str()) || found {
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
pub fn reserve_nfs_socket(
    id: &str,
    mut record: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    create_temporary_entry_before(Path::new(NFS_SOCKET_BASE), &format!("nfs-{id}"), |dir| {
        record(&dir.join("s"))
    })
    .map(|dir| dir.join("s"))
}

#[cfg(target_os = "macos")]
fn selected_nfs_socket(base: &Path, id: &str, recorded: Option<&Path>) -> Result<Option<PathBuf>> {
    if !uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
        if recorded.is_some() {
            bail!("invalid recorded NFS socket identity");
        }
        return Ok(None);
    }
    let uid = unsafe { libc::geteuid() };
    let directory = format!("nfs-{id}");
    let primary = base.join(TEMP_GROUP).join(&directory).join("s");
    let private = base
        .join(private_group_name(uid))
        .join(&directory)
        .join("s");
    match recorded {
        Some(socket) if socket == primary => Ok(Some(primary)),
        Some(socket) if socket == private => Ok(Some(private)),
        Some(_) => bail!("invalid recorded NFS socket path"),
        None => Ok(Some(base.join(format!("git-on-nfs-{id}")).join("s"))),
    }
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
    let Some(socket) = selected_nfs_socket(base, id, recorded)? else {
        return Ok(());
    };
    let directory = socket.parent().unwrap();
    let group = directory.parent().unwrap();
    let grouped = group != base;
    let uid = unsafe { libc::geteuid() };
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
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("NFS socket directory has unexpected type or ownership");
    }
    let source = nfs_mount_source(&socket);
    if mounts()?.iter().any(|(_, current)| *current == source) {
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

pub fn ensure_unmounted(dir: &Path) -> Result<()> {
    for (path, _) in mounts()? {
        if path.starts_with(dir) {
            bail!("a mount remains below {}; refusing deletion", dir.display());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mounts() -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let sep = fields
            .iter()
            .position(|x| *x == "-")
            .context("invalid mountinfo")?;
        let path = fields.get(4).context("invalid mountinfo path")?;
        let source = fields.get(sep + 2).context("invalid mountinfo source")?;
        out.push((
            Path::new(std::ffi::OsStr::from_bytes(&decode_mount(path))).to_path_buf(),
            source.to_string(),
        ));
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
fn mounts() -> Result<Vec<(PathBuf, String)>> {
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
                let source = if kind.to_bytes() == b"nfs" {
                    let from = unsafe { CStr::from_ptr(entry.f_mntfromname.as_ptr()) };
                    from.to_string_lossy().into_owned()
                } else {
                    String::new()
                };
                (
                    Path::new(OsStr::from_bytes(path.to_bytes())).to_path_buf(),
                    source,
                )
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
    fn group_open_error_removes_the_new_empty_group() {
        let temporary = tempfile::tempdir().unwrap();
        OPEN_ERROR.set(Some(libc::EIO));
        let error = create_temporary_entry(temporary.path(), "123456").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
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
    fn an_entry_is_created_only_after_its_recovery_step_succeeds() {
        let temporary = tempfile::tempdir().unwrap();
        let result = create_temporary_entry_before(temporary.path(), "nfs-example", |path| {
            assert!(!path.exists());
            Err(io::Error::other("recording failed"))
        });
        assert_eq!(result.unwrap_err().to_string(), "recording failed");
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
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
    fn an_unsearchable_group_does_not_prevent_legacy_socket_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let group = directory.path().join(TEMP_GROUP);
        fs::DirBuilder::new().mode(0o600).create(&group).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let legacy = directory.path().join(format!("git-on-nfs-{id}"));
        fs::DirBuilder::new().mode(0o700).create(&legacy).unwrap();
        let result = cleanup_nfs_socket_under(directory.path(), &id, None);
        fs::set_permissions(&group, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert!(group.is_dir());
        assert!(!legacy.exists());
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
