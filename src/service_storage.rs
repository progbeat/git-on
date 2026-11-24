//! File-shaped service data for tools which require real filesystem paths.
use anyhow::{bail, Context, Result};
use std::{
    ffi::{CStr, OsStr},
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

mod directory;
#[doc(hidden)]
pub use directory::OwnedDirectory;

const HELPER_ARGV0: &str = "--internal-service-storage";
// Older runners use this link to recognize when checked recovery has finished.
const RUN_LINK: &str = ".git-on-memory";
const BINDING: &str = "run-owner";
#[doc(hidden)]
pub const WORKSPACE_BINDING: &CStr = c".git-on-service-owner";
const RETAIN: &str = "recovery-requested";
static HELPER_AVAILABLE: AtomicBool = AtomicBool::new(false);
static WORKSPACE: Mutex<Weak<Workspace>> = Mutex::new(Weak::new());

pub(crate) struct Directory {
    directory: OwnedDirectory,
    _workspace: Arc<Workspace>,
}

impl Directory {
    pub(crate) fn new(prefix: &str) -> Result<Self> {
        Self::create(prefix, None)
    }

    pub(crate) fn new_for_run(prefix: &str, run: &Path) -> Result<Self> {
        Self::create(prefix, Some(run))
    }

    fn create(prefix: &str, run: Option<&Path>) -> Result<Self> {
        let workspace = Workspace::get()?;
        if let (Some(run), Owner::Process { .. }) = (run, &workspace._owner) {
            bind_run(run, &workspace.directory)?;
        }
        let directory = workspace.directory.child(prefix)?;
        Ok(Self {
            directory,
            _workspace: workspace,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) fn link(&self, run: &Path, name: &OsStr, entry: &Path) -> io::Result<()> {
        symlink(self.path().join(entry), run.join(name))
    }

    pub(crate) fn keep_for_owner(&mut self) {
        self.directory.remove_on_drop = false;
    }

    pub(crate) fn can_fall_back_to(&self, run: &Path) -> bool {
        std::env::var_os("GIT_ON_SERVICE_TMPDIR").is_none()
            && tmpfs_stats(self.path()).is_ok()
            && fs::metadata(self.path())
                .ok()
                .zip(fs::metadata(run).ok())
                .is_some_and(|(service, run)| service.dev() != run.dev())
    }
}

struct Workspace {
    directory: OwnedDirectory,
    _owner: Owner,
}

impl Workspace {
    fn get() -> Result<Arc<Self>> {
        let mut cached = WORKSPACE.lock().expect("service workspace mutex poisoned");
        if let Some(workspace) = cached.upgrade() {
            return Ok(workspace);
        }
        let (directory, owner) = if HELPER_AVAILABLE.load(Ordering::Relaxed) {
            let (path, owner) = Lease::start().context("start invocation-owned service storage")?;
            (OwnedDirectory::open(&path)?, owner)
        } else {
            (service_workspace()?, Owner::Inline)
        };
        let workspace = Arc::new(Self {
            directory,
            _owner: owner,
        });
        *cached = Arc::downgrade(&workspace);
        Ok(workspace)
    }
}

enum Owner {
    Inline,
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
        // available to clean up the invocation's service directory.
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
            .context("service storage helper did not return a workspace")?;
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
            // Filesystem setup can still be in progress on a slow temporary
            // filesystem. The owner will remove it after the lease closes.
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
                        "git-on: service storage: {}",
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
            "service storage helper closed its setup pipe"
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
    let mut storage = service_workspace()?;
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
    let root = storage.path().to_owned();
    let retained = DirectoryBinding::read(&root.join(BINDING))
        .ok()
        .flatten()
        .filter(|run| {
            run.owns_workspace(&root)
                && (root.join(RETAIN).exists() || crate::remote::has_ssh_connection(&root))
        });
    if let Some(run) = &retained {
        release_runner()?;
        wait_while(|| Ok(run.owns_workspace(&root)))?;
    }
    if !storage.remove()? {
        if retained.is_none() {
            release_runner()?;
        }
        // An NFS temporary directory can retain an open, unlinked diagnostic
        // file until its last writer closes the descriptor.
        wait_while(|| storage.remove().map(|removed| !removed).map_err(Into::into))?;
    }
    Ok(())
}

#[derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DirectoryBinding {
    path: Vec<u8>,
    device: u64,
    inode: u64,
}

impl DirectoryBinding {
    fn new(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self::for_directory(path, &metadata))
    }

    fn for_directory(path: &Path, metadata: &fs::Metadata) -> Self {
        Self {
            path: path.as_os_str().as_bytes().to_vec(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn path(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.path))
    }

    fn read(path: &Path) -> io::Result<Option<Self>> {
        let file = match File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Self::from_file(file).map(Some)
    }

    fn from_file(file: File) -> io::Result<Self> {
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("invalid service storage record type"));
        }
        Ok(serde_json::from_reader(file.take(32 * 1024))?)
    }

    fn write(&self, path: &Path) -> io::Result<()> {
        if Self::read(path)?.as_ref() == Some(self) {
            return Ok(());
        }
        match publish_record(path, |file| Ok(serde_json::to_writer(file, self)?)) {
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && Self::read(path)?.as_ref() == Some(self) =>
            {
                Ok(())
            }
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        metadata.is_dir() && (metadata.dev(), metadata.ino()) == (self.device, self.inode)
    }

    fn owns_workspace(&self, root: &Path) -> bool {
        fs::symlink_metadata(self.path()).is_ok_and(|metadata| self.matches(&metadata))
            && fs::read_link(self.path().join(RUN_LINK)).as_deref().ok() == Some(root)
    }
}

fn publish_record(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let mut record = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    write(record.as_file_mut())?;
    record.persist_noclobber(path)?;
    Ok(())
}

fn bind_run(run: &Path, workspace: &OwnedDirectory) -> io::Result<()> {
    let run = run.canonicalize()?;
    let root = workspace.path();
    let name = OsStr::from_bytes(WORKSPACE_BINDING.to_bytes());
    DirectoryBinding::for_directory(root, &workspace.directory.metadata()?)
        .write(&run.join(name))?;
    DirectoryBinding::new(&run)?.write(&root.join(BINDING))?;
    let link = run.join(RUN_LINK);
    match symlink(root, &link) {
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists && fs::read_link(&link)? == root =>
        {
            Ok(())
        }
        result => result,
    }
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

/// Keep the service files available after a busy mount or shutdown error.
#[doc(hidden)]
pub fn retain_for_cleanup(run: &Path) -> Result<()> {
    let workspace = WORKSPACE
        .lock()
        .expect("service workspace mutex poisoned")
        .upgrade();
    if let Some(workspace) = workspace {
        let root = workspace.directory.path();
        if fs::read_link(run.join(RUN_LINK)).as_deref().ok() == Some(root) {
            File::create(root.join(RETAIN))?;
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn open_stale_workspace(record: File) -> Result<Option<OwnedDirectory>> {
    let binding = DirectoryBinding::from_file(record).context("read service storage record")?;
    let root = binding.path();
    anyhow::ensure!(
        root.is_absolute()
            && root
                .file_name()
                .is_some_and(|name| { name.as_bytes().starts_with(WORKSPACE_PREFIX.as_bytes()) }),
        "invalid service storage location",
    );
    let workspace = match OwnedDirectory::open(root) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open service storage for checked cleanup"),
    };
    let metadata = workspace.directory.metadata()?;
    anyhow::ensure!(
        binding.matches(&metadata)
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o777 == 0o700,
        "service storage changed at {}",
        root.display(),
    );
    Ok(Some(workspace))
}

#[doc(hidden)]
pub fn legacy_workspace(run: &Path) -> Option<PathBuf> {
    let root = fs::read_link(run.join(RUN_LINK)).ok()?;
    let run = run.canonicalize().ok()?;
    let binding = DirectoryBinding::read(&root.join(BINDING)).ok()??;
    if binding.path() != run || !binding.owns_workspace(&root) {
        return None;
    }
    Some(root)
}

/// A stale workspace owner begins teardown as soon as its run disappears.
#[doc(hidden)]
pub fn wait_for_stale_workspace(root: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while root.try_exists()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "service storage remains at {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

const WORKSPACE_PREFIX: &str = ".git-on-service-";

fn service_workspace() -> Result<OwnedDirectory> {
    if let Some(root) = std::env::var_os("GIT_ON_SERVICE_TMPDIR") {
        return on_tmpfs(Path::new(&root))
            .context("GIT_ON_SERVICE_TMPDIR must name a writable tmpfs directory");
    }
    let fallback = crate::platform::temporary_base();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    workspace_in(
        [
            Some(Path::new("/dev/shm")),
            runtime
                .as_deref()
                .map(Path::new)
                .filter(|path| path.is_absolute()),
            Some(&fallback),
        ]
        .into_iter()
        .flatten(),
        &fallback,
    )
}

fn workspace_in<'a>(
    candidates: impl Iterator<Item = &'a Path>,
    fallback: &Path,
) -> Result<OwnedDirectory> {
    let mut candidates = candidates
        .filter_map(|path| {
            let root = path.canonicalize().ok()?;
            let stats = tmpfs_stats(&root).ok()?;
            #[cfg(target_os = "linux")]
            let block_size = if stats.f_frsize > 0 {
                stats.f_frsize
            } else {
                stats.f_bsize
            };
            #[cfg(target_os = "macos")]
            let block_size = stats.f_bsize;
            let free = u128::from(stats.f_bavail) * block_size as u128;
            Some((free, root))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(available, _)| std::cmp::Reverse(*available));
    candidates.dedup();
    if let Some(directory) = candidates
        .into_iter()
        .find_map(|(_, root)| on_tmpfs(&root).ok())
    {
        return Ok(directory);
    }
    let fallback = fallback.canonicalize()?;
    OwnedDirectory::create(&fallback, WORKSPACE_PREFIX)
        .with_context(|| format!("create service directory under {}", fallback.display()))
}

fn on_tmpfs(root: &Path) -> Result<OwnedDirectory> {
    let root = root.canonicalize()?;
    tmpfs_stats(&root)?;
    let dir = OwnedDirectory::create(&root, WORKSPACE_PREFIX)?;
    tmpfs_stats(dir.path())?;
    Ok(dir)
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
        let file = tempfile::tempfile_in(workspace.directory.path())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn workspace_uses_the_system_temp_directory_without_a_tmpfs_candidate() -> Result<()> {
        let fallback = tempfile::tempdir()?;
        let workspace = workspace_in(std::iter::empty(), fallback.path())?;
        let path = workspace.path().to_path_buf();
        assert_eq!(
            path.parent(),
            Some(fallback.path().canonicalize()?.as_path())
        );
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o700);
        drop(workspace);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn stale_cleanup_recognizes_a_previous_runner_binding() -> Result<()> {
        let run = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let path = run.path().canonicalize()?;
        DirectoryBinding::new(&path)?.write(&workspace.path().join(BINDING))?;
        symlink(workspace.path(), path.join(".git-on-memory"))?;

        assert_eq!(legacy_workspace(&path).as_deref(), Some(workspace.path()));
        Ok(())
    }

    #[test]
    fn an_interrupted_write_does_not_publish_a_partial_recovery_record() -> Result<()> {
        let run = tempfile::tempdir()?;
        let record = run
            .path()
            .join(OsStr::from_bytes(WORKSPACE_BINDING.to_bytes()));
        assert!(publish_record(&record, |file| {
            file.write_all(b"{\"path\":")?;
            Err(io::Error::other("fixture interrupted write"))
        })
        .is_err());
        assert!(!record.exists());

        let binding = DirectoryBinding::new(run.path())?;
        binding.write(&record)?;
        assert!(DirectoryBinding::read(&record)?.as_ref() == Some(&binding));
        Ok(())
    }

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
