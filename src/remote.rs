//! Explicit one-object fetches. Git's promisor auto-fetch is never the loader.
use crate::git::Git;
use anyhow::{bail, Context, Result};
use git2::{Oid, Repository};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const DISK_CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const DISK_CACHE_ENTRIES: usize = 4096;
pub const SSH_CONTROL_OWNER_FILE: &str = "ssh-control-owner";

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketOwner {
    format: String,
    device: u64,
    inode: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client: Option<ControlClient>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlClient {
    path: Vec<u8>,
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

pub struct Remote {
    pub git: Git,
    pub dir: PathBuf,
    source: OsString,
    cache: DiskCache,
    socket: Option<PathBuf>,
    socket_owner: Option<SocketOwner>,
    ssh: Option<OsString>,
    control_client: Option<ControlClient>,
    failed: Option<String>,
}
impl Remote {
    pub fn open(cwd: &Path, source: &OsStr, run: &Path) -> Result<(Self, Vec<u8>)> {
        let mut git = Git::capture(cwd)?;
        let version = git.output(None, &[OsStr::new("--version")])?;
        let version = std::str::from_utf8(&version)?
            .split_whitespace()
            .nth(2)
            .context("unrecognized native Git version")?;
        let numbers: Vec<u32> = version
            .split('.')
            .take(3)
            .map(|x| x.parse().unwrap_or(0))
            .collect();
        if numbers.as_slice() < [2, 45, 1].as_slice() {
            bail!("remote execution requires native Git 2.45.1 or newer to disable automatic promisor fetches");
        }
        let url = if source.as_bytes().contains(&b':') {
            source.to_os_string()
        } else {
            let out = git
                .output(
                    None,
                    &[
                        OsStr::new("remote"),
                        OsStr::new("get-url"),
                        OsStr::new("--"),
                        source,
                    ],
                )
                .context("repository must be an HTTPS/SSH URL or configured remote name")?;
            OsString::from_vec(out.strip_suffix(b"\n").unwrap_or(&out).to_vec())
        };
        let out = if source.as_bytes().contains(&b':') {
            git.output(
                None,
                &[
                    OsStr::new("ls-remote"),
                    OsStr::new("--get-url"),
                    OsStr::new("--"),
                    &url,
                ],
            )?
        } else {
            url.as_bytes().to_vec()
        };
        let url = OsString::from_vec(out.strip_suffix(b"\n").unwrap_or(&out).to_vec());
        let transport_source = if source.as_bytes().contains(&b':') {
            url.clone()
        } else {
            git.pin_remote_url(source, &url);
            source.to_os_string()
        };
        let text = url.to_str().context("remote URL must be UTF-8")?;
        let ssh_transport =
            text.starts_with("ssh://") || (!text.contains("://") && text.contains(':'));
        if !ssh_transport && !text.starts_with("https://") && !text.starts_with("file://") {
            bail!("unsupported remote transport; use HTTPS, SSH, or a configured file:// fixture remote");
        }
        let dir = run.join("remote-repository");
        Repository::init_bare(&dir)?;
        let cache = DiskCache::standard()?;
        let socket = ssh_transport.then(|| run.join("ssh-control"));
        let ssh = socket
            .as_ref()
            .map(|_| git.ssh_command().unwrap_or_else(|| OsString::from("ssh")));
        if let Some(ssh) = &ssh {
            validate_ssh_command(ssh)?;
            if git
                .ssh_variant()
                .is_some_and(|variant| variant != "ssh" && variant != "auto")
            {
                bail!("persistent SSH requires the OpenSSH variant (ssh or auto); non-OpenSSH variants are unsupported");
            }
        }
        let control_client = ssh
            .as_deref()
            .map(|ssh| ControlClient::resolve(cwd, ssh))
            .transpose()?;
        let mut remote = Self {
            git,
            dir,
            source: transport_source,
            cache,
            socket,
            socket_owner: None,
            ssh,
            control_client,
            failed: None,
        };
        // Authentication and host-key interaction happen here, before mounting.
        let capabilities = tempfile::NamedTempFile::new_in(run)?;
        let mut command = remote.command(true)?;
        command
            .args(["-c", "protocol.version=2"])
            .env("GIT_TRACE_PACKET", capabilities.path())
            .arg("ls-remote")
            .arg("--symref")
            .arg("--")
            .arg(&remote.source)
            .stdin(Stdio::inherit());
        let outcome = crate::git::run(command, true).context("advertise remote refs");
        // Record an established master even if upload-pack or cancellation then
        // fails. The in-memory identity protects Drop; the small private record
        // lets validated stale cleanup identify the same socket after SIGKILL.
        remote.record_socket_owner()?;
        let out = outcome?;
        if !out.status.success() {
            bail!(
                "remote ref advertisement/authentication failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if remote.socket.is_some() && remote.socket_owner.is_none() {
            bail!("OpenSSH did not create the owned control socket");
        }
        let trace = fs::read_to_string(capabilities.path())?;
        if !trace.lines().any(|line| {
            line.split_whitespace()
                .any(|word| word == "filter" || word == "filter\\0")
        }) {
            bail!("remote server does not advertise object filtering; full-clone fallback is forbidden");
        }
        remote.failed = None;
        Ok((remote, out.stdout))
    }
    fn command(&self, initial: bool) -> Result<Command> {
        if let Some(error) = &self.failed {
            bail!("remote object transport previously failed: {error}");
        }
        let mut command = self.git.command(Some(&self.dir));
        command.args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "fetch.writeCommitGraph=false",
        ]);
        if let (Some(socket), Some(ssh)) = (&self.socket, &self.ssh) {
            // Git appends its own SSH argv to this shell command. The function
            // keeps shell expansion of direct commands, removes short -M (which
            // bypasses first-value -o precedence), and leaves all other argv in
            // order. A final -S before Git's host overrides short user -S flags.
            let mut text = OsString::from(SSH_WRAPPER);
            text.push("; git_on_ssh ");
            text.push(ssh);
            text.push(" ");
            let mut boundary = OsString::from("-oControlPath=");
            boundary.push(socket);
            text.push(shell_quote(&boundary));
            text.push(" -S ");
            text.push(shell_quote(socket.as_os_str()));
            if !initial {
                // OpenSSH's mux proxy mode requires the existing socket. Unlike
                // ProxyCommand=false this also works with -J jump-host options.
                text.push(" -O proxy");
            }
            command
                .env("GIT_ON_SSH_CONTROL_PATH", socket)
                .env("GIT_ON_SSH_INITIAL", if initial { "1" } else { "0" });
            command
                .env("GIT_SSH_COMMAND", text)
                .env("GIT_SSH_VARIANT", "ssh");
        }
        if !initial {
            command
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "/bin/false")
                .env("SSH_ASKPASS", "/bin/false");
        }
        Ok(command)
    }
    pub fn resolve(advertisement: &[u8], revision: &str) -> Result<Oid> {
        if revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Oid::from_str(revision)?);
        }
        let mut names = vec![revision.to_owned()];
        if revision != "HEAD" && !revision.starts_with("refs/") {
            names.extend([
                format!("refs/heads/{revision}"),
                format!("refs/tags/{revision}"),
            ]);
        }
        let mut matches = Vec::new();
        for line in advertisement.split(|byte| *byte == b'\n') {
            let Some(separator) = line.iter().position(|byte| *byte == b'\t') else {
                continue;
            };
            let (oid, name) = line.split_at(separator);
            let name = &name[1..];
            // Annotated tags also advertise a peeled-object helper. It is not
            // a ref: selecting it would change which exact object is verified.
            if name.ends_with(b"^{}") {
                continue;
            }
            if names.iter().any(|n| n.as_bytes() == name) && !oid.starts_with(b"ref:") {
                matches.push((
                    name,
                    Oid::from_str(
                        std::str::from_utf8(oid).context("remote object ID is not ASCII")?,
                    )
                    .context("remote SHA-256 repositories are unsupported")?,
                ));
            }
        }
        matches.sort();
        matches.dedup();
        match matches.as_slice() { [(_, oid)] => Ok(*oid), [] => bail!("remote revision {revision:?} is not advertised; use HEAD, an exact ref, branch/tag name, or a full SHA-1 OID (ancestry/reflog syntax unsupported)"), _ => bail!("ambiguous remote revision {revision:?}; use refs/heads/ or refs/tags/") }
    }
    pub fn ensure(&mut self, oid: Oid) -> Result<()> {
        let outcome = (|| {
            let repo = Repository::open_bare(&self.dir)?;
            if repo.odb()?.exists(oid) {
                return Ok(());
            }
            if self.cache.restore(&repo, oid)? {
                return Ok(());
            }
            self.fetch(&repo, oid)
        })();
        if let Err(error) = &outcome {
            // Cache/ODB failures are just as incomplete as a failed fetch. Keep
            // the original failure even when later demands retry that operation.
            if !crate::git::is_request_interruption(error) && self.failed.is_none() {
                self.failed = Some(format!("{error:#}"));
                eprintln!("git-on: remote object loading failed: {error:#}");
            }
        }
        outcome
    }
    fn fetch(&self, repo: &Repository, oid: Oid) -> Result<()> {
        let mut command = self.command(false)?;
        command
            .args([
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--depth=1",
                "--filter=tree:0",
            ])
            .arg("--")
            .arg(&self.source)
            .arg(oid.to_string())
            .stdin(Stdio::null());
        let out = match crate::git::run(command, true) {
            Ok(out) => out,
            Err(error) => {
                if crate::git::is_request_interruption(&error) {
                    // This private repository has one serialized native fetch.
                    // Its owned group is gone before run returns. A forced
                    // shutdown may interrupt Git's shallow-lock rollback.
                    match fs::remove_file(self.dir.join("shallow.lock")) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error)
                                .context("clean interrupted private Git shallow lock");
                        }
                    }
                }
                return Err(error).context("fetch exact remote object");
            }
        };
        let error = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            bail!("remote fetch of exact object {oid} failed (server must allow requested unadvertised OIDs and filtering; SSH connection must remain alive): {}", error.trim());
        }
        if error.contains("filtering not recognized") || error.contains("filter capability") {
            bail!(
                "remote server does not support object filtering; full-clone fallback is forbidden"
            );
        }
        repo.odb()?.refresh()?;
        if !repo.odb()?.exists(oid) {
            bail!("filtered remote fetch did not supply requested object {oid}");
        }
        self.cache.store(repo, oid)?;
        Ok(())
    }
    /// Release the owned master even when the snapshot pin must remain for recovery.
    pub fn stop_transport(&self) -> Result<()> {
        if let (Some(socket), Some(owner)) = (&self.socket, &self.socket_owner) {
            stop_owned_connection(socket, owner)?;
        }
        Ok(())
    }
    fn record_socket_owner(&mut self) -> Result<()> {
        let Some(socket) = &self.socket else {
            return Ok(());
        };
        let metadata = match fs::symlink_metadata(socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect owned SSH control socket"),
        };
        if !metadata.file_type().is_socket() {
            bail!("owned SSH control path is not a socket");
        }
        let owner = SocketOwner {
            format: "git-on-ssh-control-v1".into(),
            device: metadata.dev(),
            inode: metadata.ino(),
            client: self.control_client.clone(),
        };
        self.socket_owner = Some(owner.clone());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(socket.parent().unwrap().join(SSH_CONTROL_OWNER_FILE))
            .context("record owned SSH control socket")?;
        serde_json::to_writer(&mut file, &owner)?;
        std::io::Write::flush(&mut file)?;
        Ok(())
    }
    pub fn failure(&self) -> Option<String> {
        self.failed.clone()
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if let Err(error) = self.stop_transport() {
            eprintln!("git-on: owned SSH shutdown failed: {error:#}");
        }
    }
}
/// Stop only the control connection in a validated run's private directory.
/// Recovery callers must validate and lock that run before invoking this.
pub fn stop_connection(socket: &Path) -> Result<()> {
    let record = socket.parent().unwrap().join(SSH_CONTROL_OWNER_FILE);
    if !fs::symlink_metadata(&record).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return Ok(());
    }
    let Some(value) = File::open(record)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, serde_json::Value>(file).ok())
    else {
        return Ok(());
    };
    let Some((device, inode)) = value
        .get("device")
        .and_then(|x| x.as_u64())
        .zip(value.get("inode").and_then(|x| x.as_u64()))
    else {
        return Ok(());
    };
    if value.get("format").and_then(|x| x.as_str()) != Some("git-on-ssh-control-v1") {
        return Ok(());
    }
    let identity = SocketOwner {
        format: "git-on-ssh-control-v1".into(),
        device,
        inode,
        client: None,
    };
    if !owned_socket(socket, &identity) {
        return Ok(());
    }
    // Once socket ownership is established, malformed client provenance is a
    // recoverable shutdown failure, not permission to unlink a live socket.
    let owner = serde_json::from_value::<SocketOwner>(value)
        .context("invalid owned SSH recovery record; retain the run for --cleanup")?;
    stop_owned_connection(socket, &owner)
}
fn owned_socket(socket: &Path, owner: &SocketOwner) -> bool {
    // Type alone is insufficient: a socket may be hardlinked or replaced after
    // a crash. Match the recorded identity and never follow symbolic links.
    fs::symlink_metadata(socket).is_ok_and(|metadata| {
        metadata.file_type().is_socket()
            && metadata.dev() == owner.device
            && metadata.ino() == owner.inode
    })
}
fn stop_owned_connection(socket: &Path, owner: &SocketOwner) -> Result<()> {
    if !owned_socket(socket, owner) {
        return Ok(());
    }
    if remove_disconnected_socket(socket, owner)? {
        return Ok(());
    }
    let executable = match &owner.client {
        Some(client) => client.executable()?,
        // Old v1 records did not contain executable provenance. Preserve their
        // guarded standard-client recovery, without interpreting shell text.
        None => PathBuf::from("ssh"),
    };
    if !owned_socket(socket, owner) {
        return Ok(());
    }
    let mut child = Command::new(&executable)
        .args(["-F", "/dev/null", "-S"])
        .arg(socket)
        .args(["-O", "exit", "git-on-control"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start owned SSH shutdown client")?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("wait for owned SSH shutdown client")?
        {
            break status;
        }
        if start.elapsed() > std::time::Duration::from_secs(1) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("owned SSH shutdown client timed out; retain the run for --cleanup");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    while owned_socket(socket, owner) && start.elapsed() < std::time::Duration::from_secs(1) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if owned_socket(socket, owner) && !remove_disconnected_socket(socket, owner)? {
        bail!("owned SSH shutdown did not close its control socket ({status}); retain the run for --cleanup");
    }
    Ok(())
}
fn remove_disconnected_socket(socket: &Path, owner: &SocketOwner) -> Result<bool> {
    if !endpoint_absent(socket)? {
        return Ok(false);
    }
    // SIGKILL of a master can leave a pathname after the listener closes.
    // Recheck ownership immediately before removing only that stale node.
    if owned_socket(socket, owner) {
        fs::remove_file(socket).context("remove disconnected owned SSH control socket")?;
    }
    Ok(true)
}
fn endpoint_absent(socket: &Path) -> Result<bool> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path = socket.as_os_str().as_bytes();
    if path.len() >= address.sun_path.len() || path.contains(&0) {
        bail!("invalid owned SSH socket pathname");
    }
    for (destination, byte) in address.sun_path.iter_mut().zip(path) {
        *destination = *byte as libc::c_char;
    }
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("inspect owned SSH control endpoint");
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let result = unsafe {
        libc::connect(
            file.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result == 0 {
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ECONNREFUSED | libc::ENOENT) => Ok(true),
        // A busy live listener is not evidence that its endpoint disappeared.
        Some(libc::EINPROGRESS | libc::EAGAIN) => Ok(false),
        _ => Err(error).context("inspect owned SSH control endpoint"),
    }
}
impl ControlClient {
    fn resolve(cwd: &Path, ssh: &OsStr) -> Result<Self> {
        // Use the same shell expansion/quoting as the supported direct command,
        // but emit only its executable. No command arguments are persisted.
        let mut script = OsString::from(
            r#"git_on_client() {
            case "$1" in env|exec|command|sh|bash|dash|zsh|ksh|*=*)
                echo 'git-on: SSH shell prefixes are unsupported' >&2; exit 125;; esac
            command -v "$1"
        }; git_on_client "#,
        );
        script.push(ssh);
        let mut command = Command::new("/bin/sh");
        command
            .current_dir(cwd)
            .args([OsStr::new("-c"), &script])
            .stdin(Stdio::null());
        let output =
            crate::git::run(command, true).context("resolve selected SSH shutdown client")?;
        if !output.status.success() {
            bail!(
                "selected SSH executable cannot be resolved: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let selected = PathBuf::from(OsString::from_vec(
            output
                .stdout
                .strip_suffix(b"\n")
                .unwrap_or(&output.stdout)
                .to_vec(),
        ));
        let path = fs::canonicalize(if selected.is_absolute() {
            selected
        } else {
            cwd.join(selected)
        })
        .context("resolve selected SSH executable path")?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
            bail!("selected SSH shutdown client must be a regular executable file");
        }
        Ok(Self {
            path: path.as_os_str().as_bytes().to_vec(),
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        })
    }
    fn executable(&self) -> Result<PathBuf> {
        let path = PathBuf::from(OsString::from_vec(self.path.clone()));
        if !path.is_absolute() || self.path.contains(&0) {
            bail!("invalid recorded SSH shutdown executable path; retain the run for --cleanup");
        }
        let metadata =
            fs::symlink_metadata(&path).context("inspect recorded SSH shutdown executable")?;
        if !metadata.file_type().is_file()
            || metadata.permissions().mode() & 0o111 == 0
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
            || metadata.len() != self.size
            || metadata.mtime() != self.modified_seconds
            || metadata.mtime_nsec() != self.modified_nanoseconds
        {
            bail!("recorded SSH shutdown executable changed; retain the run for --cleanup");
        }
        Ok(path)
    }
}
fn validate_ssh_command(command: &OsStr) -> Result<()> {
    let mut quote = 0;
    let mut escaped = false;
    let bytes = command.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote != b'\'' && byte == b'\\' {
            escaped = true;
            continue;
        }
        if quote == b'\'' {
            if byte == b'\'' {
                quote = 0;
            }
            continue;
        }
        if byte == b'"' {
            quote = if quote == b'"' { 0 } else { b'"' };
            continue;
        }
        if quote == 0 && byte == b'\'' {
            quote = b'\'';
            continue;
        }
        if byte == b'`'
            || (byte == b'$' && bytes.get(index + 1) == Some(&b'('))
            || (quote == 0 && matches!(byte, b';' | b'|' | b'&' | b'<' | b'>' | b'\n'))
        {
            bail!("SSH command must be one direct OpenSSH command or argv-forwarding wrapper; shell compounds and command substitutions are unsupported");
        }
    }
    if quote != 0 || escaped || bytes.is_empty() {
        bail!("SSH command must contain a nonempty, correctly quoted direct executable");
    }
    Ok(())
}
// This is a direct command wrapper, as used by Git for GIT_SSH_COMMAND. Shell
// prefixes and compounds cannot safely identify the OpenSSH option boundary.
// Transparent GIT_SSH executables that forward OpenSSH argv remain supported.
const SSH_WRAPPER: &str = r#"git_on_ssh() {
    executable=$1; shift
    case "$executable" in
        env|exec|command|sh|bash|dash|zsh|ksh|*=*)
            echo 'git-on: SSH command must name an OpenSSH executable or an argv-forwarding wrapper directly; shell prefixes are unsupported' >&2
            exit 125 ;;
    esac
    remaining=$#; scan=1; skip=0
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1)); drop=0
        if [ "$argument" = "-oControlPath=$GIT_ON_SSH_CONTROL_PATH" ]; then scan=0; fi
        if [ "$scan" = 1 ]; then
            if [ "$skip" = 1 ]; then
                skip=0
            else
                case "$argument" in
                    --)
                        echo 'git-on: SSH command may not end option parsing before Git supplies its host' >&2
                        exit 125 ;;
                    -*)
                        flags=${argument#-}; kept=-
                        while [ -n "$flags" ]; do
                            flag=${flags%"${flags#?}"}; flags=${flags#?}
                            case "$flag" in
                                M) ;;
                                O)
                                    echo 'git-on: SSH control commands (-O) are owned by the runner' >&2
                                    exit 125 ;;
                                B|b|c|D|E|e|F|I|i|J|L|l|m|o|p|Q|R|S|W|w)
                                    kept=$kept$flag$flags
                                    if [ -z "$flags" ]; then skip=1; fi
                                    flags= ;;
                                *) kept=$kept$flag ;;
                            esac
                        done
                        argument=$kept
                        if [ "$argument" = '-' ]; then drop=1; fi ;;
                esac
            fi
        fi
        if [ "$drop" = 0 ]; then set -- "$@" "$argument"; fi
    done
    if [ "$GIT_ON_SSH_INITIAL" = 1 ]; then
        exec "$executable" -o "ControlPath=$GIT_ON_SSH_CONTROL_PATH" -o ControlMaster=auto -o ControlPersist=yes "$@"
    else
        exec "$executable" -o "ControlPath=$GIT_ON_SSH_CONTROL_PATH" -o ControlMaster=no -o BatchMode=yes "$@"
    fi
}"#;

