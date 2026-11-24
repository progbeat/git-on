use anyhow::{bail, Context, Result};
use std::{
    ffi::{CStr, CString, OsStr, OsString},
    fs::{self, Metadata},
    io::{self, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const HDIUTIL: &str = "/usr/bin/hdiutil";
const DISKUTIL: &str = "/usr/sbin/diskutil";

pub(super) struct Volume {
    device: Device,
    _parent: tempfile::TempDir,
}

impl Volume {
    pub(super) fn new(caller_left: impl Fn() -> bool) -> Result<Self> {
        let parent = super::private_directory("git-on-ram-", Path::new("/private/tmp"))?;
        let mount = parent.path().join("v");
        fs::create_dir(&mount)?;
        let ram = format!("ram://{}", volume_sectors()?);
        let mut device = Device::attach(&ram, mount)?;
        anyhow::ensure!(!caller_left(), "caller ended during RAM volume setup");
        checked(
            Command::new("/sbin/newfs_hfs")
                .args(["-s", "-v", "GitOnRAM"])
                .arg(&device.path),
        )?;
        anyhow::ensure!(!caller_left(), "caller ended during RAM volume setup");
        device.check()?;
        // This device was just formatted and has a private mountpoint.
        // Mount it directly where macOS permits user-owned HFS mounts.
        let direct = checked(
            Command::new("/sbin/mount_hfs")
                .args(["-o", "nobrowse,nosuid,nodev"])
                .arg(&device.path)
                .arg(&device.mount),
        );
        device.directly_mounted = match direct {
            Err(error) if check_mount(&device.mount, &device.path).is_err() => {
                anyhow::ensure!(!caller_left(), "caller ended during RAM volume setup");
                device.check()?;
                checked(
                    Command::new(DISKUTIL)
                        .args(["mount", "-mountPoint"])
                        .arg(&device.mount)
                        .arg(&device.path),
                )
                .with_context(|| format!("mount invocation-owned RAM volume after {error:#}"))?;
                false
            }
            _ => true,
        };
        check_mount(&device.mount, &device.path)?;
        Ok(Self {
            device,
            _parent: parent,
        })
    }

    pub(super) fn mountpoint(&self) -> &Path {
        &self.device.mount
    }

    pub(super) fn try_detach(&mut self) -> Result<bool> {
        self.device.try_detach()
    }
}

struct Device {
    path: PathBuf,
    metadata: Metadata,
    mount: PathBuf,
    attached: bool,
    directly_mounted: bool,
}

impl Device {
    fn attach(image: &str, mount: PathBuf) -> Result<Self> {
        // diskutil uses DiskImages2's sparse RAM device. The hdiutil attach
        // path can allocate much of the requested capacity immediately.
        let output = checked(Command::new(DISKUTIL).args(["image", "attach", "-noMount", image]))?;
        let path = std::str::from_utf8(&output)?.trim();
        let identifier = path.strip_prefix("/dev/disk").unwrap_or_default();
        anyhow::ensure!(
            !identifier.is_empty() && identifier.bytes().all(|byte| byte.is_ascii_digit()),
            "unexpected RAM device returned by diskutil"
        );
        let metadata = fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.file_type().is_block_device() && metadata.uid() == unsafe { libc::geteuid() },
            "RAM block device is not owned by this user"
        );
        let device = Self {
            path: PathBuf::from(path),
            metadata,
            mount,
            attached: true,
            directly_mounted: false,
        };
        // Confirm that the exact device to be formatted is a DiskImages RAM
        // allocation. No caller-supplied device is ever formatted or detached.
        let plist = checked(Command::new(HDIUTIL).args(["info", "-plist"]))?;
        let mut converter = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        converter
            .stdin
            .take()
            .expect("piped plist input")
            .write_all(&plist)?;
        let info = serde_json::from_slice::<serde_json::Value>(&success(
            converter.wait_with_output()?,
            "plutil",
        )?)?;
        let verified = info["images"].as_array().into_iter().flatten().any(|row| {
            row["image-path"] == image
                && row["system-entities"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|entry| entry["dev-entry"] == path)
        });
        anyhow::ensure!(verified, "could not identify the newly attached RAM device");
        device.check()?;
        Ok(device)
    }

    fn check(&self) -> Result<()> {
        let current = fs::symlink_metadata(&self.path)?;
        anyhow::ensure!(
            current.file_type().is_block_device()
                && (current.dev(), current.ino(), current.rdev(), current.uid())
                    == (
                        self.metadata.dev(),
                        self.metadata.ino(),
                        self.metadata.rdev(),
                        self.metadata.uid()
                    ),
            "attached RAM device changed"
        );
        Ok(())
    }

    fn try_detach(&mut self) -> Result<bool> {
        if self.attached {
            self.check()?;
            // Disk Arbitration did not mount this volume. Explicitly unmount
            // it before asking DiskImages to detach the underlying device.
            if self.directly_mounted {
                if let Some(source) = mounted_device(&self.mount)? {
                    anyhow::ensure!(
                        source == self.path.as_os_str(),
                        "RAM volume mountpoint changed"
                    );
                    let path = CString::new(self.mount.as_os_str().as_bytes())?;
                    if unsafe { libc::unmount(path.as_ptr(), 0) } != 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::EBUSY) {
                            return Ok(false);
                        }
                        return Err(error).context("unmount invocation-owned RAM volume");
                    }
                }
                self.directly_mounted = false;
                self.check()?;
            }
            let output = Command::new(HDIUTIL)
                .arg("detach")
                .arg(&self.path)
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .output()?;
            if output.status.code() == Some(libc::EBUSY) {
                return Ok(false);
            }
            success(output, HDIUTIL)?;
            self.attached = false;
        }
        Ok(true)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Err(error) = self.try_detach().and_then(|detached| {
            anyhow::ensure!(detached, "RAM volume is busy");
            Ok(())
        }) {
            let _ = writeln!(
                io::stderr().lock(),
                "git-on: detach invocation-owned RAM volume: {error:#}"
            );
        }
    }
}

