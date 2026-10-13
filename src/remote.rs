//! Explicit one-object fetches. Git's promisor auto-fetch is never the loader.
use crate::git::Git;
use anyhow::{bail, Context, Result};
use git2::{Oid, Repository};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const DISK_CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const DISK_CACHE_ENTRIES: usize = 4096;
pub const SSH_CONTROL_OWNER_FILE: &str = "ssh-control-owner";

fn require_native_git(cwd: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .arg("--version")
        .stdin(Stdio::null());
    let output = crate::git::run(command, true).map_err(|error| {
        let missing = error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        });
        error.context(if missing {
            "native Git was not found on PATH; remote execution requires Git 2.45.1 or newer; install a supported Git with your distribution tools and check PATH"
        } else {
            "cannot run native Git --version; remote execution requires Git 2.45.1 or newer; check the Git executable on PATH"
        })
    })?;
    if !output.status.success() {
        bail!("native Git --version failed: {}; remote execution requires Git 2.45.1 or newer; check the Git executable on PATH", String::from_utf8_lossy(&output.stderr).trim());
    }
    let detected = String::from_utf8_lossy(&output.stdout);
    let version = detected
        .strip_prefix("git version ")
        .and_then(|value| value.split_whitespace().next());
    let numbers = version
        .map(|value| {
            value
                .split('.')
                .take(3)
                .map(str::parse::<u32>)
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .transpose()
        .ok()
        .flatten()
        .filter(|numbers| numbers.len() == 3);
    let Some(numbers) = numbers else {
        bail!("unrecognized native Git version {:?}; remote execution requires Git 2.45.1 or newer; install a supported Git and check PATH", detected.trim());
    };
    if numbers.as_slice() < [2, 45, 1].as_slice() {
        bail!("detected native Git {}; remote execution requires Git 2.45.1 or newer to disable automatic promisor fetches; upgrade Git with your distribution tools or select a supported Git on PATH", version.unwrap());
    }
    Ok(())
}

fn github_https_alternative(source: &str) -> Option<String> {
    let path = source.strip_prefix("git@github.com:").or_else(|| {
        let (authority, path) = source.strip_prefix("ssh://")?.split_once('/')?;
        matches!(
            authority,
            "git@github.com" | "git@github.com:22" | "github.com" | "github.com:22"
        )
        .then_some(path)
    })?;
    let components: Vec<_> = path.split('/').collect();
    // Only a plain GitHub owner/repository path becomes a suggested URL. Never
    // reinterpret arbitrary hosts, URL credentials, query strings or escapes.
    if components.len() != 2
        || components.iter().any(|component| {
            component.is_empty()
                || matches!(*component, "." | "..")
                || !component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return None;
    }
    Some(format!("https://github.com/{path}"))
}

fn authentication_advice(stderr: &str, https_alternative: Option<&str>) -> String {
    if !stderr.to_ascii_lowercase().contains("publickey") {
        return String::new();
    }
    let mut advice = String::from("; check your existing SSH key/agent and repository access");
    if let Some(url) = https_alternative {
        advice.push_str(&format!("; if this GitHub repository is public, try {url} instead (public HTTPS needs no SSH key; private HTTPS still requires your existing credentials)"));
    }
    advice
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_sha256: Option<[u8; 32]>,
}

pub struct Remote {
    pub git: Git,
    pub dir: PathBuf,
    source: OsString,
    https_alternative: Option<String>,
    cache: DiskCache,
    socket: Option<PathBuf>,
    socket_owner: Option<SocketOwner>,
    ssh: Option<OsString>,
    control_client: Option<ControlClient>,
    failed: Option<String>,
}
impl Remote {
    pub fn open(cwd: &Path, source: &OsStr, revision: &OsStr, run: &Path) -> Result<(Self, Oid)> {
        // Capture effective configuration first: even `git --version` can read
        // it. Preserve the first native operation's cancellation/TTY lifecycle.
        let git = Git::capture(cwd).map_err(|error| {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) {
                error.context("remote execution requires native Git 2.45.1 or newer; install a supported Git with your distribution tools and check PATH")
            } else {
                error
            }
        })?;
        require_native_git(cwd)?;
        // Git owns repository spelling, named remotes and URL rewriting. This
        // native query is only used to configure the optional owned SSH master;
        // every network operation still receives the original source operand.
        let url = git.output(
            None,
            &[
                OsStr::new("ls-remote"),
                OsStr::new("--get-url"),
                OsStr::new("--"),
                source,
            ],
        )?;
        let url = url.strip_suffix(b"\n").unwrap_or(&url);
        let text = String::from_utf8_lossy(url);
        let ssh_transport = ["ssh://", "git+ssh://", "ssh+git://"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
            || (!text.contains("://")
                && !text.contains("::")
                && text
                    .find(':')
                    .is_some_and(|colon| text.find('/').is_none_or(|slash| colon < slash)));
        let dir = run.join("remote-repository");
        let repo = Repository::init_bare(&dir)?;
        // Native Git uses the original source as an implicit promisor remote
        // name. An absolute path is a valid fetch operand but an invalid remote
        // name, so provide separate owned metadata for filtered connectivity.
        // It has no URL: GIT_NO_LAZY_FETCH forbids automatic object loading, and
        // every explicit demand below still passes the original source to Git.
        repo.config()?.set_bool(
            &format!("remote.git-on-owned-{}.promisor", uuid::Uuid::new_v4()),
            true,
        )?;
        drop(repo);
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
            source: source.to_os_string(),
            https_alternative: github_https_alternative(&text),
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
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "remote ref advertisement/authentication failed: {}{}",
                stderr.trim(),
                authentication_advice(&stderr, remote.https_alternative.as_deref())
            );
        }
        if remote.socket.is_some() && remote.socket_owner.is_none() {
            bail!("OpenSSH did not create the owned control socket");
        }
        let trace = fs::read(capabilities.path())?;
        if !trace.split(|byte| *byte == b'\n').any(|line| {
            line.split(u8::is_ascii_whitespace)
                .any(|word| word == b"filter" || word == b"filter\\0")
        }) {
            bail!("remote server does not advertise object filtering; full-clone fallback is forbidden");
        }
        remote.failed = None;
        let oid = remote.fetch_selected(revision)?;
        Ok((remote, oid))
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
            // A fetched destination ref names a commit, but its trees and
            // blobs may still be absent. Advertising that commit as a 'have'
            // can make the server omit an explicitly demanded descendant.
            "-c",
            "fetch.negotiationAlgorithm=noop",
        ]);
        if let (Some(socket), Some(ssh)) = (&self.socket, &self.ssh) {
            // Git appends argv to the outer function, so its argument count
            // gives the inner function a structural user/Git boundary. User
            // shell expansion cannot impersonate that boundary. Remove short
            // -M (which bypasses first-value -o precedence) and place the owned
            // -S/-O after user options and before Git's host.
            let mut text = OsString::from(SSH_WRAPPER);
            text.push("; git_on_ssh() { git_on_git_argc=$#; git_on_ssh_command ");
            text.push(ssh);
            text.push(" \"$@\"; }; git_on_ssh");
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
    fn fetch_selected(&self, revision: &OsStr) -> Result<Oid> {
        // A fetch refspec belongs to native Git, including its leading '+',
        // namespace abbreviation and destination. All resulting refs and
        // FETCH_HEAD live in this owned bare repository, never in the caller.
        let mut command = self.command(false)?;
        command
            .args(["fetch", "--no-tags", "--depth=1", "--filter=tree:0"])
            .arg("--")
            .arg(&self.source)
            .arg(revision)
            .stdin(Stdio::null());
        let out = crate::git::run(command, true).context("fetch selected remote revision")?;
        let error = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            bail!(
                "native Git fetch failed: {}{}",
                error.trim(),
                authentication_advice(&error, self.https_alternative.as_deref())
            );
        }
        if error.contains("filtering not recognized") || error.contains("filter capability") {
            bail!(
                "remote server does not support object filtering; full-clone fallback is forbidden"
            );
        }
        let head = fs::read(self.dir.join("FETCH_HEAD")).context("read native Git fetch result")?;
        let mut selected = None;
        for line in head
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let oid = line.split(|byte| *byte == b'\t').next().unwrap();
            let oid = Oid::from_str(std::str::from_utf8(oid)?).context(
                "native Git fetched a non-SHA-1 object (SHA-256 repositories unsupported)",
            )?;
            if selected.is_some_and(|selected| selected != oid) {
                bail!("native Git fetch selected multiple objects; a snapshot requires exactly one selected object");
            }
            selected = Some(oid);
        }
        let oid = selected.context("native Git fetch selected no object to mount")?;
        let repo = Repository::open_bare(&self.dir)?;
        if !repo.odb()?.exists(oid) {
            bail!("native Git fetch did not supply selected object {oid}");
        }
        self.cache.store(&repo, oid)?;
        Ok(oid)
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
            bail!("remote fetch of exact object {oid} failed (server must allow requested unadvertised OIDs and filtering; SSH connection must remain alive): {}{}", error.trim(), authentication_advice(&error, self.https_alternative.as_deref()));
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
    if let Some(client) = &owner.client {
        client.validate()?;
    }
    if !owned_socket(socket, owner) {
        return Ok(());
    }
    // A digest followed by exec(path), or even an open executable fd, still
    // permits replacement/in-place mutation between validation and execution.
    // Send OpenSSH's mux terminate request ourselves: shutdown executes no
    // selected-client bytes, including script wrappers or legacy PATH clients.
    let deadline = Instant::now() + Duration::from_secs(1);
    if let Err(error) = terminate_owned_master(socket, owner, deadline) {
        if remove_disconnected_socket(socket, owner)? {
            return Ok(());
        }
        return Err(error).context("owned SSH mux shutdown failed; retain the run for --cleanup");
    }
    while owned_socket(socket, owner) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if owned_socket(socket, owner) && !remove_disconnected_socket(socket, owner)? {
        bail!("owned SSH shutdown did not close its control socket; retain the run for --cleanup");
    }
    Ok(())
}
fn terminate_owned_master(socket: &Path, owner: &SocketOwner, deadline: Instant) -> Result<()> {
    let mut stream = loop {
        if !owned_socket(socket, owner) {
            bail!("owned SSH socket changed before shutdown");
        }
        mux_remaining(deadline)?;
        let (stream, error) = connect_control_endpoint(socket)?;
        match error {
            None => {
                if !owned_socket(socket, owner) {
                    bail!("owned SSH socket changed during shutdown connection");
                }
                stream.set_nonblocking(false)?;
                break stream;
            }
            Some(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EINPROGRESS | libc::EAGAIN | libc::EINTR)
                ) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Some(error) => return Err(error).context("connect to owned SSH control endpoint"),
        }
    };
    exchange_mux_shutdown(&mut stream, deadline)
}
fn mux_remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("owned SSH mux shutdown timed out");
    }
    Ok(remaining)
}
fn mux_write(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(mux_remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => bail!("owned SSH control endpoint closed during shutdown"),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("write owned SSH mux shutdown"),
        }
    }
    Ok(())
}
fn mux_read(stream: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(mux_remaining(deadline)?))?;
        match stream.read(bytes) {
            Ok(0) => bail!("owned SSH control endpoint closed during shutdown"),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("read owned SSH mux shutdown"),
        }
    }
    Ok(())
}
fn mux_packet(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut length = [0; 4];
    mux_read(stream, &mut length, deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    // Bound an unsupported/malformed local peer before allocating its packet.
    if !(8..=65536).contains(&length) {
        bail!("invalid owned SSH mux packet length {length}");
    }
    let mut packet = vec![0; length];
    mux_read(stream, &mut packet, deadline)?;
    Ok(packet)
}
fn exchange_mux_shutdown(stream: &mut UnixStream, deadline: Instant) -> Result<()> {
    // OpenSSH PROTOCOL.mux: version 4 HELLO, then TERMINATE with request id 0.
    // No session, authentication, TCP connection or external client is created.
    mux_write(stream, &[0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 4], deadline)?;
    let hello = mux_packet(stream, deadline)?;
    if hello[..8] != [0, 0, 0, 1, 0, 0, 0, 4] {
        bail!("unsupported owned SSH mux greeting");
    }
    let mut extensions = &hello[8..];
    while !extensions.is_empty() {
        for _ in 0..2 {
            let length = extensions
                .get(..4)
                .context("invalid owned SSH mux greeting extension")?;
            let length = u32::from_be_bytes(length.try_into()?) as usize;
            extensions = extensions
                .get(4..)
                .and_then(|bytes| bytes.get(length..))
                .context("invalid owned SSH mux greeting extension")?;
        }
    }
    mux_write(stream, &[0, 0, 0, 8, 0x10, 0, 0, 5, 0, 0, 0, 0], deadline)?;
    let reply = mux_packet(stream, deadline)?;
    if reply != [0x80, 0, 0, 1, 0, 0, 0, 0] {
        bail!("owned SSH master refused or returned an invalid shutdown acknowledgement");
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
        match fs::remove_file(socket) {
            Ok(()) => {}
            // OpenSSH may remove its own node after the ownership check.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("remove disconnected owned SSH control socket")
            }
        }
    }
    Ok(true)
}
fn endpoint_absent(socket: &Path) -> Result<bool> {
    let (_stream, error) = connect_control_endpoint(socket)?;
    let Some(error) = error else {
        return Ok(false);
    };
    match error.raw_os_error() {
        Some(libc::ECONNREFUSED | libc::ENOENT) => Ok(true),
        // A busy live listener is not evidence that its endpoint disappeared.
        Some(libc::EINPROGRESS | libc::EAGAIN) => Ok(false),
        _ => Err(error).context("inspect owned SSH control endpoint"),
    }
}
fn connect_control_endpoint(socket: &Path) -> Result<(UnixStream, Option<std::io::Error>)> {
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
    let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    Ok((stream, (result != 0).then(std::io::Error::last_os_error)))
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
        let mut file = client_file(&path)?;
        let metadata = file.metadata()?;
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
            content_sha256: Some(client_digest(&mut file)?),
        })
    }
    fn validate(&self) -> Result<()> {
        let path = PathBuf::from(OsString::from_vec(self.path.clone()));
        if !path.is_absolute() || self.path.contains(&0) {
            bail!("invalid recorded SSH shutdown executable path; retain the run for --cleanup");
        }
        let mut file = client_file(&path).context("inspect recorded SSH shutdown executable")?;
        let metadata = file.metadata()?;
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
        let digest = self.content_sha256.context(
            "recorded SSH shutdown executable lacks a content fingerprint; close the owned master with a trusted client, then retry --cleanup",
        )?;
        if client_digest(&mut file)? != digest {
            bail!("recorded SSH shutdown executable changed; retain the run for --cleanup");
        }
        Ok(())
    }
}
fn client_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?)
}
fn client_digest(file: &mut File) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut bytes = [0; 8192];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
    }
    Ok(digest.finalize().into())
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
// prefixes and compounds cannot safely identify the OpenSSH executable.
// Transparent GIT_SSH executables that forward OpenSSH argv remain supported.
const SSH_WRAPPER: &str = r#"git_on_ssh_command() {
    executable=$1; shift
    case "$executable" in
        env|exec|command|sh|bash|dash|zsh|ksh|*=*)
            echo 'git-on: SSH command must name an OpenSSH executable or an argv-forwarding wrapper directly; shell prefixes are unsupported' >&2
            exit 125 ;;
    esac
    remaining=$(($# - git_on_git_argc)); skip=0
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1)); drop=0
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
                            B|b|c|D|E|e|F|I|i|J|L|l|m|o|p|P|Q|R|S|W|w)
                                kept=$kept$flag$flags
                                if [ -z "$flags" ]; then skip=1; fi
                                flags= ;;
                            *) kept=$kept$flag ;;
                        esac
                    done
                    argument=$kept
                    if [ "$argument" = '--' ]; then
                        echo 'git-on: SSH command may not end option parsing before Git supplies its host' >&2
                        exit 125
                    fi
                    if [ "$argument" = '-' ]; then drop=1; fi ;;
                *)
                    echo 'git-on: SSH command may contain only an executable and options; Git supplies the host and remote command' >&2
                    exit 125 ;;
            esac
        fi
        if [ "$drop" = 0 ]; then set -- "$@" "$argument"; fi
    done
    if [ "$skip" = 1 ]; then
        echo 'git-on: SSH option is missing its argument' >&2
        exit 125
    fi
    set -- "$@" -S "$GIT_ON_SSH_CONTROL_PATH"
    if [ "$GIT_ON_SSH_INITIAL" != 1 ]; then
        # Mux proxy mode requires the existing socket, including with -J.
        set -- "$@" -O proxy
    fi
    remaining=$git_on_git_argc
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1))
        set -- "$@" "$argument"
    done
    if [ "$GIT_ON_SSH_INITIAL" = 1 ]; then
        exec "$executable" -o "ControlPath=$GIT_ON_SSH_CONTROL_PATH" -o ControlMaster=auto -o ControlPersist=yes "$@"
    else
        exec "$executable" -o "ControlPath=$GIT_ON_SSH_CONTROL_PATH" -o ControlMaster=no -o BatchMode=yes "$@"
    fi
}"#;

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
    #[test]
    fn mux_shutdown_rejects_invalid_greetings_before_termination() {
        for greeting in [
            vec![0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 5],
            vec![0, 1, 0, 1],
            vec![0, 0, 0, 9, 0, 0, 0, 1, 0, 0, 0, 4, 0],
        ] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let peer = std::thread::spawn(move || {
                let mut hello = [0; 12];
                server.read_exact(&mut hello).unwrap();
                server.write_all(&greeting).unwrap();
                server
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut termination = [0; 12];
                assert_eq!(server.read(&mut termination).unwrap(), 0);
            });
            assert!(
                exchange_mux_shutdown(&mut client, Instant::now() + Duration::from_secs(1))
                    .is_err()
            );
            drop(client);
            peer.join().unwrap();
        }
    }
    #[test]
    fn mux_shutdown_rejects_nonmatching_acknowledgements() {
        for reply in [[0x80, 0, 0, 1, 0, 0, 0, 1], [0x80, 0, 0, 2, 0, 0, 0, 0]] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let peer = std::thread::spawn(move || {
                let mut request = [0; 12];
                server.read_exact(&mut request).unwrap();
                server
                    .write_all(&[0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 4])
                    .unwrap();
                server.read_exact(&mut request).unwrap();
                server.write_all(&8u32.to_be_bytes()).unwrap();
                server.write_all(&reply).unwrap();
            });
            assert!(
                exchange_mux_shutdown(&mut client, Instant::now() + Duration::from_secs(1))
                    .is_err()
            );
            peer.join().unwrap();
        }
    }
    #[test]
    fn mux_shutdown_deadline_cannot_be_extended_by_partial_packets() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            let mut hello = [0; 12];
            server.read_exact(&mut hello).unwrap();
            for byte in [0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 4] {
                if server.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        assert!(exchange_mux_shutdown(&mut client, start + Duration::from_millis(60)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(client);
        peer.join().unwrap();
    }
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
