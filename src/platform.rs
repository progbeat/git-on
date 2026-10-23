//! Host-specific FUSE discovery and checked mount ownership. No configuration,
//! extension loading, privilege changes, or lazy unmounts are performed here.
use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
use std::path::Path;

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
        if !Path::new("/usr/local/lib/libfuse.2.dylib").is_file() {
            return Ok(Some(
                "macFUSE libfuse2 is absent (/usr/local/lib/libfuse.2.dylib)".into(),
            ));
        }
        if !Path::new("/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse").is_file()
        {
            return Ok(Some("the macFUSE mount helper is absent".into()));
        }
    }
    Ok(None)
}

pub fn mount_is_ours(dir: &Path, id: &str) -> Result<bool> {
    let point = dir.join("mount");
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
                let source = unsafe { CStr::from_ptr(entry.f_mntfromname.as_ptr()) };
                (
                    Path::new(OsStr::from_bytes(path.to_bytes())).to_path_buf(),
                    source.to_string_lossy().into_owned(),
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
