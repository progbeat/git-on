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

pub fn mount_is_ours(dir: &Path, id: &str) -> Result<bool> {
    let point = dir.join("mount");
    #[cfg(target_os = "macos")]
    let expected = nfs_mount_source(id);
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
pub fn nfs_socket(id: &str) -> Option<std::path::PathBuf> {
    let uuid = uuid::Uuid::parse_str(id).ok()?;
    (uuid.to_string() == id).then(|| format!("/private/tmp/git-on-nfs-{id}/s").into())
}

#[cfg(target_os = "macos")]
pub fn nfs_mount_source(id: &str) -> Option<String> {
    Some(format!("<{}>:/", nfs_socket(id)?.display()))
}

#[cfg(target_os = "macos")]
pub fn cleanup_nfs_socket(id: &str) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Some(socket) = nfs_socket(id) else {
        return Ok(());
    };
    let directory = socket.parent().context("missing NFS socket directory")?;
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect NFS socket directory"),
    };
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("NFS socket directory has unexpected type or ownership");
    }
    let source = nfs_mount_source(id).context("invalid NFS mount identity")?;
    if mounts()?.iter().any(|(_, current)| *current == source) {
        bail!("NFS socket still belongs to a system mount");
    }
    match std::fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
            std::fs::remove_file(&socket).context("remove private NFS socket")?;
        }
        Ok(_) => bail!("NFS socket has unexpected type or ownership"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("inspect private NFS socket"),
    }
    std::fs::remove_dir(directory).context("remove private NFS socket directory")
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
