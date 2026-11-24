//! File-shaped service data for tools which require real filesystem paths.
use anyhow::{bail, Context, Result};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

const HELPER_ARGV0: &str = "--internal-service-memory";
const RUN_LINK: &str = ".git-on-memory";
const BINDING: &str = "run-owner";
const RETAIN: &str = "recovery-requested";
static HELPER_AVAILABLE: AtomicBool = AtomicBool::new(false);
static WORKSPACE: Mutex<Weak<Workspace>> = Mutex::new(Weak::new());

pub(crate) struct Directory {
    directory: tempfile::TempDir,
    _workspace: Arc<Workspace>,
}

impl Directory {
    pub(crate) fn new(prefix: &str) -> Result<Self> {
        let workspace = Workspace::get()?;
        let directory = private_directory(prefix, &workspace.path)?;
        Ok(Self {
            directory,
            _workspace: workspace,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) fn link(&self, run: &Path, name: &OsStr, entry: &Path) -> io::Result<()> {
        if matches!(self._workspace._owner, Owner::Process { .. }) {
            bind_run(run, &self._workspace.path)?;
        }
        symlink(self.path().join(entry), run.join(name))
    }

    pub(crate) fn keep_for_owner(&mut self) {
        self.directory.disable_cleanup(true);
    }
}

struct Workspace {
    path: PathBuf,
    _owner: Owner,
}

impl Workspace {
    fn get() -> Result<Arc<Self>> {
        let mut cached = WORKSPACE.lock().expect("memory workspace mutex poisoned");
        if let Some(workspace) = cached.upgrade() {
            return Ok(workspace);
        }
        let (path, owner) = if HELPER_AVAILABLE.load(Ordering::Relaxed) {
            Lease::start().context("start invocation-owned memory storage")?
        } else {
            let storage = Storage::new(|| false)?;
            (
                storage.path().to_path_buf(),
                Owner::Inline { _storage: storage },
            )
        };
        let workspace = Arc::new(Self {
            path,
            _owner: owner,
        });
        *cached = Arc::downgrade(&workspace);
        Ok(workspace)
    }
}

enum Owner {
    Inline { _storage: Storage },
    Process { _lease: Lease },
}

struct Lease {
    child: Child,
    reply: Option<BufReader<ChildStdout>>,
    abandoned_setup: bool,
}

impl Lease {
    fn start() -> Result<(PathBuf, Owner)> {
        crate::object::check_transfer()?;
        // The lease pipe closes even if the caller is killed. Give its owner
        // a separate process group so ordinary job cancellation leaves it
        // available to clean up the invocation's RAM storage.
        let child = Command::new(std::env::current_exe()?)
            .arg0(HELPER_ARGV0)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut lease = Self {
            child,
            reply: None,
            abandoned_setup: false,
        };
        let mut reply = BufReader::new(lease.child.stdout.take().expect("piped workspace path"));
        let bytes = match setup_reply(&mut reply, crate::object::check_transfer) {
            Ok(bytes) => bytes,
            Err(error) => {
                lease.abandoned_setup = crate::object::is_cancelled(&error);
                return Err(error);
            }
        };
        if let Some(error) = bytes.strip_prefix(b"E") {
            bail!("{}", String::from_utf8_lossy(error));
        }
        let path = bytes
            .strip_prefix(b"P")
            .filter(|path| path.starts_with(b"/"))
            .context("memory storage helper did not return a workspace")?;
        lease.reply = Some(reply);
        Ok((
            PathBuf::from(OsStr::from_bytes(path)),
            Owner::Process { _lease: lease },
        ))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        if self.abandoned_setup {
            // An attach already in progress must finish reporting its device
            // before its owner can tear it down. It has no inherited STDIO.
            return;
        }
        let mut byte = [0];
        if let Some(mut reply) = self.reply.take() {
            if reply.read_exact(&mut byte).is_ok() {
                if byte == *b"R" {
                    // The owner is still needed for checked recovery or a
                    // detached helper's open file.
                    return;
                }
                if byte == *b"E" {
                    let mut message = Vec::new();
                    let _ = reply.read_until(0, &mut message);
                    let message = message.strip_suffix(&[0]).unwrap_or(&message);
                    let _ = writeln!(
                        io::stderr().lock(),
                        "git-on: memory storage: {}",
                        String::from_utf8_lossy(message)
                    );
                }
            }
        }
        let _ = self.child.wait();
    }
}

fn setup_reply<R: Read + AsRawFd>(
    reply: &mut BufReader<R>,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut descriptor = libc::pollfd {
        fd: reply.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        check()?;
        if reply.buffer().is_empty() {
            match unsafe { libc::poll(&mut descriptor, 1, 10) } {
                0 => continue,
                -1 => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                _ => {}
            }
        }
        let input = reply.fill_buf()?;
        if let Some(end) = input.iter().position(|&byte| byte == 0) {
            bytes.extend_from_slice(&input[..end]);
            reply.consume(end + 1);
            return Ok(bytes);
        }
        anyhow::ensure!(
            !input.is_empty(),
            "memory storage helper closed its setup pipe"
        );
        let length = input.len();
        bytes.extend_from_slice(input);
        reply.consume(length);
    }
}

/// Register the internal entry point in executables that use this library.
#[doc(hidden)]
pub fn invoke() -> Option<i32> {
    HELPER_AVAILABLE.store(true, Ordering::Relaxed);
    if std::env::args_os().next().as_deref() != Some(OsStr::new(HELPER_ARGV0)) {
        return None;
    }
    Some(match serve() {
        Ok(()) => 0,
        Err(error) => {
            let _ = send_reply(b'E', format!("{error:#}").as_bytes());
            125
        }
    })
}

fn send_reply(kind: u8, value: &[u8]) -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(&[kind])?;
    output.write_all(value)?;
    output.write_all(&[0])?;
    output.flush()
}

fn caller_left() -> bool {
    let mut pipe = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pipe, 1, 0) };
    ready > 0 && pipe.revents & libc::POLLHUP != 0
}

