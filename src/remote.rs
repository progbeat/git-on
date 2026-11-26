//! Explicit exact-object fetches. Git's promisor auto-fetch is never the loader.
use crate::{
    git::Git,
    raw_blob::{ReceivedObject, Transfer},
    service_storage::Directory,
    transport,
};
use anyhow::{bail, Context, Result};
use git2::Oid;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

pub const SSH_CONTROL_OWNER_FILE: &str = "ssh-control-owner";
const SSH_LAUNCHER: &str = "git-on-ssh-launcher";
const SSH_WARM_DELAY: Duration = Duration::from_millis(64);
const SSH_READ_ONLY: &str = "StrictHostKeyChecking=yes UpdateHostKeys=no AddKeysToAgent=no ClearAllForwardings=yes ForwardAgent=no ForwardX11=no PermitLocalCommand=no Tunnel=no";

mod ssh_host_files;

#[doc(hidden)]
pub fn invoke_ssh_launcher() -> Option<i32> {
    let mut arguments = std::env::args_os().peekable();
    let launcher = PathBuf::from(arguments.next()?);
    if launcher.file_name()? != SSH_LAUNCHER
        || std::env::var_os("GIT_ON_SSH_LAUNCHER").as_deref() != Some(launcher.as_os_str())
    {
        return None;
    }
    if arguments.next()? != "--launch" {
        return None;
    }
    let program = arguments.next()?;
    let git_arguments = arguments.next()?.to_str()?.parse::<usize>().ok()?;
    let options: Vec<_> = arguments.collect();
    let settings = options
        .len()
        .checked_sub(git_arguments)
        .context("SSH launcher is missing the Git arguments")
        .and_then(|before_host| ssh_host_files::launch(&program, &launcher, options, before_host));
    let error = match settings {
        Err(error) => error,
        Ok(options) => Command::new(program).args(options).exec().into(),
    };
    eprintln!("git-on: cannot start SSH client: {error}");
    Some(125)
}

