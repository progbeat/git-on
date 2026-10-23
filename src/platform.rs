//! Host-specific FUSE discovery and checked mount ownership. No configuration,
//! extension loading, privilege changes, or lazy unmounts are performed here.
use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountIdentity {
    filesystem_id: [i32; 2],
    owner: u32,
    source: String,
}

pub fn fuse_unavailable() -> Result<Option<String>> {
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
        if !Path::new(
            "/Library/Filesystems/macfuse.fs/Contents/Frameworks/MFMount.framework/MFMount",
        )
        .is_file()
        {
            return Ok(Some(
                "official macFUSE FSKit MFMount transport is absent".into(),
            ));
        }
        if !Path::new("/Library/Filesystems/macfuse.fs/Contents/Resources/macfuse.app").is_dir() {
            return Ok(Some(
                "the official macFUSE application/helpers are absent".into(),
            ));
        }
    }
    Ok(None)
}

pub fn mount_is_ours(dir: &Path, id: &str, identity: Option<&MountIdentity>) -> Result<bool> {
    let point = dir.join("mount");
    #[cfg(target_os = "macos")]
    {
        let _ = id;
        let records = mac_mounts()?;
        let matches: Vec<_> = records
            .iter()
            .filter(|record| record.path == point)
            .collect();
        if matches.is_empty() {
            return Ok(false);
        }
        anyhow::ensure!(
            matches.len() == 1
                && matches[0].kind == "macfuse"
                && matches[0].identity.owner == unsafe { libc::geteuid() }
                && identity == Some(&matches[0].identity),
            "mountpoint is occupied by a different, stacked or unverified filesystem"
        );
        Ok(true)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = identity;
        let expected = format!("git-on:{id}");
        let mut found = false;
        for (path, source) in mounts()? {
            if path == point {
                if source != expected || found {
                    bail!("mountpoint is occupied by a different or stacked filesystem");
                }
                found = true;
            }
        }
        Ok(found)
    }
}

#[cfg(target_os = "macos")]
pub fn capture_mount_identity(dir: &Path) -> Result<MountIdentity> {
    let records = mac_mounts()?;
    let matches: Vec<_> = records
        .iter()
        .filter(|record| record.path == dir.join("mount"))
        .collect();
    anyhow::ensure!(
        matches.len() == 1
            && matches[0].kind == "macfuse"
            && matches[0].identity.owner == unsafe { libc::geteuid() },
        "expected one macFUSE mount owned by the current user"
    );
    Ok(matches[0].identity.clone())
}

#[cfg(target_os = "macos")]
pub fn verify_mount_protections(dir: &Path, read_only: bool) -> Result<()> {
    let records = mac_mounts()?;
    let record = records
        .iter()
        .find(|record| record.path == dir.join("mount"))
        .context("macFUSE mount disappeared")?;
    let required = (libc::MNT_NOSUID | libc::MNT_NODEV) as u32;
    anyhow::ensure!(
        record.flags & required == required,
        "macFUSE FSKit did not preserve required nosuid/nodev mount flags"
    );
    anyhow::ensure!(
        !read_only || record.flags & libc::MNT_RDONLY as u32 != 0,
        "macFUSE FSKit did not provide a read-only mount for policy none on this macOS version"
    );
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
    Ok(mac_mounts()?
        .into_iter()
        .map(|record| (record.path, record.identity.source))
        .collect())
}

#[cfg(target_os = "macos")]
struct MacMount {
    path: std::path::PathBuf,
    kind: String,
    identity: MountIdentity,
    flags: u32,
}

#[cfg(target_os = "macos")]
fn mac_mounts() -> Result<Vec<MacMount>> {
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
                let source = unsafe { CStr::from_ptr(entry.f_mntfromname.as_ptr()) };
                let kind = unsafe { CStr::from_ptr(entry.f_fstypename.as_ptr()) };
                MacMount {
                    path: Path::new(OsStr::from_bytes(path.to_bytes())).to_path_buf(),
                    kind: kind.to_string_lossy().into_owned(),
                    identity: MountIdentity {
                        filesystem_id: unsafe {
                            std::mem::transmute::<libc::fsid_t, [i32; 2]>(entry.f_fsid)
                        },
                        owner: entry.f_owner,
                        source: source.to_string_lossy().into_owned(),
                    },
                    flags: entry.f_flags,
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