fn serve() -> Result<()> {
    if caller_left() {
        return Ok(());
    }
    let mut storage = Storage::new(caller_left)?;
    send_reply(b'P', storage.path().as_os_str().as_bytes())?;
    // Do not consume arbitrary data from the parent. A read end without a
    // writer is the only message required by the lease.
    let mut lease = io::stdin().lock();
    let mut byte = [0];
    loop {
        if lease.read(&mut byte)? == 0 {
            break;
        }
    }
    crate::git::stop_memory_verifiers(storage.path());
    let root = storage.path();
    let retained = RunBinding::load(root).filter(|run| {
        run.matches(root)
            && (root.join(RETAIN).exists() || crate::remote::has_memory_connection(root))
    });
    if let Some(run) = &retained {
        release_runner()?;
        wait_while(|| Ok(run.matches(storage.path())))?;
    }
    if !storage.try_close()? {
        if retained.is_none() {
            release_runner()?;
        }
        // Detached programs can still own an unlinked diagnostic descriptor.
        // Retry the ordinary unmount once the OS has released those readers.
        wait_while(|| Ok(!storage.try_close()?))?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RunBinding {
    path: Vec<u8>,
    device: u64,
    inode: u64,
}

impl RunBinding {
    fn load(root: &Path) -> Option<Self> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join(BINDING))
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        serde_json::from_reader(file.take(32 * 1024)).ok()
    }

    fn matches(&self, root: &Path) -> bool {
        let path = Path::new(OsStr::from_bytes(&self.path));
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_dir() && (metadata.dev(), metadata.ino()) == (self.device, self.inode)
        }) && fs::read_link(path.join(RUN_LINK)).as_deref().ok() == Some(root)
    }
}