fn require_native_git(cwd: &Path) -> Result<String> {
    let mut command = crate::git::native_git_command();
    command
        .current_dir(cwd)
        .arg("--version")
        .stdin(Stdio::null());
    let output = crate::git::run_builtin(command).map_err(|error| {
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
    let description = detected
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("git version "));
    let version = description.and_then(|value| value.split_whitespace().next());
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
    Ok(description.unwrap().to_owned())
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

#[derive(Clone, Copy)]
enum SnapshotTarget<'a> {
    Object(Oid),
    Ref(&'a transport::RefQuery),
}

impl<'a> SnapshotTarget<'a> {
    fn reference(self) -> Option<&'a transport::RefQuery> {
        match self {
            Self::Object(_) => None,
            Self::Ref(query) => Some(query),
        }
    }

    fn oid(self, selected: Option<Oid>) -> Option<Oid> {
        match self {
            Self::Object(oid) => Some(oid),
            Self::Ref(_) => selected,
        }
    }
}

fn exact_oid(revision: &OsStr) -> Option<Oid> {
    let value = revision.to_str()?;
    (value.len() == 40)
        .then(|| Oid::from_str(value).ok())
        .flatten()
}

pub(crate) enum Source {
    Local(PathBuf),
    Network(NetworkSource),
}

pub(crate) struct NetworkSource {
    git: Git,
    source: OsString,
    url: OsString,
    ssh: bool,
}

pub(crate) fn resolve_source(cwd: &Path, source: &OsStr, verify: bool) -> Result<Source> {
    // Capture effective configuration first: even `git --version` can read
    // it. Preserve the first native operation's cancellation/TTY lifecycle.
    // Config includes use the caller's context for network verification.
    let git = if verify {
        Git::capture(cwd)
    } else {
        Git::capture_configuration(cwd)
    }
    .map_err(|error| {
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
    // Resolve named remotes and Git's URL rewrites before choosing a transport.
    let mut url = git.output(
        None,
        &[
            OsStr::new("ls-remote"),
            OsStr::new("--get-url"),
            OsStr::new("--"),
            source,
        ],
    )?;
    if url.last() == Some(&b'\n') {
        url.pop();
    }
    if let Some(file) = url.strip_prefix(b"file://") {
        let start = file
            .iter()
            .position(|&byte| byte == b'/')
            .context("file URL requires an absolute repository path")?;
        return Ok(Source::Local(decode_file_path(&file[start..])?));
    }
    let text = String::from_utf8_lossy(&url);
    let separator = text
        .split_once(':')
        .filter(|(prefix, _)| !prefix.contains('/'));
    if separator.is_none() {
        anyhow::ensure!(!url.is_empty(), "missing local repository path");
        return Ok(Source::Local(cwd.join(OsStr::from_bytes(&url))));
    }
    anyhow::ensure!(
        !git.has_multiple_group_members(source),
        "select one remote; remote groups are outside the direct transport profile"
    );
    let ssh = text.starts_with("ssh://")
        || separator.is_some_and(|(prefix, tail)| {
            !tail.starts_with("//") && (!tail.starts_with(':') || prefix.contains('['))
        });
    anyhow::ensure!(
        ssh || text.starts_with("https://"),
        "remote transport must be direct HTTPS or OpenSSH"
    );
    Ok(Source::Network(NetworkSource {
        git,
        source: source.to_os_string(),
        url: OsString::from_vec(url),
        ssh,
    }))
}

fn decode_file_path(encoded: &[u8]) -> Result<PathBuf> {
    let path = crate::git::decode_url_bytes(encoded);
    anyhow::ensure!(!path.contains(&0), "file URL contains a NUL byte");
    Ok(PathBuf::from(OsString::from_vec(path)))
}

pub struct Remote {
    pub git: Arc<Git>,
    native_git_version: String,
    run: PathBuf,
    completed_fetches: u64,
    session_fetch_operations: u64,
    session_decode_operations: u64,
    session_starts: u64,
    session_received_pack_bytes: u64,
    eager_verified_pack_capacity: usize,
    forecast_seen: HashSet<Oid>,
    demand_pool: Option<crate::demand::Pool>,
    pub(crate) pack_verification: crate::demand::PackVerification,
    demand_url: OsString,
    socket: Option<PathBuf>,
    socket_owner: Option<SocketOwner>,
    ssh: Option<OsString>,
    ssh_storage: Option<Directory>,
    ssh_upload_pack_args: OnceLock<Vec<OsString>>,
    control_client: Option<ControlClient>,
    failed: OnceLock<String>,
    eager_pack_received: bool,
    pub(crate) raw_blobs: crate::raw_blob::RawBlobs,
}
impl Remote {
    pub fn open(cwd: &Path, source: &OsStr, revision: &OsStr, run: &Path) -> Result<(Self, Oid)> {
        let Source::Network(source) = resolve_source(cwd, source, false)? else {
            bail!("local repositories are loaded by Snapshot::load_source");
        };
        Self::open_for_execution(source, revision, run, false)
    }
    pub(crate) fn open_for_execution(
        source: NetworkSource,
        revision: &OsStr,
        run: &Path,
        materialized: bool,
    ) -> Result<(Self, Oid)> {
        let run = std::path::absolute(run).context("resolve private run directory")?;
        let run = run.as_path();
        let NetworkSource {
            git,
            source,
            url,
            ssh: ssh_transport,
        } = source;
        let native_git_version = require_native_git(&git.cwd)?;
        let protocol = if ssh_transport { "ssh" } else { "https" };
        anyhow::ensure!(
            git.transport_policy_is_default(protocol) && git.demand_session_compatible(&source)?,
            "Git configuration is outside the direct {protocol} protocol v2 profile"
        );
        if !ssh_transport {
            anyhow::ensure!(
                git.http2_compatible(&source, &url)?,
                "Git HTTP configuration is outside the direct HTTPS profile"
            );
        }
        let socket = ssh_transport.then(|| crate::platform::ssh_control_path(run));
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
            .map(|ssh| ControlClient::resolve(&git.cwd, ssh))
            .transpose()?;
        let ssh_storage = socket
            .as_ref()
            .map(|socket| -> Result<_> {
                let storage = Directory::new_for_run("ssh-", run)?;
                std::os::unix::fs::symlink(socket, storage.path().join("socket"))?;
                std::os::unix::fs::symlink(
                    std::env::current_exe()?,
                    storage.path().join(SSH_LAUNCHER),
                )?;
                std::os::unix::fs::symlink(
                    run.join(SSH_CONTROL_OWNER_FILE),
                    storage.path().join(SSH_CONTROL_OWNER_FILE),
                )?;
                Ok(storage)
            })
            .transpose()?;
        let raw_blobs = crate::raw_blob::RawBlobs::new(run)?;
        let pack_verification = crate::demand::PackVerification::new(&raw_blobs);
        let mut remote = Self {
            git: Arc::new(git),
            native_git_version,
            run: run.to_path_buf(),
            completed_fetches: 0,
            session_fetch_operations: 0,
            session_decode_operations: 0,
            session_starts: 0,
            session_received_pack_bytes: 0,
            eager_verified_pack_capacity: 0,
            forecast_seen: HashSet::new(),
            demand_pool: None,
            pack_verification,
            demand_url: url,
            socket,
            socket_owner: None,
            ssh,
            ssh_storage,
            ssh_upload_pack_args: OnceLock::new(),
            control_client,
            failed: OnceLock::new(),
            eager_pack_received: false,
            raw_blobs,
        };
        let oid = remote.select_network(revision, materialized)?;
        Ok((remote, oid))
    }

    fn select_network(&mut self, revision: &OsStr, materialized: bool) -> Result<Oid> {
        let revision = revision.to_str().context("remote revision must be UTF-8")?;
        let mut source = revision.strip_prefix('+').unwrap_or(revision);
        if let Some((name, destination)) = source.split_once(':') {
            anyhow::ensure!(
                destination.starts_with("refs/") && git2::Reference::is_valid_name(destination),
                "a remote refspec destination must be a full Git ref"
            );
            source = name;
        }
        let exact = exact_oid(OsStr::new(source));
        let reference = if exact.is_none() {
            Some(
                transport::RefQuery::for_network(OsStr::new(source))
                    .context("select one remote ref or a full SHA-1 object ID")?,
            )
        } else {
            None
        };
        let target = exact
            .map(SnapshotTarget::Object)
            .or_else(|| reference.as_ref().map(SnapshotTarget::Ref))
            .expect("parsed remote revision");
        if materialized {
            return self.fetch_snapshot(target);
        }
        if self.uses_http() {
            let selection =
                reference.map_or(crate::http2::Selection::Exact, crate::http2::Selection::Ref);
            let selected = self.open_http2_pool(selection)?;
            let pool = self
                .demand_pool
                .as_ref()
                .context("missing HTTP/2 object loader")?;
            let oid = selected
                .or(exact)
                .context("remote ref did not select one object")?;
            let batch = pool.request(oid, false)?.wait();
            self.finish_demand(&batch)?;
            return Ok(oid);
        }
        self.select_ssh(target)
    }

    pub(crate) fn verify_selection(&self, oid: Oid, kind: git2::ObjectType) -> Result<()> {
        crate::git::signature_verifier(kind)?;
        let ReceivedObject { object, .. } = self
            .raw_blobs
            .received(oid)
            .with_context(|| format!("missing verified Git metadata {oid}"))?;
        self.git.verify_streamed_object(&self.run, &object)
    }

    fn command(&self, initial: bool) -> Result<Command> {
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        let mut command = self.git.command(None);
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
            let directory = self.ssh_storage.as_ref().unwrap().path();
            command
                .env("GIT_ON_SSH_CONTROL_PATH", socket)
                .env("GIT_ON_SSH_CONFIG", directory.join("config"))
                .env("GIT_ON_SSH_LAUNCHER", directory.join(SSH_LAUNCHER))
                .env("GIT_ON_SSH_READONLY", SSH_READ_ONLY)
                .env("GIT_ON_SSH_INITIAL", if initial { "1" } else { "0" })
                .env("GIT_ON_SSH_ADDITIONAL", "0");
            command
                .env("GIT_SSH_COMMAND", text)
                .env("GIT_SSH_VARIANT", "ssh");
        }
        if !(initial || crate::terminal_ui::preparing() && crate::terminal_ui::owns_terminal()) {
            command
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "/bin/false")
                .env("SSH_ASKPASS", "/bin/false");
        }
        Ok(command)
    }

    pub(crate) fn fetch_copy_objects(&mut self, selected: Oid) -> Result<()> {
        self.fetch_snapshot(SnapshotTarget::Object(selected))?;
        Ok(())
    }

    fn close_lazy_channels_for_copy(&mut self) -> Result<()> {
        if let Some(pool) = &self.demand_pool {
            self.pack_verification.wait(true).and(pool.stop())?;
        }
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        Ok(())
    }

    pub(crate) fn has_received_object(&self, oid: Oid) -> bool {
        self.raw_blobs.has_received(oid)
    }
    pub(crate) fn copy_materialized_blob(
        &mut self,
        oid: Oid,
        output: &mut (dyn Write + Send),
        validate: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<u64> {
        let outcome = (|| {
            let ReceivedObject { object, transfer } = self
                .raw_blobs
                .received(oid)
                .context("missing materialization blob")?;
            anyhow::ensure!(
                object.kind() == git2::ObjectType::Blob,
                "materialized file is not a blob"
            );
            let data = object.data();
            validate(data)?;
            crate::object::write_verified_buffer(data, output)?;
            if let Some(transfer) = transfer {
                transfer.mark_served();
            }
            self.raw_blobs.forget_received(oid);
            Ok(data.len() as u64)
        })();
        self.record_result(outcome)
    }
    pub fn ensure(&mut self, oid: Oid) -> Result<()> {
        self.ensure_object(oid, false)
    }

    pub(crate) fn ensure_object(&mut self, oid: Oid, is_tree: bool) -> Result<()> {
        let outcome = (|| {
            if self.raw_blobs.claim(oid)? {
                return Ok(());
            }
            if let Some(ReceivedObject { object, transfer }) = self.raw_blobs.received(oid) {
                return self.publish_verified(object, transfer);
            }
            if let Some(error) = self.failure() {
                bail!("remote object transport previously failed: {error}");
            }
            if self.eager_pack_received {
                bail!("shallow snapshot pack omitted required object {oid}");
            }
            let batch = self
                .demand_pool
                .as_ref()
                .context("missing network object loader")?
                .request(oid, is_tree)?
                .wait();
            self.finish_demand(&batch)
        })();
        self.record_result(outcome)
    }

    pub(crate) fn ensure_batch(
        &mut self,
        objects: impl Iterator<Item = (Oid, bool)>,
    ) -> Result<()> {
        // An eager snapshot's complete pack is already validated.
        if self.eager_pack_received {
            return Ok(());
        }
        let outcome = (|| {
            let mut missing = objects
                .filter(|(oid, _)| !self.raw_blobs.available(*oid))
                .peekable();
            if missing.peek().is_none() {
                return Ok(());
            }
            let batch = self
                .demand_pool
                .as_ref()
                .context("missing network object loader")?
                .request_batch(missing)?;
            for demand in batch {
                self.finish_demand(&demand.wait())?;
            }
            Ok(())
        })();
        self.record_result(outcome)
    }

    fn uses_http(&self) -> bool {
        self.demand_url.as_bytes().starts_with(b"https://")
    }

    pub(crate) fn start_demand<I: Iterator<Item = Oid>>(
        &mut self,
        oid: Oid,
        is_tree: bool,
        candidates: Option<impl FnOnce(bool) -> I>,
    ) -> Result<Option<crate::demand::Demand>> {
        let outcome = (|| {
            if self.failure().is_some() {
                return Ok(None);
            }
            let wanted = !self.raw_blobs.available(oid);
            if !wanted && candidates.is_none() {
                return Ok(None);
            }
            let Some(pool) = &self.demand_pool else {
                return Ok(None);
            };
            let demand = wanted.then(|| pool.request(oid, is_tree)).transpose()?;
            if let Some(candidates) = candidates.filter(|_| self.forecast_seen.insert(oid)) {
                pool.prefetch(oid, candidates, |_| false);
            }
            Ok(demand)
        })();
        self.record_result(outcome)
    }

    fn install_pool(&mut self, pool: crate::demand::Pool) {
        self.pack_verification.observe(&pool);
        self.demand_pool = Some(pool);
    }

    fn open_http2_pool(&mut self, selection: crate::http2::Selection) -> Result<Option<Oid>> {
        let (pool, selected) = self.with_http2_configuration(|configuration, scratch| {
            crate::demand::Pool::http2(configuration, selection, scratch)
        })?;
        self.install_pool(pool);
        self.session_starts += 1;
        Ok(selected)
    }

    fn with_http2_configuration<T>(
        &self,
        open: impl FnOnce(crate::http2::Configuration, &crate::raw_blob::RawBlobs) -> Result<T>,
    ) -> Result<T> {
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        let git = self.git.clone();
        let credential = move |url: &str, challenges: &[Vec<u8>]| {
            crate::http2::credentials(git.command(None), url, challenges)
        };
        let configuration = crate::http2::Configuration::new(
            &self.demand_url,
            &self.native_git_version,
            credential,
        )
        .context("direct HTTPS needs a supported URL and libcurl with TLS and HTTP/2")?;
        open(configuration, &self.raw_blobs)
    }

    fn fetch_snapshot(&mut self, target: SnapshotTarget<'_>) -> Result<Oid> {
        let (oid, mut received) = if self.uses_http() {
            let selection = target
                .reference()
                .cloned()
                .map_or(crate::http2::Selection::Exact, crate::http2::Selection::Ref);
            let pack = self.with_http2_configuration(|configuration, scratch| {
                let (mut client, selected) = crate::http2::Client::negotiate(
                    configuration,
                    Default::default(),
                    selection,
                    transport::FetchMode::Snapshot,
                )?;
                let oid = target
                    .oid(selected)
                    .context("remote ref did not select one object")?;
                let response =
                    crate::pack_stream::Response::for_materialization(scratch.clone(), oid);
                let pack = client
                    .fetch_snapshot(oid, response)
                    .with_context(|| format!("fetch shallow HTTPS snapshot {oid}"))?;
                Ok((oid, pack))
            })?;
            self.session_starts += 1;
            pack
        } else {
            self.fetch_ssh_snapshot(target)?
        };
        received.cache_extras(&self.raw_blobs, None, |_| {});
        self.eager_pack_received = true;
        self.completed_fetches += 1;
        self.session_fetch_operations += 1;
        self.session_decode_operations += 1;
        self.session_received_pack_bytes += received.bytes;
        self.eager_verified_pack_capacity = received.eager_verified_capacity;
        Ok(oid)
    }

    fn fetch_ssh_snapshot(
        &mut self,
        target: SnapshotTarget<'_>,
    ) -> Result<(Oid, crate::pack_stream::Received)> {
        if let (Some(pool), SnapshotTarget::Object(oid)) = (&self.demand_pool, target) {
            // Reuse the selection channel for automatic mount-to-copy fallback.
            // A server can allow just one channel on the owned SSH connection.
            if let Some(pack) = pool.fetch_ssh_snapshot(oid)? {
                return Ok((oid, pack));
            }
            self.close_lazy_channels_for_copy()?;
        }
        let (session, oid, _) = self.open_ssh_selection(target, transport::FetchMode::Snapshot)?;
        let response =
            crate::pack_stream::Response::for_materialization(self.raw_blobs.clone(), oid);
        let received = session.fetch_snapshot(oid, response, "SSH")?;
        self.session_starts += 1;
        Ok((oid, received))
    }

    pub(crate) fn finish_demand(&mut self, batch: &crate::demand::Batch) -> Result<()> {
        let Some(acquired) = batch.take() else {
            if let Some(error) = self.failure() {
                bail!("remote object transport previously failed: {error}");
            }
            return Ok(());
        };
        let outcome = acquired.and_then(|acquired| match acquired {
            crate::demand::Acquired::Stored | crate::demand::Acquired::ValidatedExtra => {
                // An already received delta base can become known between
                // the snapshot's first cache lookup and queue admission.
                for &oid in &batch.oids {
                    if self.raw_blobs.get(oid)?.is_some() {
                        continue;
                    }
                    if let Some(ReceivedObject { object, transfer }) = self.raw_blobs.received(oid)
                    {
                        self.publish_verified(object, transfer)?;
                    }
                }
                Ok(())
            }
            crate::demand::Acquired::Object(object) => self.publish_verified(object, None),
        });
        self.demand_pool.as_ref().unwrap().retire(&batch.oids);
        self.record_result(outcome)
    }
    fn record_result<T>(&mut self, outcome: Result<T>) -> Result<T> {
        if let Err(error) = &outcome {
            // ODB failures are just as incomplete as a failed fetch. Keep
            // the original failure even when later demands retry that operation.
            if !crate::object::is_cancelled(error)
                && self.failed.get().is_none()
                && self.raw_blobs.failure().is_none()
            {
                let failure = self.failed.get_or_init(|| format!("{error:#}"));
                crate::git::write_live_error(format_args!(
                    "git-on: remote object loading failed: {failure}\n"
                ));
            }
        }
        outcome
    }
    fn ssh_socket(&self) -> Option<crate::transport::SshSocket> {
        let path = self.socket.clone()?;
        let owner = self.socket_owner.as_ref()?;
        Some(crate::transport::SshSocket {
            path,
            device: owner.device,
            inode: owner.inode,
        })
    }

    fn open_ssh_selection(
        &mut self,
        target: SnapshotTarget<'_>,
        mode: transport::FetchMode,
    ) -> Result<(transport::Session, Oid, Option<transport::StartingSession>)> {
        if let Some(socket) = self.ssh_socket() {
            socket.check()?;
        }
        let initial = self.socket_owner.is_none();
        let mut command = self
            .upload_pack_command(initial)?
            .context("missing OpenSSH upload-pack command")?;
        if initial {
            crate::terminal_ui::configure(&mut command);
        } else {
            command.env("SSH_ASKPASS_REQUIRE", "never");
        }
        let starting = transport::StartingSession::spawn(command, None)?;
        let selection = if initial {
            self.wait_for_ssh_advertisement(starting, mode)
        } else {
            starting.negotiate_for(mode).map(|session| (session, None))
        };
        self.record_socket_owner()?;
        crate::object::check_transfer()?;
        let (session, spare) = selection.map_err(|error| {
            let diagnostic = format!("{error:#}");
            if diagnostic.contains("Permission denied (publickey") {
                let https = self.demand_url.to_str().and_then(github_https_alternative);
                let advice = authentication_advice(&diagnostic, https.as_deref());
                error.context(format!("SSH authentication failed{advice}"))
            } else {
                error
            }
        })?;
        let mut session = session
            .context("SSH server cannot negotiate the required Git protocol v2 capabilities")?;
        self.ssh_socket()
            .context("OpenSSH did not create the owned control socket")?
            .check()?;
        let selected = match target.reference() {
            Some(reference) => session.select_ref(reference)?,
            None => None,
        };
        let oid = target.oid(selected).context(
            "remote ref did not select one object; use a full Git ref when names compete",
        )?;
        Ok((session, oid, spare))
    }

    fn wait_for_ssh_advertisement(
        &mut self,
        primary: transport::StartingSession,
        mode: transport::FetchMode,
    ) -> Result<(
        Option<transport::Session>,
        Option<transport::StartingSession>,
    )> {
        let started = Instant::now();
        let mut pending = vec![primary];
        let mut opened_spare = false;
        loop {
            if let Some(index) = transport::StartingSession::wait_for_advertisement(&pending, mode)?
            {
                let result = pending.swap_remove(index).negotiate_for(mode);
                if matches!(result, Ok(Some(_))) || pending.is_empty() {
                    return result.map(|session| (session, pending.pop()));
                }
            } else if !opened_spare && started.elapsed() >= SSH_WARM_DELAY {
                self.record_socket_owner()?;
                if let Some(socket) = self.ssh_socket() {
                    opened_spare = true;
                    let command = self
                        .upload_pack_command(false)?
                        .context("missing OpenSSH upload-pack command")?;
                    let connection = transport::Connection::new(command, Some(socket));
                    // A spare can only open a session on the owned master.
                    // If its session is rejected, the original remains active.
                    if let Ok(starting) = connection.start_additional(None) {
                        pending.push(starting);
                    }
                }
            }
        }
    }

    fn select_ssh(&mut self, target: SnapshotTarget<'_>) -> Result<Oid> {
        let started = std::time::Instant::now();
        let (session, oid, spare) =
            self.open_ssh_selection(target, transport::FetchMode::Filtered)?;
        let connection = transport::Connection::new(
            self.upload_pack_command(false)?
                .context("missing OpenSSH upload-pack command")?,
            self.ssh_socket(),
        );
        // Warm spare channels alongside a slow selection. A fast handshake
        // can still be followed by a slow pack, so measure that request too.
        let warm = (spare.is_some() || started.elapsed() >= SSH_WARM_DELAY)
            .then(|| crate::demand::Pool::prepare_native(&connection, &self.raw_blobs, true, spare))
            .transpose()?;
        let mut first = session;
        let fetching = Instant::now();
        self.fetch_initial_ssh(oid, &mut first)?;
        let mut pool = match warm {
            Some(pool) => pool,
            None => crate::demand::Pool::prepare_native(
                &connection,
                &self.raw_blobs,
                fetching.elapsed() >= crate::demand::SLOW_REPLY,
                None,
            )?,
        };
        self.session_starts += 1;
        pool.attach_primary(first, connection)?;
        self.install_pool(pool);
        Ok(oid)
    }

    fn upload_pack_command(&self, initial: bool) -> Result<Option<Command>> {
        let native = self.command(initial)?;
        let Some(mut command) = self.ssh_upload_pack(&native)? else {
            return Ok(None);
        };
        command.current_dir(&self.git.cwd);
        for (key, value) in native.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command.env("GIT_PROTOCOL", "version=2");
        Ok(Some(command))
    }

    fn ssh_upload_pack(&self, native: &Command) -> Result<Option<Command>> {
        let Some(ssh) = native
            .get_envs()
            .find_map(|(key, value)| (key == "GIT_SSH_COMMAND").then_some(value).flatten())
        else {
            return Ok(None);
        };
        let args = match self.ssh_upload_pack_args.get() {
            Some(args) => args,
            None => {
                let Some(args) = self.probe_ssh_upload_pack_args()? else {
                    return Ok(None);
                };
                self.ssh_upload_pack_args.get_or_init(|| args)
            }
        };
        let mut script = ssh.to_os_string();
        script.push(" \"$@\"");
        let mut command = Command::new("/bin/sh");
        command.args([OsStr::new("-c"), &script, OsStr::new("git-on-ssh")]);
        command.args(args);
        Ok(Some(command))
    }

    fn probe_ssh_upload_pack_args(&self) -> Result<Option<Vec<OsString>>> {
        // Ask Git for the actual OpenSSH argv for the already resolved URL.
        // Use an empty Git configuration so it applies URL rewrites only
        // during the earlier resolution. A shell stub replaces SSH here.
        // The result can be reused when the first channel joins the pool.
        const MARKER: &[u8] = b"\0git-on-ssh-arguments\0";
        let mut probe = crate::git::native_git_command();
        probe
            .current_dir(&self.git.cwd)
            .args(["--git-dir=/dev/null", "-c", "protocol.version=2", "ls-remote", "--"])
            .arg(&self.demand_url)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_COMMON_DIR")
            .env("GIT_ALLOW_PROTOCOL", "ssh")
            .env("GIT_SSH_VARIANT", "ssh")
            .env(
                "GIT_SSH_COMMAND",
                r#"f() { printf '\000git-on-ssh-arguments\000%s\000' "$#" >&2; printf '%s\000' "$@" >&2; printf '\000' >&2; }; f"#,
            )
            .stdin(Stdio::null());
        let output = crate::git::run_builtin(probe)?;
        // No SSH protocol stream is produced by the stub. Git reports the
        // closed transport after the local argv frame has been written.
        let Some(start) = output
            .stderr
            .windows(MARKER.len())
            .position(|bytes| bytes == MARKER)
        else {
            return Ok(None);
        };
        let mut fields = output.stderr[start + MARKER.len()..].split(|byte| *byte == 0);
        let Some(count) = fields
            .next()
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<usize>().ok())
        else {
            return Ok(None);
        };
        let args: Vec<_> = fields
            .by_ref()
            .take(count)
            .map(|field| OsStr::from_bytes(field).to_os_string())
            .collect();
        if args.len() != count || count < 2 || fields.next() != Some(&[][..]) {
            return Ok(None);
        }
        Ok(Some(args))
    }
    fn fetch_initial_ssh(&mut self, oid: Oid, session: &mut transport::Session) -> Result<()> {
        // Decode directly from the reusable channel. Hold the object locally
        // until the reply is complete, including the pack's own checksum.
        let mut object = None;
        let response = crate::pack_stream::Response::new(
            self.raw_blobs.clone(),
            vec![oid],
            Box::new(|received| {
                object = Some(received);
                Ok(())
            }),
        );
        let mut received =
            session.fetch_stream(&[oid], transport::FetchMode::Filtered, response)?;
        self.session_fetch_operations += 1;
        self.session_received_pack_bytes = self
            .session_received_pack_bytes
            .saturating_add(received.bytes);
        let object = object.context("stream did not publish requested Git object")?;
        self.publish_verified(object, None)?;
        received.cache_extras(&self.raw_blobs, None, |_| {});
        self.session_decode_operations += 1;
        self.completed_fetches += 1;
        Ok(())
    }

    fn publish_verified(
        &mut self,
        object: crate::pack_stream::VerifiedObject,
        transfer: Option<Arc<Transfer>>,
    ) -> Result<()> {
        let kind = object.kind();
        if kind == git2::ObjectType::Blob {
            return self
                .raw_blobs
                .insert_verified_blob(object, transfer, false, || false, |_| {});
        }
        if kind == git2::ObjectType::Tree {
            self.raw_blobs.insert_verified_tree(&object, transfer)?;
            return Ok(());
        }
        self.raw_blobs
            .remember_verified([object], transfer.as_ref());
        Ok(())
    }
    /// Release the owned master even when the run directory must remain for recovery.
    pub fn stop_transport(&self) -> Result<()> {
        if let Some(pool) = &self.demand_pool {
            pool.stop()?;
        }
        if let (Some(socket), Some(owner)) = (&self.socket, &self.socket_owner) {
            stop_owned_connection(socket, owner)?;
        }
        Ok(())
    }
    fn record_socket_owner(&mut self) -> Result<()> {
        let Some(socket) = &self.socket else {
            return Ok(());
        };
        if let Some(owner) = &self.socket_owner {
            anyhow::ensure!(
                owned_socket(socket, owner),
                "owned SSH control connection changed during preparation"
            );
            return Ok(());
        }
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
        let record = serde_json::to_vec(&owner)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.run.join(SSH_CONTROL_OWNER_FILE))
            .context("record owned SSH control socket")?;
        file.write_all(&record)?;
        Ok(())
    }
    pub fn failure(&self) -> Option<String> {
        self.failed
            .get()
            .cloned()
            .or_else(|| self.raw_blobs.failure())
            .or_else(|| {
                self.demand_pool
                    .as_ref()
                    .and_then(crate::demand::Pool::failure)
            })
    }
    fn pool_metrics(&self) -> crate::demand::Metrics {
        self.demand_pool
            .as_ref()
            .map_or_else(Default::default, crate::demand::Pool::metrics)
    }
    pub(crate) fn completed_fetches(&self) -> u64 {
        self.completed_fetches + self.pool_metrics().oids
    }
    pub(crate) fn response_metrics(&self) -> (u64, u64, u64) {
        let pool = self.pool_metrics();
        (
            pool.streamed_packs + u64::from(self.eager_pack_received),
            pool.prefetches,
            pool.prefetched_blobs,
        )
    }
    pub(crate) fn session_operations(&self) -> (u64, u64, u64) {
        let pool = self.pool_metrics();
        (
            self.session_fetch_operations + pool.packs,
            self.session_decode_operations + pool.streamed_packs,
            self.session_starts
                + self
                    .demand_pool
                    .as_ref()
                    .map_or(0, crate::demand::Pool::additional_starts),
        )
    }
    pub(crate) fn received_pack_bytes(&self) -> u64 {
        self.session_received_pack_bytes + self.pool_metrics().bytes
    }
    pub(crate) fn eager_verified_pack_capacity(&self) -> usize {
        self.eager_verified_pack_capacity
    }
    pub(crate) fn http2_metrics(&self) -> (u64, u64) {
        let (fetches, peak) = self
            .demand_pool
            .as_ref()
            .map_or((0, 0), crate::demand::Pool::http2_metrics);
        if self.eager_pack_received && self.uses_http() {
            return (fetches + 1, peak.max(1));
        }
        (fetches, peak)
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if let Err(error) = self.stop_transport() {
            if let Some(storage) = &mut self.ssh_storage {
                storage.keep_for_owner();
            }
            crate::git::write_run_error(format_args!(
                "git-on: owned remote transport shutdown failed: {error:#}\n"
            ));
        }
    }
}
/// Stop only the control connection in a validated run's private directory.
/// Recovery callers must validate and lock that run before invoking this.
pub fn stop_connection(socket: &Path) -> Result<()> {
    let mut record = socket.parent().unwrap().join(SSH_CONTROL_OWNER_FILE);
    if let Ok(target) = fs::read_link(&record) {
        if !target.is_absolute()
            || target.file_name() != Some(OsStr::new(SSH_CONTROL_OWNER_FILE))
            || !fs::read_link(target.with_file_name("socket"))
                .is_ok_and(|owned| same_socket_path(&owned, socket))
        {
            return Ok(());
        }
        record = target;
    }
    stop_connection_with_record(socket, &record)
}

