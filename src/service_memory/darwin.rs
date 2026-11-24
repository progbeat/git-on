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
    image: String,
    attachment_pid: Option<u64>,
    block: Option<BlockIdentity>,
    mount: PathBuf,
    attached: bool,
    directly_mounted: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct RamAttachment {
    pid: Option<u64>,
}

type BlockIdentity = (u64, u64, u64, u32);

fn block_identity(metadata: &Metadata) -> BlockIdentity {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.rdev(),
        metadata.uid(),
    )
}

impl Device {
    fn attach(image: &str, mount: PathBuf) -> Result<Self> {
        Self::attach_with_metadata(image, mount, |path| fs::symlink_metadata(path))
    }

    fn attach_with_metadata(
        image: &str,
        mount: PathBuf,
        read_metadata: impl FnOnce(&Path) -> io::Result<Metadata>,
    ) -> Result<Self> {
        // diskutil uses DiskImages2's sparse RAM device. The hdiutil attach
        // path can allocate much of the requested capacity immediately.
        let output = checked(Command::new(DISKUTIL).args(["image", "attach", "-noMount", image]))?;
        let path = std::str::from_utf8(&output)?.trim();
        let identifier = path.strip_prefix("/dev/disk").unwrap_or_default();
        anyhow::ensure!(
            !identifier.is_empty() && identifier.bytes().all(|byte| byte.is_ascii_digit()),
            "unexpected RAM device returned by diskutil"
        );
        let mut device = Self {
            path: PathBuf::from(path),
            image: image.to_owned(),
            attachment_pid: None,
            block: None,
            mount,
            attached: true,
            directly_mounted: false,
        };
        // Either lookup can fail after attach. Preserve the identity reported
        // by the other one for cleanup before propagating the failure.
        let block = (|| -> Result<_> {
            let metadata =
                read_metadata(&device.path).context("inspect attached RAM block device")?;
            anyhow::ensure!(
                metadata.file_type().is_block_device()
                    && metadata.uid() == unsafe { libc::geteuid() },
                "RAM block device is not owned by this user"
            );
            Ok(block_identity(&metadata))
        })();
        device.block = block.as_ref().ok().copied();
        let Some(attachment) = device.inspect_attachment()? else {
            // DiskImages answered and did not confirm this device.
            device.block = None;
            bail!("could not identify the newly attached RAM image");
        };
        device.attachment_pid = attachment.pid;
        block?;
        device.check()?;
        Ok(device)
    }

    fn inspect_attachment(&self) -> Result<Option<RamAttachment>> {
        #[cfg(test)]
        if IDENTITY_FAULT.get() == Some(IdentityFault::InventoryRead) {
            return Err(io::Error::from_raw_os_error(libc::EIO)).context("inspect DiskImages");
        }
        ram_attachment(&self.image, &self.path)
    }

    fn check(&self) -> Result<()> {
        #[cfg(test)]
        if IDENTITY_FAULT.get() == Some(IdentityFault::BlockCheck) {
            return Err(io::Error::from_raw_os_error(libc::EIO).into());
        }
        let expected = self.block.context("inspect RAM block device first")?;
        let current = fs::symlink_metadata(&self.path)?;
        anyhow::ensure!(
            current.file_type().is_block_device() && block_identity(&current) == expected,
            "attached RAM device changed"
        );
        Ok(())
    }

    fn owns_attachment(&self) -> Result<bool> {
        if self.block.is_some() {
            match self.check() {
                Ok(()) => return Ok(true),
                Err(error) if error.is::<io::Error>() && self.attachment_pid.is_some() => {}
                Err(error) => return Err(error),
            }
        }
        let Some(expected) = self.attachment_pid else {
            return Ok(false);
        };
        let Some(attachment) = self.inspect_attachment()? else {
            return Ok(false);
        };
        anyhow::ensure!(
            attachment.pid == Some(expected),
            "RAM image attachment changed before cleanup"
        );
        Ok(true)
    }