fn bind_run(run: &Path, root: &Path) -> io::Result<()> {
    let path = run.canonicalize()?;
    let metadata = fs::metadata(&path)?;
    let binding = RunBinding {
        path: path.as_os_str().as_bytes().to_vec(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    let record = root.join(BINDING);
    match File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(record)
    {
        Ok(file) => serde_json::to_writer(file, &binding)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if RunBinding::load(root)
                .is_some_and(|previous| previous.path == binding.path && previous.matches(root))
            {
                return Ok(());
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    }
    symlink(root, path.join(RUN_LINK))
}

fn wait_while(mut busy: impl FnMut() -> Result<bool>) -> Result<()> {
    let mut delay = Duration::from_millis(20);
    while busy()? {
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(1));
    }
    Ok(())
}

fn release_runner() -> io::Result<()> {
    {
        let mut reply = io::stdout().lock();
        let _ = reply.write_all(b"R").and_then(|()| reply.flush());
    }
    let null = File::options().read(true).write(true).open("/dev/null")?;
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Keep the memory-backed state available after a busy mount or shutdown error.
#[doc(hidden)]
pub fn retain_for_cleanup(run: &Path) -> Result<()> {
    let workspace = WORKSPACE
        .lock()
        .expect("memory workspace mutex poisoned")
        .upgrade();
    if let Some(workspace) = workspace {
        let root = &workspace.path;
        if fs::read_link(run.join(RUN_LINK)).as_deref().ok() == Some(root) {
            File::create(root.join(RETAIN))?;
        }
    }
    Ok(())
}

/// Record the memory lease before a validated stale run is removed.
#[doc(hidden)]
pub fn stale_memory_workspace(run: &Path) -> Option<PathBuf> {
    let root = fs::read_link(run.join(RUN_LINK)).ok()?;
    let run = run.canonicalize().ok()?;
    let binding = RunBinding::load(&root)?;
    if binding.path != run.as_os_str().as_bytes() || !binding.matches(&root) {
        return None;
    }
    Some(root)
}

/// A stale workspace owner begins teardown as soon as its run disappears.
#[doc(hidden)]
pub fn wait_for_stale_memory(root: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while root.try_exists()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "memory storage remains at {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

enum Storage {
    Tmpfs(tempfile::TempDir),
    #[cfg(target_os = "macos")]
    RamVolume(darwin::Volume),
}

impl Storage {
    fn new(_caller_left: impl Fn() -> bool) -> Result<Self> {
        if let Some(root) = std::env::var_os("GIT_ON_SERVICE_TMPDIR") {
            return Self::on_tmpfs(Path::new(&root))
                .context("GIT_ON_SERVICE_TMPDIR must name a writable memory filesystem");
        }
        #[cfg(target_os = "linux")]
        {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR");
            Self::linux_tmpfs(runtime.as_deref().map(Path::new))
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self::RamVolume(darwin::Volume::new(_caller_left)?))
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_tmpfs(runtime: Option<&Path>) -> Result<Self> {
        let mut candidates = std::iter::once(Path::new("/dev/shm"))
            .chain(runtime.filter(|path| path.is_absolute()))
            .filter_map(|path| {
                let root = path.canonicalize().ok()?;
                let stats = tmpfs_stats(&root).ok()?;
                let block_size = if stats.f_frsize > 0 {
                    stats.f_frsize
                } else {
                    stats.f_bsize
                };
                let free = u128::from(stats.f_bavail) * u128::try_from(block_size).ok()?;
                Some((free, root))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(available, _)| std::cmp::Reverse(*available));
        candidates.dedup_by(|a, b| a.1 == b.1);
        candidates
            .into_iter()
            .find_map(|(_, root)| Self::on_tmpfs(&root).ok())
            .context("no writable service tmpfs; set GIT_ON_SERVICE_TMPDIR to a tmpfs directory")
    }

    fn on_tmpfs(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        tmpfs_stats(&root)?;
        let dir = private_directory("git-on-", &root)?;
        tmpfs_stats(dir.path())?;
        Ok(Self::Tmpfs(dir))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Tmpfs(dir) => dir.path(),
            #[cfg(target_os = "macos")]
            Self::RamVolume(volume) => volume.mountpoint(),
        }
    }

    fn try_close(&mut self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        if let Self::RamVolume(volume) = self {
            return volume.try_detach();
        }
        Ok(true)
    }
}

fn private_directory(prefix: &str, root: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(root)
}

fn tmpfs_stats(path: &Path) -> Result<libc::statfs> {
    let fd = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(fd.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    #[cfg(target_os = "linux")]
    let in_memory = stats.f_type == libc::TMPFS_MAGIC;
    #[cfg(target_os = "macos")]
    let in_memory = unsafe { std::ffi::CStr::from_ptr(stats.f_fstypename.as_ptr()) } == c"tmpfs";
    anyhow::ensure!(in_memory, "{} is not on tmpfs", path.display());
    Ok(stats)
}

pub(crate) struct CaptureFile {
    pub(crate) file: File,
    _workspace: Option<Arc<Workspace>>,
}

impl CaptureFile {
    pub(crate) fn new() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::memfd_create(c"git-on-output".as_ptr(), libc::MFD_CLOEXEC) };
            if fd >= 0 {
                return Ok(Self {
                    file: unsafe { File::from_raw_fd(fd) },
                    _workspace: None,
                });
            }
        }
        let workspace = Workspace::get()?;
        let file = tempfile::tempfile_in(&workspace.path)?;
        Ok(Self {
            file,
            _workspace: Some(workspace),
        })
    }

    #[cfg(test)]
    pub(crate) fn fixture(file: File) -> Self {
        Self {
            file,
            _workspace: None,
        }
    }
}

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn cancellation_ends_a_workspace_handshake_with_no_response() -> Result<()> {
        let (reader, _writer) = UnixStream::pair()?;
        reader.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut readiness_checks = 0;
        let error = setup_reply(&mut BufReader::new(reader), || {
            readiness_checks += 1;
            anyhow::ensure!(readiness_checks < 3, "fixture cancellation");
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "fixture cancellation");
        Ok(())
    }
}