fn shell_quote(arg: &OsStr) -> OsString {
    let mut out = vec![b'\''];
    for b in arg.as_bytes() {
        if *b == b'\'' {
            out.extend(b"'\\''");
        } else {
            out.push(*b);
        }
    }
    out.push(b'\'');
    OsString::from_vec(out)
}

/// Persistent loose objects are copied into each run's private repository.
/// Eviction cannot invalidate active readers; temporary run storage has no quota.
pub struct DiskCache {
    root: PathBuf,
    budget: u64,
    entries: usize,
}
impl DiskCache {
    pub fn standard() -> Result<Self> {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|x| PathBuf::from(x).join(".cache")))
            .context("remote cache requires XDG_CACHE_HOME or HOME")?
            .join("git-on/objects-v1");
        Self::new(root, DISK_CACHE_BYTES, DISK_CACHE_ENTRIES)
    }
    pub fn new(root: PathBuf, budget: u64, entries: usize) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        Ok(Self {
            root,
            budget,
            entries,
        })
    }
    fn lock(&self) -> Result<File> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join("lock"))?;
        loop {
            if let Some(signal) = crate::git::cancelled_signal() {
                bail!("remote cache lock cancelled by signal {signal}");
            }
            if crate::git::operations_stopped() {
                bail!("remote cache lock stopped after command completion");
            }
            crate::git::check_request_interruption()?;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock
                && error.kind() != std::io::ErrorKind::Interrupted
            {
                return Err(error.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.reconcile()?;
        Ok(f)
    }
    fn reconcile(&self) -> Result<()> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                bail!("unrecognized remote cache entry");
            };
            if name == "lock" {
                continue;
            }
            if name.starts_with(".tmp") {
                // Every writer holds the same flock through atomic publication.
                // With that lock held, these are interrupted staging files.
                fs::remove_file(entry.path())?;
                continue;
            }
            if name.len() != 40 || Oid::from_str(name).is_err() || !entry.file_type()?.is_file() {
                bail!("unrecognized remote cache entry {name:?}");
            }
            let metadata = entry.metadata()?;
            files.push((metadata.modified()?, metadata.len(), entry.path()));
        }
        files.sort_by_key(|x| x.0);
        let mut bytes: u64 = files.iter().map(|x| x.1).sum();
        let mut count = files.len();
        for (_, size, path) in files {
            if bytes <= self.budget && count <= self.entries {
                break;
            }
            fs::remove_file(path)?;
            bytes = bytes.saturating_sub(size);
            count -= 1;
        }
        Ok(())
    }
    fn restore(&self, repo: &Repository, oid: Oid) -> Result<bool> {
        let _lock = self.lock()?;
        let file = self.root.join(oid.to_string());
        let data = match fs::read(&file) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let Some((&kind, data)) = data.split_first() else {
            fs::remove_file(file)?;
            return Ok(false);
        };
        let Some(kind) = git2::ObjectType::from_raw(i32::from(kind)) else {
            fs::remove_file(file)?;
            return Ok(false);
        };
        if !matches!(
            kind,
            git2::ObjectType::Commit
                | git2::ObjectType::Tree
                | git2::ObjectType::Blob
                | git2::ObjectType::Tag
        ) || Oid::hash_object(kind, data)? != oid
        {
            fs::remove_file(file)?;
            return Ok(false);
        }
        repo.odb()?.write(kind, data)?;
        File::open(file)?
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
        Ok(true)
    }
    fn store(&self, repo: &Repository, oid: Oid) -> Result<()> {
        let odb = repo.odb()?;
        let object = odb.read(oid)?;
        let weight = object.len() as u64 + 1;
        let _lock = self.lock()?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.len() == 40 && Oid::from_str(name).is_ok())
            {
                let metadata = entry.metadata()?;
                files.push((metadata.modified()?, metadata.len(), entry.path()));
            }
        }
        files.sort_by_key(|x| x.0);
        let mut total: u64 = files.iter().map(|x| x.1).sum();
        let mut count = files.len();
        let target = self.root.join(oid.to_string());
        if target.exists() {
            return Ok(());
        }
        let retain = weight <= self.budget && self.entries > 0;
        for (_, size, path) in files {
            if total.saturating_add(if retain { weight } else { 0 }) <= self.budget
                && count + usize::from(retain) <= self.entries
            {
                break;
            }
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
            count -= 1;
        }
        if retain {
            use std::io::Write;
            let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
            temporary.write_all(&[object.kind().raw() as u8])?;
            temporary.write_all(object.data())?;
            temporary.persist(target)?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn total(root: &Path) -> u64 {
        fs::read_dir(root)
            .unwrap()
            .map(|x| x.unwrap().metadata().unwrap().len())
            .sum()
    }
    #[test]
    fn review_abandoned_staging_files_are_reclaimed_with_cache_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let cache = DiskCache::new(root.clone(), 64, 2).unwrap();
        fs::write(root.join(".tmpAbandonedWrite"), vec![0; 128]).unwrap();
        let repo = Repository::init_bare(temporary.path().join("repository")).unwrap();
        let oid = repo.blob(b"demanded").unwrap();
        cache.store(&repo, oid).unwrap();
        assert!(!root.join(".tmpAbandonedWrite").exists());
        assert!(total(&root) <= 64);
    }
    #[test]
    fn concurrent_eviction_keeps_active_run_copies_and_bounds_bytes_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let cache = DiskCache::new(root.clone(), 64, 2).unwrap();
        let original = Repository::init_bare(temporary.path().join("original")).unwrap();
        let active = Repository::init_bare(temporary.path().join("active")).unwrap();
        let oid = original.blob(b"active copy").unwrap();
        cache.store(&original, oid).unwrap();
        assert!(cache.restore(&active, oid).unwrap());
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let root = root.clone();
                let path = temporary.path().join(format!("worker-{worker}"));
                scope.spawn(move || {
                    let cache = DiskCache::new(root.clone(), 64, 2).unwrap();
                    let repo = Repository::init_bare(path).unwrap();
                    for n in 0..30 {
                        let oid = repo.blob(&[worker, n]).unwrap();
                        cache.store(&repo, oid).unwrap();
                        let _lock = cache.lock().unwrap();
                        assert!(total(&root) <= 64);
                        assert!(fs::read_dir(&root).unwrap().count() <= 3);
                    }
                });
            }
        });
        assert_eq!(active.find_blob(oid).unwrap().content(), b"active copy");
        let oversized = original.blob(&[0; 65]).unwrap();
        cache.store(&original, oversized).unwrap();
        assert!(!root.join(oversized.to_string()).exists());
    }
}
