//! Host-specific backend discovery and checked mount ownership. No configuration,
//! extension loading, privilege changes, or lazy unmounts are performed here.
use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
use std::path::Path;

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
    let expected = nfs_sockets(Path::new(NFS_SOCKET_ROOT), id, recorded_socket)?;
    #[cfg(target_os = "linux")]
    let expected = format!("git-on:{id}");
    let mut found = false;
    for (path, source) in mounts()? {
        if path == point {
            #[cfg(target_os = "macos")]
            let matches = expected
                .iter()
                .any(|socket| source == nfs_mount_source(socket));
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
const NFS_SOCKET_ROOT: &str = "/private/tmp";

#[cfg(target_os = "macos")]
fn nfs_sockets(root: &Path, id: &str, recorded: Option<&Path>) -> Result<Vec<std::path::PathBuf>> {
    if !uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
        anyhow::ensure!(recorded.is_none(), "invalid recorded NFS socket identity");
        return Ok(Vec::new());
    }
    let mut sockets = crate::run_directory::child_locations(root, &format!("nfs-{id}"))
        .map(|path| path.join("s"))
        .to_vec();
    if let Some(recorded) = recorded {
        anyhow::ensure!(
            sockets.iter().any(|socket| socket == recorded),
            "invalid recorded NFS socket path"
        );
        sockets.retain(|socket| socket == recorded);
    } else {
        sockets.push(root.join(format!("git-on-nfs-{id}")).join("s"));
    }
    Ok(sockets)
}

#[cfg(target_os = "macos")]
pub(crate) fn prepare_nfs_socket(
    id: &str,
) -> std::io::Result<(crate::run_directory::Group, std::path::PathBuf)> {
    let (group, directory) = crate::run_directory::Group::create_named(
        Path::new(NFS_SOCKET_ROOT),
        &format!("nfs-{id}"),
    )?;
    Ok((group, directory.join("s")))
}

#[cfg(target_os = "macos")]
pub(crate) fn nfs_mount_source(socket: &Path) -> String {
    format!("<{}>:/", socket.display())
}

#[cfg(target_os = "macos")]
pub fn cleanup_nfs_socket(id: &str, recorded: Option<&Path>) -> Result<()> {
    cleanup_nfs_socket_under(Path::new(NFS_SOCKET_ROOT), id, recorded)
}

#[cfg(target_os = "macos")]
fn cleanup_nfs_socket_under(root: &Path, id: &str, recorded: Option<&Path>) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let uid = unsafe { libc::geteuid() };
    for socket in nfs_sockets(root, id, recorded)? {
        let directory = socket.parent().context("missing NFS socket directory")?;
        let parent = directory.parent().context("missing NFS socket group")?;
        let _group = if parent == root {
            None
        } else {
            let Some(group) = crate::run_directory::nfs_cleanup_group(parent, recorded.is_some())
                .context("inspect NFS socket group")?
            else {
                continue;
            };
            Some(group)
        };
        let metadata = match directory.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("inspect NFS socket directory"),
        };
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            bail!("NFS socket directory has unexpected type or ownership");
        }
        let source = nfs_mount_source(&socket);
        if mounts()?.iter().any(|(_, current)| *current == source) {
            bail!("NFS socket still belongs to a system mount");
        }
        match socket.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
                std::fs::remove_file(&socket).context("remove private NFS socket")?;
            }
            Ok(_) => bail!("NFS socket has unexpected type or ownership"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect private NFS socket"),
        }
        std::fs::remove_dir(directory).context("remove private NFS socket directory")?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn legacy_nfs_cleanup_removes_only_the_recorded_socket_group() {
    use std::os::unix::fs::DirBuilderExt;
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let [primary, private] =
        crate::run_directory::child_locations(root.path(), &format!("nfs-{id}"));
    for group in [&primary, &private] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(group)
            .unwrap();
    }
    let recorded = private.join("s");
    drop(std::os::unix::net::UnixListener::bind(&recorded).unwrap());
    cleanup_nfs_socket_under(root.path(), &id, None).unwrap();
    assert!(recorded.exists());
    cleanup_nfs_socket_under(root.path(), &id, Some(&recorded)).unwrap();
    assert!(!private.parent().unwrap().exists());
    assert!(primary.is_dir());
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
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_MOUNTPOINT: std::cell::RefCell<Option<std::path::PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(target_os = "linux")]
fn mounts() -> Result<Vec<(std::path::PathBuf, String)>> {
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
    #[cfg(test)]
    if let Some(path) = TEST_MOUNTPOINT.with_borrow(Clone::clone) {
        out.push((path, String::new()));
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
fn mounts() -> Result<Vec<(std::path::PathBuf, String)>> {
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