fn check_mount(path: &Path, device: &Path) -> Result<()> {
    anyhow::ensure!(
        mounted_device(path)?.as_deref() == Some(device.as_os_str()),
        "RAM volume was not mounted at its private mountpoint"
    );
    Ok(())
}

fn mounted_device(path: &Path) -> Result<Option<OsString>> {
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    let mount = CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::statfs(mount.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("inspect mounted RAM volume");
    }
    let stats = unsafe { stats.assume_init() };
    let mounted = unsafe { CStr::from_ptr(stats.f_mntonname.as_ptr()) };
    Ok((mounted == mount.as_c_str()).then(|| {
        let from = unsafe { CStr::from_ptr(stats.f_mntfromname.as_ptr()) };
        OsStr::from_bytes(from.to_bytes()).to_owned()
    }))
}

fn volume_sectors() -> Result<u64> {
    match std::env::var("GIT_ON_SERVICE_RAM_MIB") {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .and_then(|mib| mib.checked_mul(2048))
            .filter(|&sectors| sectors > 0)
            .context("GIT_ON_SERVICE_RAM_MIB must be a positive size in MiB"),
        Err(std::env::VarError::NotPresent) => Ok(physical_memory()? / 512),
        Err(error) => Err(error.into()),
    }
}

fn physical_memory() -> Result<u64> {
    let mut bytes = 0_u64;
    let mut length = std::mem::size_of_val(&bytes);
    if unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("read physical memory size");
    }
    anyhow::ensure!(
        length == std::mem::size_of_val(&bytes) && bytes >= 512,
        "invalid physical memory size"
    );
    Ok(bytes)
}

fn checked(command: &mut Command) -> Result<Vec<u8>> {
    command.stdin(Stdio::null()).env("LC_ALL", "C");
    success(command.output()?, &command.get_program().to_string_lossy())
}

fn success(output: std::process::Output, tool: &str) -> Result<Vec<u8>> {
    if !output.status.success() {
        bail!(
            "{tool} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}