fn same_socket_path(left: &Path, right: &Path) -> bool {
    left.file_name() == right.file_name()
        && left
            .parent()
            .zip(right.parent())
            .is_some_and(|(left, right)| {
                left.canonicalize()
                    .is_ok_and(|path| right.canonicalize().is_ok_and(|other| other == path))
            })
}

pub(crate) fn has_ssh_connection(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let path = entry.path();
            entry.file_name().as_bytes().starts_with(b"ssh-")
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && path.join(SSH_CONTROL_OWNER_FILE).is_file()
                && fs::read_link(path.join("socket")).is_ok_and(|socket| {
                    fs::symlink_metadata(socket).is_ok_and(|info| info.file_type().is_socket())
                })
        })
    })
}

fn stop_connection_with_record(socket: &Path, record: &Path) -> Result<()> {
    let Some(value) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(record)
        .ok()
        .filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
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
    #[cfg(target_os = "linux")]
    let socket_flags = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    #[cfg(target_os = "macos")]
    let socket_flags = libc::SOCK_STREAM;
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, socket_flags, 0) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("inspect owned SSH control endpoint");
    }
    let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
    #[cfg(target_os = "macos")]
    {
        stream.set_nonblocking(true)?;
        if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error()).context("protect SSH endpoint descriptor");
        }
    }
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
            crate::git::run_builtin(command).context("resolve selected SSH shutdown client")?;
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
const SSH_WRAPPER: &str = r#"git_on_ssh_config() {
    git_on_config=
    if [ "$git_on_explicit_config" = 0 ]; then return; fi
    git_on_config=$GIT_ON_SSH_CONFIG
    if [ -f "$git_on_config" ]; then return; fi
    git_on_pending=$(/usr/bin/mktemp "$git_on_config.XXXXXX") || return 125
    {
        printf '%s\n' 'Host *' $GIT_ON_SSH_READONLY \
            'ControlMaster no' 'ControlPath none' 'ControlPersist no'
        if [ "$git_on_source_config" != none ]; then
            /bin/cat -- "$git_on_source_config" || return 125
        fi
    } > "$git_on_pending" || return 125
    /bin/mv -f "$git_on_pending" "$git_on_config"
}
git_on_ssh_command() {
    executable=$1; shift
    case "$executable" in
        env|exec|command|sh|bash|dash|zsh|ksh|*=*)
            echo 'git-on: SSH command must name an OpenSSH executable or an argv-forwarding wrapper directly; shell prefixes are unsupported' >&2
            exit 125 ;;
    esac
    remaining=$(($# - git_on_git_argc)); skip=0
    git_on_explicit_config=0; git_on_next_config=0
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1)); drop=0
        if [ "$git_on_next_config" = 1 ]; then
            git_on_source_config=$argument; git_on_next_config=0; drop=1
        elif [ "$skip" = 1 ]; then
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
                            M|A|X|Y) ;;
                            O)
                                echo 'git-on: SSH control commands (-O) are owned by the runner' >&2
                                exit 125 ;;
                            E|y)
                                echo 'git-on: SSH log destinations are limited to the diagnostic stream' >&2
                                exit 125 ;;
                            w)
                                echo 'git-on: SSH tunnel devices are unsupported' >&2
                                exit 125 ;;
                            F)
                                git_on_explicit_config=1
                                if [ -z "$flags" ]; then git_on_next_config=1
                                else git_on_source_config=$flags; fi
                                flags= ;;
                            B|b|c|D|e|I|i|J|L|l|m|o|p|P|Q|R|S|W)
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
    if [ "$skip" = 1 ] || [ "$git_on_next_config" = 1 ]; then
        echo 'git-on: SSH option is missing its argument' >&2
        exit 125
    fi
    git_on_ssh_config || exit $?
    if [ -n "$git_on_config" ]; then
        set -- "$@" -F "$git_on_config"
    elif [ "$git_on_explicit_config" = 1 ]; then
        set -- "$@" -F "$git_on_source_config"
    fi
    set -- "$@" -S "$GIT_ON_SSH_CONTROL_PATH"
    if [ "$GIT_ON_SSH_INITIAL" != 1 ] && [ "$GIT_ON_SSH_ADDITIONAL" != 1 ]; then
        # Require the existing SSH master for this channel.
        set -- "$@" -O proxy
    fi
    remaining=$git_on_git_argc
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1))
        set -- "$@" "$argument"
    done
    for git_on_option in $GIT_ON_SSH_READONLY; do set -- -o "$git_on_option" "$@"; done
    if [ "$GIT_ON_SSH_INITIAL" = 1 ]; then
        set -- -o ControlMaster=auto -o ControlPersist=yes "$@"
    elif [ "$GIT_ON_SSH_ADDITIONAL" = 1 ]; then
        # Ask the master to open the session. A rejected channel in raw mux
        # proxy mode can disconnect the master. The false proxy forbids
        # a replacement connection if ordinary session multiplexing fails.
        set -- -o ControlMaster=no -o BatchMode=yes -o ProxyCommand=/usr/bin/false "$@"
    else
        set -- -o ControlMaster=no -o BatchMode=yes "$@"
    fi
    if [ "$GIT_ON_SSH_INITIAL" = 1 ]; then
        exec "$GIT_ON_SSH_LAUNCHER" --launch "$executable" "$git_on_git_argc" "$@"
    fi
    exec "$executable" "$@"
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn mux_shutdown_rejects_invalid_greetings_before_termination() {
        for greeting in [
            vec![0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 5],
            vec![0, 1, 0, 1],
            vec![0, 0, 0, 9, 0, 0, 0, 1, 0, 0, 0, 4, 0],
        ] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let peer = std::thread::spawn(move || {
                let mut hello = [0; 12];
                server.read_exact(&mut hello).unwrap();
                server.write_all(&greeting).unwrap();
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
}