    fn try_detach(&mut self) -> Result<bool> {
        if self.attached && self.owns_attachment()? {
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
                if !self.owns_attachment()? {
                    self.attached = false;
                    return Ok(true);
                }
            }
            if !detach_device(&self.path)? {
                return Ok(false);
            }
        }
        self.attached = false;
        Ok(true)
    }
}

fn ram_attachment(image: &str, path: &Path) -> Result<Option<RamAttachment>> {
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
    let images = info["images"]
        .as_array()
        .context("DiskImages did not list images")?;
    let uid = u64::from(unsafe { libc::geteuid() });
    let attachment = images.iter().find(|row| {
        row["owner-uid"].as_u64() == Some(uid)
            && row["image-path"] == image
            && row["system-entities"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|entry| entry["dev-entry"].as_str().map(Path::new) == Some(path))
    });
    Ok(attachment.map(|row| {
        let pid = row["hdid-pid"].as_u64();
        #[cfg(test)]
        let pid = pid.filter(|_| IDENTITY_FAULT.get() != Some(IdentityFault::MissingPid));
        RamAttachment { pid }
    }))
}

fn detach_device(path: &Path) -> Result<bool> {
    let output = Command::new(HDIUTIL)
        .arg("detach")
        .arg(path)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()?;
    if output.status.code() == Some(libc::EBUSY) {
        return Ok(false);
    }
    success(output, HDIUTIL)?;
    Ok(true)
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

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdentityFault {
    BlockMetadata,
    BlockCheck,
    InventoryRead,
    MissingPid,
}

#[cfg(test)]
thread_local! {
    static IDENTITY_FAULT: std::cell::Cell<Option<IdentityFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_is_detached_when_either_identity_lookup_fails() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let image = "ram://8219";
        for failure in [
            IdentityFault::BlockMetadata,
            IdentityFault::InventoryRead,
            IdentityFault::MissingPid,
        ] {
            let mut observed = None;
            let result = Device::attach_with_metadata(image, scratch.path().join("v"), |path| {
                let attached = ram_attachment(image, path).unwrap().unwrap();
                let pid_available = attached.pid.is_some();
                observed = Some((path.to_owned(), attached));
                match failure {
                    IdentityFault::BlockMetadata if pid_available => {
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    }
                    IdentityFault::BlockMetadata => fs::symlink_metadata(path),
                    _ => {
                        IDENTITY_FAULT.set(Some(failure));
                        fs::symlink_metadata(path)
                    }
                }
            });
            IDENTITY_FAULT.set(None);
            let outcome = result.and_then(|mut device| {
                if failure == IdentityFault::MissingPid {
                    IDENTITY_FAULT.set(Some(IdentityFault::BlockCheck));
                    let fault = device.try_detach();
                    IDENTITY_FAULT.set(None);
                    anyhow::ensure!(
                        fault.err().and_then(|error| error
                            .downcast_ref::<io::Error>()
                            .and_then(io::Error::raw_os_error))
                            == Some(libc::EIO),
                        "a transient block check must report its error and remain retryable"
                    );
                }
                anyhow::ensure!(device.try_detach()?, "fixture RAM image is busy");
                Ok(())
            });
            let (path, attached) = observed.context("fixture did not attach a RAM image")?;
            let metadata_failure_exercised =
                failure == IdentityFault::BlockMetadata && attached.pid.is_some();
            let remaining = ram_attachment(image, &path)?;
            let expected = Some(attached);
            if remaining == expected {
                // Clean up the fixture even when regression detection fails.
                detach_device(&path)?;
            }
            assert_ne!(remaining, expected, "failed setup leaked its RAM image");
            if metadata_failure_exercised || failure == IdentityFault::InventoryRead {
                let error = outcome.err().context("accepted unavailable RAM identity")?;
                assert_eq!(
                    error
                        .downcast_ref::<io::Error>()
                        .and_then(io::Error::raw_os_error),
                    Some(libc::EIO)
                );
            } else {
                outcome.context("the matching block device supplies its identity")?;
            }
        }
        Ok(())
    }
}
