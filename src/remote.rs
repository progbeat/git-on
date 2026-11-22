//! Explicit exact-object fetches. Git's promisor auto-fetch is never the loader.
use crate::{
    git::Git,
    raw_blob::{ReceivedObject, Transfer},
    transport,
};
use anyhow::{bail, Context, Result};
use git2::{Odb, Oid, Repository};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, Instant},
};

pub const SSH_CONTROL_OWNER_FILE: &str = "ssh-control-owner";
const SSH_STANDBY_DELAY: Duration = Duration::from_millis(250);
const SSH_STANDBY_CHANNELS: usize = 2;

fn require_native_git(cwd: &Path) -> Result<String> {
    let mut command = Command::new("git");
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

struct ReceivedPack {
    // Release ODB/index descriptors before deleting the owned directory.
    repo: Repository,
    directory: Arc<tempfile::TempDir>,
}
enum PrivateRepository {
    Context(Repository),
    Objects(Repository),
}
enum SnapshotTransport {
    Https { http2: bool },
    Ssh,
    File,
}

enum SnapshotPreparation {
    Ready(Oid),
    Native(Option<Oid>),
}

#[derive(Clone, Copy)]
enum NativeRevision<'a> {
    Query(&'a OsStr),
    Selected(Oid),
}

#[derive(Clone, Copy)]
enum SnapshotTarget<'a> {
    Object(Oid),
    Branch(&'a transport::BranchQuery),
}

impl<'a> SnapshotTarget<'a> {
    fn branch(self) -> Option<&'a transport::BranchQuery> {
        match self {
            Self::Object(_) => None,
            Self::Branch(query) => Some(query),
        }
    }

    fn oid(self, selected: Option<Oid>) -> Option<Oid> {
        match self {
            Self::Object(oid) => Some(oid),
            Self::Branch(_) => selected,
        }
    }
}

pub struct Remote {
    pub git: Arc<Git>,
    native_git_version: String,
    pub dir: PathBuf,
    repo: Option<PrivateRepository>,
    credential_repository: Option<mpsc::Receiver<Result<Repository>>>,
    source: OsString,
    https_alternative: Option<String>,
    completed_fetches: u64,
    fetch_operations: u64,
    decode_operations: u64,
    session_fetch_operations: u64,
    session_decode_operations: u64,
    session_starts: u64,
    native_received_pack_bytes: u64,
    session_received_pack_bytes: u64,
    eager_verified_pack_capacity: usize,
    session: Option<crate::transport::Session>,
    session_checked: bool,
    http2_checked: bool,
    pool_checked: bool,
    forecast_seen: HashSet<Oid>,
    demand_pool: Option<crate::demand::Pool>,
    pub(crate) pack_verification: crate::demand::PackVerification,
    demand_url: OsString,
    socket: Option<PathBuf>,
    socket_owner: Option<SocketOwner>,
    ssh: Option<OsString>,
    ssh_upload_pack_args: OnceLock<Vec<OsString>>,
    local_git_environment: OnceLock<Vec<u8>>,
    control_client: Option<ControlClient>,
    failed: OnceLock<String>,
    materializing: bool,
    snapshot_transport: Option<SnapshotTransport>,
    active_materialized_pack: Option<ReceivedPack>,
    pub(crate) raw_blobs: crate::raw_blob::RawBlobs,
}
impl Remote {
    pub fn open(cwd: &Path, source: &OsStr, revision: &OsStr, run: &Path) -> Result<(Self, Oid)> {
        Self::open_for_execution(cwd, source, revision, run, false, false)
    }
    pub(crate) fn open_for_execution(
        cwd: &Path,
        source: &OsStr,
        revision: &OsStr,
        run: &Path,
        materialized: bool,
        verify: bool,
    ) -> Result<(Self, Oid)> {
        // Capture effective configuration first: even `git --version` can read
        // it. Preserve the first native operation's cancellation/TTY lifecycle.
        // Config includes still use the caller's context. Any repository
        // we pass to Git for this remote belongs to the new run.
        let git = Git::capture_configuration(cwd).map_err(|error| {
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
        let native_git_version = require_native_git(cwd)?;
        let multi_remote = git.has_multiple_group_members(source);
        // Git owns repository spelling, named remotes and URL rewriting. This
        // native query is only used to configure the optional owned SSH master;
        // every network operation still receives the original source operand.
        let mut url = if multi_remote {
            Vec::new()
        } else {
            git.output(
                None,
                &[
                    OsStr::new("ls-remote"),
                    OsStr::new("--get-url"),
                    OsStr::new("--"),
                    source,
                ],
            )?
        };
        if url.last() == Some(&b'\n') {
            url.pop();
        }
        let text = String::from_utf8_lossy(&url);
        let ssh_transport = ["ssh://", "git+ssh://", "ssh+git://"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
            || (!text.contains("://")
                && !text.contains("::")
                && text
                    .find(':')
                    .is_some_and(|colon| text.find('/').is_none_or(|slash| colon < slash)));
        let https_alternative = github_https_alternative(&text);
        let dir = run.join("remote-repository");
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
            .map(|ssh| ControlClient::resolve(cwd, ssh))
            .transpose()?;
        let raw_blobs = crate::raw_blob::RawBlobs::new(run)?;
        let pack_verification = crate::demand::PackVerification::new(&raw_blobs);
        let mut remote = Self {
            git: Arc::new(git),
            native_git_version,
            dir,
            repo: None,
            credential_repository: None,
            source: source.to_os_string(),
            https_alternative,
            completed_fetches: 0,
            fetch_operations: 0,
            decode_operations: 0,
            session_fetch_operations: 0,
            session_decode_operations: 0,
            session_starts: 0,
            native_received_pack_bytes: 0,
            session_received_pack_bytes: 0,
            eager_verified_pack_capacity: 0,
            session: None,
            session_checked: false,
            http2_checked: false,
            pool_checked: false,
            forecast_seen: HashSet::new(),
            demand_pool: None,
            pack_verification,
            demand_url: OsString::from_vec(url),
            socket,
            socket_owner: None,
            ssh,
            ssh_upload_pack_args: OnceLock::new(),
            local_git_environment: OnceLock::new(),
            control_client,
            failed: OnceLock::new(),
            // Eager execution needs the whole selected tree in one transfer.
            materializing: materialized,
            snapshot_transport: None,
            active_materialized_pack: None,
            raw_blobs,
        };
        if multi_remote {
            // A remote group has no single advertisement. Native Git handles
            // the explicit revision before opening any member's transport.
            let oid = remote.fetch_selected(NativeRevision::Query(revision), true, false)?;
            return Ok((remote, oid));
        }
        // Matching fetch mappings and unsupported settings require native
        // fetch before any direct transport can select the revision.
        let native_selection = remote.git.fetch_settings_require_native_git(materialized)
            || remote.git.has_fetch_mapping(source)?;
        let exact = (!native_selection
            && revision.as_bytes().len() == 40
            && revision.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .then(|| {
            revision
                .to_str()
                .and_then(|value| Oid::from_str(value).ok())
        })
        .flatten();
        // On Darwin the private repository determines how native Git matches
        // non-ASCII refs.
        let select_ref_directly = !native_selection
            && exact.is_none()
            && (!cfg!(target_os = "macos") || revision.as_bytes().is_ascii());
        let branch = select_ref_directly
            .then(|| transport::BranchQuery::for_snapshot(revision))
            .flatten();
        let stream_target = exact
            .map(SnapshotTarget::Object)
            .or_else(|| branch.as_ref().map(SnapshotTarget::Branch))
            .filter(|_| !verify);
        let mut selected_for_native = None;
        if remote.materializing {
            // Native fetch owns refspec selection and configured ref updates.
            // Plain refs can be selected directly when the server advertises
            // one unambiguous match.
            if let Some(target) = stream_target {
                match remote.fetch_snapshot(target)? {
                    SnapshotPreparation::Ready(oid) => return Ok((remote, oid)),
                    SnapshotPreparation::Native(selected) => selected_for_native = selected,
                }
            }
            let revision = selected_for_native
                .map_or(NativeRevision::Query(revision), NativeRevision::Selected);
            let oid = remote.fetch_selected(revision, remote.socket_owner.is_none(), false)?;
            return Ok((remote, oid));
        }
        if verify {
            // Native signature verification takes an object name in a
            // Git repository and uses the caller's configured trust policy.
            remote.prepare_native_repository()?;
        }
        // Select an exact ID or unambiguous branch on the same channel used
        // for mounting and later demands. Other refspecs use native Git.
        let branch = branch.as_ref().filter(|_| revision != "HEAD");
        if exact.is_some() || branch.is_some() {
            let selection = branch.cloned().map_or(
                crate::http2::Selection::Exact,
                crate::http2::Selection::Branch,
            );
            let selected = remote.open_http2_pool(selection)?.or(exact);
            if let (Some(oid), Some(pool)) = (selected, &remote.demand_pool) {
                remote.http2_checked = true;
                let batch = pool.request(oid)?.wait();
                if !batch.requires_native_preparation() {
                    remote.pool_checked = true;
                    remote.finish_demand(&batch)?;
                    return Ok((remote, oid));
                }
                // The first fetch produced no objects. Native Git can
                // handle another HTTP version or its own authentication.
                drop(remote.demand_pool.take());
                selected_for_native = Some(oid);
            }
            if let Some(oid) = remote.select_ssh(exact, branch)? {
                return Ok((remote, oid));
            }
        }
        if let Some(target) = stream_target {
            if let Some(oid) = remote.select_file(target)? {
                return Ok((remote, oid));
            }
        }
        remote.prepare_native_repository()?;
        // Authentication and host-key interaction happen here, before mounting.
        let mut command = remote.command(remote.socket_owner.is_none())?;
        command
            .args(["-c", "protocol.version=2"])
            .arg("ls-remote")
            .arg("--symref")
            .arg("--")
            .arg(&remote.source)
            .stdin(Stdio::inherit());
        let outcome =
            crate::git::run_ref_advertisement(command, run).context("advertise remote refs");
        // Record an established master even if upload-pack or cancellation then
        // fails. The in-memory identity protects Drop; the small private record
        // lets validated stale cleanup identify the same socket after SIGKILL.
        remote.record_socket_owner()?;
        let (out, filtering) = outcome?;
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
        if !filtering {
            bail!("remote server does not advertise object filtering; full-clone fallback is forbidden");
        }
        let _ = remote.failed.take();
        if let Some(oid) = exact.filter(|_| remote.socket_owner.is_some()) {
            remote.session = remote.start_session(false)?;
            if remote.session.is_some() {
                remote.session_checked = true;
                if remote.fetch_in_session(oid)? {
                    return Ok((remote, oid));
                }
            }
        }
        let revision =
            selected_for_native.map_or(NativeRevision::Query(revision), NativeRevision::Selected);
        let oid = remote.fetch_selected(revision, false, false)?;
        Ok((remote, oid))
    }
    pub(crate) fn native_repository(&self) -> Option<&Repository> {
        match &self.repo {
            Some(PrivateRepository::Objects(repo)) => Some(repo),
            _ => None,
        }
    }
    fn odb(&self) -> Result<Option<Odb<'_>>> {
        Ok(self.native_repository().map(Repository::odb).transpose()?)
    }
    fn prepare_native_repository(&mut self) -> Result<()> {
        self.receive_credential_repository()?;
        let repo = match self.repo.take() {
            Some(PrivateRepository::Context(repo) | PrivateRepository::Objects(repo)) => repo,
            None => Self::create_native_repository(&self.dir)?,
        };
        self.repo = Some(PrivateRepository::Objects(repo));
        Ok(())
    }

    fn receive_credential_repository(&mut self) -> Result<()> {
        let Some(repository) = &self.credential_repository else {
            return Ok(());
        };
        match repository.try_recv() {
            Ok(repo) => {
                self.credential_repository = None;
                self.repo = Some(PrivateRepository::Context(repo?));
            }
            Err(mpsc::TryRecvError::Disconnected) => self.credential_repository = None,
            Err(mpsc::TryRecvError::Empty) => (),
        }
        Ok(())
    }
    fn create_native_repository(dir: &Path) -> Result<Repository> {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .context("create private Git repository directory")?;
        let repo = Repository::init_bare(dir)?;
        // A source path can be a valid fetch operand but an invalid
        // promisor remote name. This separate promisor has no URL.
        repo.config()?.set_bool(
            &format!("remote.git-on-owned-{}.promisor", uuid::Uuid::new_v4()),
            true,
        )?;
        Ok(repo)
    }
    fn native_command(git: &Git, dir: &Path) -> Command {
        let mut command = git.command(Some(dir));
        command.args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "fetch.writeCommitGraph=false",
            // These objects belong to one disposable invocation. A machine
            // crash ends that invocation; stale cleanup never resumes it.
            // Durability barriers add no OID-integrity or signature guarantee.
            "-c",
            "core.fsync=none",
            "-c",
            "core.bigFileThreshold=8m",
            // A fetched destination ref names a commit, but its trees and
            // blobs may still be absent. Advertising that commit as a 'have'
            // can make the server omit an explicitly demanded descendant.
            "-c",
            "fetch.negotiationAlgorithm=noop",
        ]);
        command
    }
    fn command(&self, initial: bool) -> Result<Command> {
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        let mut command = Self::native_command(&self.git, &self.dir);
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
    fn fetch_selected(
        &mut self,
        revision: NativeRevision<'_>,
        initial: bool,
        refetch: bool,
    ) -> Result<Oid> {
        // A fetch refspec belongs to native Git, including its leading '+',
        // namespace abbreviation and destination. All resulting refs and
        // FETCH_HEAD live in this owned bare repository, never in the caller.
        self.prepare_native_repository()?;
        let mut command = self.command(initial)?;
        command.args(["fetch", "--no-tags", "--depth=1"]);
        command.args(refetch.then_some("--refetch"));
        let materializing = self.materializing || refetch;
        if materializing {
            // Prevent small eager transfers from becoming a second decoded
            // loose-object copy before final-path materialization.
            command.arg("--keep");
        }
        command.arg(if materializing {
            "--no-filter"
        } else {
            "--filter=tree:0"
        });
        crate::terminal_ui::configure_fetch(&mut command);
        let oid_argument;
        let argument = match revision {
            NativeRevision::Query(query) => query,
            NativeRevision::Selected(oid) => {
                oid_argument = oid.to_string();
                OsStr::new(&oid_argument)
            }
        };
        command
            .arg("--")
            .arg(&self.source)
            .arg(argument)
            .stdin(Stdio::null());
        let outcome =
            crate::git::run_fetch(command, None).context("fetch selected remote revision");
        if initial {
            self.record_socket_owner()?;
        }
        let out = outcome?;
        let error = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            bail!(
                "native Git fetch failed: {}{}",
                error.trim(),
                authentication_advice(&error, self.https_alternative.as_deref())
            );
        }
        if initial && self.socket.is_some() && self.socket_owner.is_none() {
            bail!("OpenSSH did not create the owned control socket");
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
        if let NativeRevision::Selected(expected) = revision {
            anyhow::ensure!(oid == expected, "remote changed the selected snapshot");
        }
        if materializing {
            self.stage_materialization()?;
        } else {
            self.import_received(&[oid])?;
        }
        if !self.native_repository().unwrap().odb()?.exists(oid)
            && self.raw_blobs.get(oid)?.is_none()
        {
            bail!("native Git fetch did not supply selected object {oid}");
        }
        self.completed_fetches += 1;
        self.fetch_operations += 1;
        Ok(oid)
    }
    pub(crate) fn fetch_copy_objects(&mut self, selected: Oid) -> Result<()> {
        self.materializing = true;
        // The revision has already been resolved and possibly verified.
        // Request that exact object again without the lazy fetch filter.
        if !self.git.fetch_settings_require_native_git(true)
            && !self.git.has_fetch_mapping(&self.source)?
            && matches!(
                self.fetch_snapshot(SnapshotTarget::Object(selected))?,
                SnapshotPreparation::Ready(_)
            )
        {
            return Ok(());
        }
        self.close_lazy_channels_for_copy()?;
        self.fetch_selected(NativeRevision::Selected(selected), false, true)?;
        Ok(())
    }
    fn close_lazy_channels_for_copy(&mut self) -> Result<()> {
        // A server can allow only one upload-pack on the owned SSH master.
        // Release the lazy channels before starting a separate fetch.
        if let Some(session) = self.session.take() {
            if self.socket.is_some() {
                session.confirm_ssh_exit().and(session.stop())?;
            } else {
                session.stop()?;
            }
        }
        if let Some(pool) = &self.demand_pool {
            self.pack_verification.wait().and(pool.stop())?;
        }
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        Ok(())
    }

    pub(crate) fn begin_materialization(&mut self) {
        self.materializing = true;
        self.session = None;
    }
    pub(crate) fn has_received_object(&self, oid: Oid) -> bool {
        self.raw_blobs.has_received(oid)
    }
    pub(crate) fn copy_materialized_blob(
        &mut self,
        oid: Oid,
        output: &mut (dyn Write + Send),
        validate: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<(u64, bool)> {
        let outcome = (|| {
            let directory = match self
                .raw_blobs
                .received(oid)
                .context("missing materialization blob")?
            {
                ReceivedObject::Verified { object, transfer } => {
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
                    return Ok((data.len() as u64, false));
                }
                ReceivedObject::Packed(directory) => directory,
            };
            if self
                .active_materialized_pack
                .as_ref()
                .is_none_or(|pack| !Arc::ptr_eq(&pack.directory, &directory))
            {
                self.active_materialized_pack = None;
                self.active_materialized_pack = Some(ReceivedPack {
                    repo: Repository::open_bare(directory.path())?,
                    directory: directory.clone(),
                });
            }
            drop(directory);
            let pack = self.active_materialized_pack.as_ref().unwrap();
            let odb = pack.repo.odb()?;
            let (size, kind) = odb.read_header(oid)?;
            anyhow::ensure!(
                kind == git2::ObjectType::Blob,
                "materialized file is not a blob"
            );
            let streamed = size > crate::raw_blob::BLOCK;
            if streamed {
                let mut checked = crate::object::VerifiedWriter::new(output, kind, size, oid);
                let mut command = self.git.command(Some(pack.directory.path()));
                command
                    .args(["-c", "core.bigFileThreshold=8m", "cat-file", "blob"])
                    .arg(oid.to_string());
                let result = crate::git::run_to_writer(command, &mut checked, None)?;
                self.decode_operations += 1;
                anyhow::ensure!(
                    result.status.success(),
                    "cannot decode received Git blob: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                checked.finish()?;
            } else {
                let object = odb.read(oid)?;
                anyhow::ensure!(
                    object.id() == oid && object.len() == size && object.kind() == kind,
                    "Git blob changed during transfer"
                );
                crate::object::copy_odb_blob(&object, output, validate)?;
            }
            Ok((size as u64, streamed))
        })();
        if outcome.is_ok() {
            self.raw_blobs.forget_received(oid);
            if self
                .active_materialized_pack
                .as_ref()
                .is_some_and(|pack| Arc::strong_count(&pack.directory) == 1)
            {
                self.active_materialized_pack = None;
            }
        }
        self.record_result(outcome)
    }
    pub fn ensure(&mut self, oid: Oid) -> Result<()> {
        let outcome = (|| {
            if self.raw_blobs.claim(oid)? || self.odb()?.is_some_and(|odb| odb.exists(oid)) {
                return Ok(());
            }
            match self.raw_blobs.received(oid) {
                Some(ReceivedObject::Verified { object, transfer }) => {
                    self.publish_verified(object, transfer)
                }
                Some(ReceivedObject::Packed(directory)) if !self.materializing => {
                    self.import_packed_object(oid, directory.path())
                }
                Some(ReceivedObject::Packed(_)) => Ok(()),
                None => self.fetch(&[oid]),
            }
        })();
        self.record_result(outcome)
    }
    /// Batch only object IDs needed for known eager destinations.
    pub(crate) fn ensure_batch(&mut self, oids: impl Iterator<Item = Oid>) -> Result<()> {
        if self.snapshot_transport.is_some() {
            // The complete pack is already validated. File and tree reads
            // check their required OIDs while materializing their destinations.
            return Ok(());
        }
        let outcome = (|| {
            let missing = self.missing_demands(oids)?;
            if missing.is_empty() {
                return Ok(());
            }
            self.fetch(&missing)
        })();
        self.record_result(outcome)
    }
    fn missing_demands(&self, oids: impl Iterator<Item = Oid>) -> Result<Vec<Oid>> {
        let odb = self.odb()?;
        let mut unique = HashSet::new();
        let mut missing = Vec::new();
        for oid in oids {
            if unique.insert(oid)
                && !self.raw_blobs.available(oid)
                && !odb.as_ref().is_some_and(|odb| odb.exists(oid))
            {
                missing.push(oid);
            }
        }
        Ok(missing)
    }
    pub(crate) fn uses_http(&self) -> bool {
        let url = self.demand_url.as_bytes();
        url.starts_with(b"https://") || url.starts_with(b"http://")
    }
    pub(crate) fn reusable_transport(&self) -> bool {
        self.uses_http() || self.socket.is_some()
    }

    pub(crate) fn start_demand<I: Iterator<Item = Oid>>(
        &mut self,
        oid: Oid,
        immediate: bool,
        candidates: Option<impl FnOnce(bool) -> I>,
    ) -> Result<Option<crate::demand::Demand>> {
        let outcome = (|| {
            if !self.reusable_transport() || self.failure().is_some() {
                return Ok(None);
            }
            let wanted =
                !self.raw_blobs.available(oid) && !self.odb()?.is_some_and(|odb| odb.exists(oid));
            if !wanted && candidates.is_none() {
                return Ok(None);
            }
            self.prepare_pool()?;
            let Some(pool) = &self.demand_pool else {
                return Ok(None);
            };
            let demand = wanted
                .then(|| pool.request_with_batching(oid, immediate))
                .transpose()?;
            if let Some(candidates) = candidates.filter(|_| self.forecast_seen.insert(oid)) {
                let odb = self.odb()?;
                pool.prefetch(oid, candidates, |oid| {
                    odb.as_ref().is_some_and(|odb| odb.exists(oid))
                });
            }
            Ok(demand)
        })();
        self.record_result(outcome)
    }

    fn prepare_pool(&mut self) -> Result<()> {
        if self.demand_pool.is_some() || self.pool_checked || crate::terminal_ui::preparing() {
            return Ok(());
        }
        self.open_demand_pool()
    }

    fn open_demand_pool(&mut self) -> Result<()> {
        let http = self.uses_http();
        if http {
            self.try_http2_pool()?;
        }
        self.pool_checked = true;
        if self.demand_pool.is_some() {
            return Ok(());
        }
        let Some(connection) = self.connection(http)? else {
            return Ok(());
        };
        self.session_checked = true;
        let first = match self.session.take() {
            Some(first) => first,
            None => {
                let Some(first) = connection.connect(None)? else {
                    return Ok(());
                };
                self.session_starts += 1;
                first
            }
        };
        let pool = crate::demand::Pool::new(first, connection, &self.raw_blobs)?;
        self.install_pool(pool);
        Ok(())
    }

    fn install_pool(&mut self, pool: crate::demand::Pool) {
        self.pack_verification.observe(&pool);
        self.demand_pool = Some(pool);
    }

    fn try_http2_pool(&mut self) -> Result<()> {
        if self.http2_checked
            || crate::git::controlling_terminal().is_some() && crate::terminal_ui::preparing()
        {
            return Ok(());
        }
        self.http2_checked = true;
        self.open_http2_pool(crate::http2::Selection::Demand)?;
        Ok(())
    }

    fn open_http2_pool(&mut self, selection: crate::http2::Selection) -> Result<Option<Oid>> {
        let pool = self.with_http2_configuration(|configuration, scratch| {
            crate::demand::Pool::http2(configuration, selection, scratch)
        })?;
        let Some((pool, selected)) = pool else {
            return Ok(None);
        };
        if let Some(session) = self.session.take() {
            session.stop()?;
        }
        self.install_pool(pool);
        self.session_starts += 1;
        Ok(selected)
    }

    fn with_http2_configuration<T>(
        &mut self,
        open: impl FnOnce(crate::http2::Configuration, &crate::raw_blob::RawBlobs) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        if !self.demand_url.as_bytes().starts_with(b"https://")
            || !self.git.http2_compatible()
            || !self.git.demand_session_compatible()?
        {
            return Ok(None);
        }
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        self.receive_credential_repository()?;
        let git = self.git.clone();
        let directory = self.dir.clone();
        let mut existing_repository = self.repo.is_some();
        let (prepared, repository) = mpsc::channel();
        let credential =
            move |url: &str,
                  challenges: &[Vec<u8>],
                  previous: Option<&crate::http2::Credentials>| {
                if !(git.credential_replay_has_host_scope()
                    && crate::http2::replayable_credential_challenge(
                        challenges,
                        previous.is_some_and(
                            crate::http2::Credentials::continues_encoded_authentication,
                        ),
                    ))
                {
                    // Native Git owns repository-scoped credentials and other
                    // challenges outside a supported exchange.
                    return Ok(None);
                }
                let command = || Self::native_command(&git, &directory);
                if !existing_repository {
                    // A configured helper can choose credentials based on the
                    // repository context. Create it before the first credential
                    // lookup, keeping the anonymous path entirely in memory.
                    let repo = Self::create_native_repository(&directory);
                    let ready = repo.is_ok();
                    let _ = prepared.send(repo);
                    if !ready {
                        return Ok(None);
                    }
                    existing_repository = true;
                }
                let credentials = crate::http2::credentials(command(), url, challenges, previous)?;
                Ok(credentials.map(|mut credentials| {
                    let git = git.clone();
                    let directory = directory.clone();
                    credentials.on_verdict(move |data, accepted, challenges| {
                        let command = Self::native_command(&git, &directory);
                        crate::http2::report_credentials(command, data, accepted, challenges)
                    });
                    credentials
                }))
            };
        let Some(configuration) = crate::http2::Configuration::new(
            &self.demand_url,
            &self.native_git_version,
            credential,
        ) else {
            return Ok(None);
        };
        let ready = open(configuration, &self.raw_blobs);
        self.credential_repository = Some(repository);
        self.receive_credential_repository()?;
        ready
    }

    fn fetch_snapshot(&mut self, target: SnapshotTarget<'_>) -> Result<SnapshotPreparation> {
        let branch = target.branch();
        let selection = branch.cloned().map_or(
            crate::http2::Selection::Exact,
            crate::http2::Selection::Branch,
        );
        let https = self.with_http2_configuration(|configuration, scratch| {
            let ready = crate::http2::Client::negotiate(
                configuration,
                Default::default(),
                selection,
                transport::FetchMode::Snapshot {
                    selecting_ref: branch.is_some(),
                },
            );
            crate::object::check_transfer()?;
            // No objects have been requested during discovery. Native fetch
            // can still apply the caller's full compatibility policy.
            let (mut client, selected) = match ready {
                Ok(Some(ready)) => ready,
                Err(error) if error.is::<crate::http2::AuthenticationRejected>() => {
                    return Err(error);
                }
                _ => return Ok(None),
            };
            let Some(oid) = target.oid(selected) else {
                return Ok(None);
            };
            let response = crate::pack_stream::Response::for_materialization(scratch.clone(), oid);
            let pack = match client.fetch_snapshot(oid, response) {
                Err(error) if error.is::<crate::http2::NativeAuthentication>() => None,
                result => result.with_context(|| format!("fetch shallow HTTPS snapshot {oid}"))?,
            };
            Ok(Some((oid, pack)))
        })?;
        let ((oid, mut received), transport) = match https {
            Some((oid, Some((pack, http2)))) => {
                self.session_starts += 1;
                ((oid, pack), SnapshotTransport::Https { http2 })
            }
            Some((oid, None)) => return Ok(SnapshotPreparation::Native(Some(oid))),
            None => {
                if let Some(received) = self.fetch_ssh_snapshot(target)? {
                    (received, SnapshotTransport::Ssh)
                } else if let Some(received) = self.fetch_file_snapshot(target)? {
                    (received, SnapshotTransport::File)
                } else {
                    return Ok(SnapshotPreparation::Native(None));
                }
            }
        };
        received.cache_extras(&self.raw_blobs, None, |_| {});
        self.snapshot_transport = Some(transport);
        self.completed_fetches += 1;
        self.session_fetch_operations += 1;
        self.session_decode_operations += 1;
        self.session_received_pack_bytes += received.bytes;
        self.eager_verified_pack_capacity = received.eager_verified_capacity;
        Ok(SnapshotPreparation::Ready(oid))
    }

    fn fetch_file_snapshot(
        &mut self,
        target: SnapshotTarget<'_>,
    ) -> Result<Option<(Oid, crate::pack_stream::Received)>> {
        let mode = transport::FetchMode::Snapshot {
            selecting_ref: target.branch().is_some(),
        };
        let Some((session, oid)) = self.open_file_session(target, mode)? else {
            return Ok(None);
        };
        let response =
            crate::pack_stream::Response::for_materialization(self.raw_blobs.clone(), oid);
        let received = session.fetch_snapshot(oid, response, "local Git")?;
        self.session_starts += 1;
        Ok(Some((oid, received)))
    }

    fn select_file(&mut self, target: SnapshotTarget<'_>) -> Result<Option<Oid>> {
        let mode = transport::FetchMode::FilteredSelection {
            selecting_ref: target.branch().is_some(),
        };
        let Some((session, oid)) = self.open_file_session(target, mode)? else {
            return Ok(None);
        };
        self.session = Some(session);
        self.session_checked = true;
        self.session_starts += 1;
        Ok(self.fetch_in_session(oid)?.then_some(oid))
    }

    fn open_file_session(
        &mut self,
        target: SnapshotTarget<'_>,
        mode: transport::FetchMode,
    ) -> Result<Option<(transport::Session, Oid)>> {
        let url = self.demand_url.as_bytes();
        if self.socket.is_some()
            || url.is_empty()
            || url.starts_with(b"-")
            || (!url.starts_with(b"file://") && url.windows(3).any(|part| part == b"://"))
            || !self.git.transport_policy_is_default("file")
            || !self.git.demand_session_compatible()?
        {
            return Ok(None);
        }
        if !url.starts_with(b"file://") && self.git.cwd.join(&self.demand_url).is_file() {
            // Git can select the bundle transport for a local file operand.
            return Ok(None);
        }
        if self.plain_file_path().is_none() && self.repo.is_none() {
            // Native fetch-pack needs a Git directory even to parse a URL.
            // Keep streamed objects in RAM until native fetch needs the ODB.
            self.repo = Some(PrivateRepository::Context(Self::create_native_repository(
                &self.dir,
            )?));
        }
        let Some(command) = self.non_http_command(false)? else {
            return Ok(None);
        };
        let ready = match transport::StartingSession::spawn(command, false, None) {
            Ok(session) => session.negotiate_selection(target.branch(), mode),
            Err(_) => Ok(None),
        };
        crate::object::check_transfer()?;
        let Some((session, selected)) = ready? else {
            return Ok(None);
        };
        Ok(target.oid(selected).map(|oid| (session, oid)))
    }

    fn fetch_ssh_snapshot(
        &mut self,
        target: SnapshotTarget<'_>,
    ) -> Result<Option<(Oid, crate::pack_stream::Received)>> {
        if let (Some(socket), SnapshotTarget::Object(oid)) = (self.ssh_socket(), target) {
            if let Some(error) = self.failure() {
                bail!("remote object transport previously failed: {error}");
            }
            socket.check()?;
            // A master can allow just one server channel. Finish the shallow
            // snapshot on the channel that already read the selected revision.
            if let Some(session) = self.session.take() {
                let response =
                    crate::pack_stream::Response::for_materialization(self.raw_blobs.clone(), oid);
                if let Some(received) = session.reuse_ssh_snapshot(oid, response)? {
                    return Ok(Some((oid, received)));
                }
            }
            if let Some(pool) = &self.demand_pool {
                if let Some(pack) = pool.fetch_ssh_snapshot(oid)? {
                    return Ok(Some((oid, pack)));
                }
            }
            self.close_lazy_channels_for_copy()?;
        }
        let Some(command) = self.direct_ssh_command()? else {
            return Ok(None);
        };
        let mode = transport::FetchMode::Snapshot {
            selecting_ref: target.branch().is_some(),
        };
        let ready = match transport::StartingSession::spawn(command, false, None) {
            Ok(session) => session.negotiate_selection(target.branch(), mode),
            Err(_) => Ok(None),
        };
        self.record_socket_owner()?;
        crate::object::check_transfer()?;
        let Some((session, selected)) = ready? else {
            return Ok(None);
        };
        let Some(oid) = target.oid(selected) else {
            return Ok(None);
        };
        self.ssh_socket()
            .context("OpenSSH did not create the owned control socket")?
            .check()?;
        let response =
            crate::pack_stream::Response::for_materialization(self.raw_blobs.clone(), oid);
        let received = session.fetch_snapshot(oid, response, "SSH")?;
        self.session_starts += 1;
        Ok(Some((oid, received)))
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
                    match self.raw_blobs.received(oid) {
                        Some(ReceivedObject::Verified { object, transfer }) => {
                            self.publish_verified(object, transfer)?;
                        }
                        Some(ReceivedObject::Packed(directory)) => {
                            self.import_packed_object(oid, directory.path())?;
                        }
                        None => (),
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
                self.failed.get_or_init(|| format!("{error:#}"));
                let _drawing = crate::terminal_ui::native_terminal_lease(true);
                eprintln!("git-on: remote object loading failed: {error:#}");
            }
        }
        outcome
    }
    fn fetch(&mut self, oids: &[Oid]) -> Result<()> {
        // Session reuse bypasses command(), so reject incomplete invocations
        // before either transport can start another missing-object demand.
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        if self.snapshot_transport.is_some() {
            bail!("shallow snapshot pack omitted required object {}", oids[0]);
        }
        if let ([oid], false) = (oids, self.materializing) {
            if let Some(pool) = &self.demand_pool {
                let batch = pool.request(*oid)?.wait();
                return self.finish_demand(&batch);
            }
            if self.fetch_in_session(*oid)? {
                return Ok(());
            }
        }
        self.prepare_native_repository()?;
        let mut command = self.command(false)?;
        command.args([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--depth=1",
            "--filter=tree:0",
        ]);
        crate::terminal_ui::configure_fetch(&mut command);
        let mut requests = Vec::new();
        if oids.len() == 1 {
            command
                .arg("--")
                .arg(&self.source)
                .arg(oids[0].to_string())
                .stdin(Stdio::null());
        } else {
            for oid in oids {
                writeln!(requests, "{oid}")?;
            }
            command.arg("--stdin").arg("--").arg(&self.source);
        }
        let output = if requests.is_empty() {
            crate::git::run_fetch(command, None)
        } else {
            crate::git::run_fetch(command, Some(&mut requests.as_slice()))
        };
        let out = match output {
            Ok(out) => out,
            Err(error) => {
                return Err(error).context("fetch exact remote object");
            }
        };
        let error = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let selected = if oids.len() == 1 {
                format!("object {}", oids[0])
            } else {
                format!("objects ({} requests, first {})", oids.len(), oids[0])
            };
            bail!("remote fetch of exact {selected} failed (server must allow requested unadvertised OIDs and filtering; SSH connection must remain alive): {}{}", error.trim(), authentication_advice(&error, self.https_alternative.as_deref()));
        }
        if error.contains("filtering not recognized") || error.contains("filter capability") {
            bail!(
                "remote server does not support object filtering; full-clone fallback is forbidden"
            );
        }
        if self.materializing {
            self.stage_materialization()?;
        } else {
            self.import_received(oids)?;
        }
        let odb = self.native_repository().unwrap().odb()?;
        for oid in oids {
            if !self.raw_blobs.available(*oid) && !odb.exists(*oid) {
                bail!("filtered remote fetch did not supply requested object {oid}");
            }
        }
        drop(odb);
        self.completed_fetches += oids.len() as u64;
        self.fetch_operations += 1;
        Ok(())
    }
    fn http_command(&self) -> Result<Command> {
        let mut command = self.command(false)?;
        command
            .args(["-c", "protocol.version=2"])
            .arg(if self.demand_url.as_bytes().starts_with(b"https://") {
                "remote-https"
            } else {
                "remote-http"
            })
            .arg(&self.source)
            .arg(&self.demand_url);
        Ok(command)
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

    fn connection(&self, http: bool) -> Result<Option<crate::transport::Connection>> {
        if self.demand_url.as_bytes().starts_with(b"-") || !self.git.demand_session_compatible()? {
            return Ok(None);
        }
        let command = if http {
            Some(self.http_command()?)
        } else {
            self.non_http_command(false)?
        };
        Ok(command
            .map(|command| crate::transport::Connection::new(command, http, self.ssh_socket())))
    }

    fn start_session(&mut self, http: bool) -> Result<Option<crate::transport::Session>> {
        let Some(connection) = self.connection(http)? else {
            return Ok(None);
        };
        self.session_starts += 1;
        connection.connect(None)
    }

    fn direct_ssh_command(&self) -> Result<Option<Command>> {
        if self.socket.is_none()
            || !self.git.transport_policy_is_default("ssh")
            || !self.git.demand_session_compatible()?
        {
            return Ok(None);
        }
        if let Some(socket) = self.ssh_socket() {
            socket.check()?;
        }
        let Some(mut command) = self.non_http_command(self.socket_owner.is_none())? else {
            return Ok(None);
        };
        // An existing agent or identity can open a direct v2 session. Leave
        // interactive authentication to native Git's normal preparation path.
        command.env("SSH_ASKPASS_REQUIRE", "never");
        Ok(Some(command))
    }

    fn select_ssh(
        &mut self,
        exact: Option<Oid>,
        branch: Option<&crate::transport::BranchQuery>,
    ) -> Result<Option<Oid>> {
        let Some(command) = self.direct_ssh_command()? else {
            return Ok(None);
        };
        let started = Instant::now();
        let mut additional = transport::copy_command(&command);
        additional
            .env("GIT_ON_SSH_INITIAL", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false");
        if let Some(branch) = branch.filter(|_| !self.git.sets_protocol_version()) {
            return self.select_ssh_branch(branch, command, additional);
        }
        let mut candidates: Vec<_> = transport::StartingSession::spawn(command, false, None)
            .into_iter()
            .collect();
        let mut connection = None;
        // Exercise the primary-to-pool handoff even under load in the test
        // that counts command preparation calls.
        let use_standby = !cfg!(feature = "test-metrics")
            || std::env::var_os("GIT_ON_TEST_DISABLE_SSH_STANDBY").is_none();
        let session = (|| -> Result<_> {
            while !candidates.is_empty() {
                if let Some(index) =
                    transport::StartingSession::wait_for_advertisement(&candidates)?
                {
                    if let Ok(Some(session)) = candidates.swap_remove(index).negotiate() {
                        return Ok(Some(session));
                    }
                } else if use_standby
                    && connection.is_none()
                    && started.elapsed() >= SSH_STANDBY_DELAY
                {
                    self.record_socket_owner()?;
                    if let Some(socket) = self.ssh_socket() {
                        let owned = transport::Connection::new(
                            transport::copy_command(&additional),
                            false,
                            Some(socket),
                        );
                        for _ in 0..SSH_STANDBY_CHANNELS {
                            if let Ok(candidate) = owned.start_additional() {
                                candidates.push(candidate);
                            }
                        }
                        connection = Some(owned);
                    }
                }
            }
            Ok(None)
        })();
        self.record_socket_owner()?;
        crate::object::check_transfer()?;
        let Some(mut session) = session? else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.socket_owner.is_some(),
            "OpenSSH did not create the owned control socket"
        );
        let oid = match (exact, branch) {
            (Some(oid), _) => oid,
            (None, Some(branch)) => match session.select_branch(branch) {
                Ok(Some(oid)) => oid,
                _ => {
                    crate::object::check_transfer()?;
                    return Ok(None);
                }
            },
            (None, None) => return Ok(None),
        };
        self.fetch_selected_ssh(oid, session, connection, candidates)
    }

    fn select_ssh_branch(
        &mut self,
        branch: &transport::BranchQuery,
        mut command: Command,
        additional: Command,
    ) -> Result<Option<Oid>> {
        // The first channel can advertise refs as soon as it opens. Negotiate
        // the object channels through the same authenticated OpenSSH master.
        command.env("GIT_PROTOCOL", "version=0");
        let stop = Arc::new(AtomicBool::new(false));
        let Ok(starter) = transport::StartingSession::spawn(command, false, Some(stop.clone()))
        else {
            crate::object::check_transfer()?;
            return Ok(None);
        };
        let picked = std::thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(1);
            let advertisement = scope.spawn(move || {
                let _ = send.send(starter.advertised_branch(branch).ok().flatten());
            });
            let picked = (|| -> Result<_> {
                let connection = loop {
                    self.record_socket_owner()?;
                    if let Some(socket) = self.ssh_socket() {
                        break transport::Connection::new(additional, false, Some(socket));
                    }
                    if advertisement.is_finished() {
                        return Ok(None);
                    }
                    crate::object::check_transfer()?;
                    std::thread::sleep(Duration::from_millis(5));
                };
                let mut candidates = (0..=SSH_STANDBY_CHANNELS)
                    .filter_map(|_| connection.start_additional().ok())
                    .collect::<Vec<_>>();
                let started = Instant::now();
                let mut tried_replacement = false;
                while !candidates.is_empty() {
                    if let Some(index) =
                        transport::StartingSession::wait_for_advertisement(&candidates)?
                    {
                        if let Ok(Some(primary)) = candidates.swap_remove(index).negotiate() {
                            let result =
                                Self::resolve_ssh_branch(primary, &mut candidates, branch, receive);
                            crate::object::check_transfer()?;
                            return Ok(result
                                .ok()
                                .flatten()
                                .map(|(session, oid)| (connection, candidates, session, oid)));
                        }
                    } else if !tried_replacement
                        && advertisement.is_finished()
                        && started.elapsed() >= SSH_STANDBY_DELAY
                    {
                        // A v2 spare can reach the server ahead of the initial
                        // advertisement and consume its only spare slot. The
                        // initial channel has since closed, so retry once on
                        // that slot without waiting for the stalled spare.
                        tried_replacement = true;
                        if let Ok(candidate) = connection.start_additional() {
                            candidates.push(candidate);
                        }
                    }
                }
                // A master with only one session can reject every spare
                // while its first upload-pack is still opening. Finish that
                // channel before falling back on the authenticated socket.
                let _ = advertisement.join();
                Ok(None)
            })();
            stop.store(true, Ordering::Relaxed);
            picked
        })?;
        let Some((connection, candidates, session, oid)) = picked else {
            return Ok(None);
        };
        self.session_starts += 1;
        self.fetch_selected_ssh(oid, session, Some(connection), candidates)
    }

    fn resolve_ssh_branch(
        mut primary: transport::Session,
        spares: &mut Vec<transport::StartingSession>,
        branch: &transport::BranchQuery,
        advertisement: mpsc::Receiver<Option<Oid>>,
    ) -> Result<Option<(transport::Session, Oid)>> {
        match advertisement.try_recv() {
            Ok(Some(oid)) => return Ok(Some((primary, oid))),
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                return Ok(primary.select_branch(branch)?.map(|oid| (primary, oid)));
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
        primary.request_branch(branch)?;
        let mut advertised = None;
        while !primary.branch_ready()? {
            if advertised.is_none() {
                match advertisement.try_recv() {
                    Ok(Some(oid)) => advertised = Some(oid),
                    Ok(None) | Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => (),
                }
            }
            if let Some(oid) = advertised {
                if let Some(index) = spares
                    .iter()
                    .position(transport::StartingSession::filtered_v2_ready)
                {
                    if let Ok(Some(secondary)) = spares.swap_remove(index).negotiate() {
                        return Ok(Some((secondary, oid)));
                    }
                }
            }
        }
        Ok(primary.read_branch(branch)?.map(|oid| (primary, oid)))
    }

    fn fetch_selected_ssh(
        &mut self,
        oid: Oid,
        session: transport::Session,
        connection: Option<transport::Connection>,
        mut candidates: Vec<transport::StartingSession>,
    ) -> Result<Option<Oid>> {
        self.session = Some(session);
        self.session_checked = true;
        self.session_starts += 1;
        let fetch_started = Instant::now();
        if !self.fetch_in_session(oid)? {
            return Ok(None);
        }
        if let Some(connection) = connection {
            if fetch_started.elapsed() >= crate::demand::SLOW_REPLY {
                // Let the first fetch finish before negotiating the next
                // spares, so they can open while the root is being fetched.
                for _ in 0..SSH_STANDBY_CHANNELS {
                    if let Ok(candidate) = connection.start_additional() {
                        candidates.push(candidate);
                    }
                }
            }
            let mut pool =
                crate::demand::Pool::prepare_native(&connection, &self.raw_blobs, candidates)?;
            let primary = self
                .session
                .take()
                .context("missing selected SSH channel")?;
            pool.attach_primary(primary, connection)?;
            self.pool_checked = true;
            self.install_pool(pool);
        }
        Ok(Some(oid))
    }

    fn non_http_command(&self, initial: bool) -> Result<Option<Command>> {
        let native = self.command(initial)?;
        let command = if self.socket.is_some() {
            self.ssh_upload_pack(&native)?
        } else {
            self.file_upload_pack()?
        };
        let Some(mut command) = command else {
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
        // Match native Git's local transport child environment for this exact
        // installed Git version, including config/object-location overrides.
        let local = match self.local_git_environment.get() {
            Some(local) => local,
            None => {
                let local = self.git.output(
                    None,
                    &[OsStr::new("rev-parse"), OsStr::new("--local-env-vars")],
                )?;
                self.local_git_environment.get_or_init(|| local)
            }
        };
        for key in local
            .split(|byte| *byte == b'\n')
            .filter(|key| !key.is_empty())
        {
            command.env_remove(OsStr::from_bytes(key));
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
        let mut probe = Command::new("git");
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
    fn plain_file_path(&self) -> Option<&OsStr> {
        let path = self.demand_url.as_bytes().strip_prefix(b"file://")?;
        // Use Git's parser for escaping and host syntax, including brackets.
        (path.starts_with(b"/")
            && !path.starts_with(b"//")
            && !path.contains(&b'%')
            && !path.windows(2).any(|part| part == b"@["))
        .then_some(OsStr::from_bytes(path))
    }

    fn file_upload_pack(&self) -> Result<Option<Command>> {
        if let Some(path) = self.plain_file_path() {
            let mut command = Command::new("git");
            command.args([OsStr::new("upload-pack"), OsStr::new("--"), path]);
            return Ok(Some(command));
        }
        let mut command = self.command(false)?;
        command
            .args(["fetch-pack", "--diag-url"])
            .arg(&self.demand_url)
            .stdin(Stdio::null());
        let diagnostic = crate::git::run_builtin(command)?;
        if !diagnostic.status.success() {
            return Ok(None);
        }
        let mut prefix = b"Diag: url=".to_vec();
        prefix.extend_from_slice(self.demand_url.as_bytes());
        prefix.extend_from_slice(b"\nDiag: protocol=file\nDiag: hostandport=");
        let Some(path) = diagnostic
            .stdout
            .strip_prefix(prefix.as_slice())
            .and_then(|rest| rest.splitn(2, |byte| *byte == b'\n').nth(1))
            .and_then(|rest| rest.strip_prefix(b"Diag: path="))
            .and_then(|rest| rest.strip_suffix(b"\n"))
        else {
            return Ok(None);
        };
        if path.starts_with(b"-") {
            return Ok(None);
        }
        let mut command = Command::new("git");
        command
            .args([OsStr::new("upload-pack"), OsStr::new("--")])
            .arg(OsStr::from_bytes(path));
        Ok(Some(command))
    }
    fn prepare_session(&mut self) -> Result<()> {
        if !self.session_checked {
            let http = self.uses_http();
            let terminal = crate::git::controlling_terminal().is_some();
            if terminal && http && crate::terminal_ui::preparing() {
                // Preparation owns interactive authentication. Leave the
                // session decision open until the command is running.
                return Ok(());
            }
            if http || !terminal {
                self.session = self.start_session(http)?;
            }
            self.session_checked = true;
        }
        Ok(())
    }
    fn fetch_in_session(&mut self, oid: Oid) -> Result<bool> {
        self.prepare_session()?;
        let socket = self.ssh_socket();
        let Some(session) = self.session.as_mut() else {
            return Ok(false);
        };
        if let Some(socket) = socket {
            socket.check()?;
        }
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
            match session.fetch_stream(&[oid], transport::FetchMode::Filtered, response) {
                Ok(received) => received,
                Err(error) => {
                    self.session = None;
                    return Err(error);
                }
            };
        self.session_fetch_operations += 1;
        self.session_received_pack_bytes = self
            .session_received_pack_bytes
            .saturating_add(received.bytes);
        let object = object.context("stream did not publish requested Git object")?;
        self.publish_verified(object, None)?;
        received.cache_extras(&self.raw_blobs, None, |_| {});
        self.session_decode_operations += 1;
        self.completed_fetches += 1;
        Ok(true)
    }
    /// Native filtered fetches always retain promisor packs. Keeping one pack
    /// per demand makes later Git/libgit2 pack discovery grow with all previous
    /// demands. Decode only new received objects directly into raw run storage,
    /// then release their files/indexes before the next operation. The private
    /// repository has one serialized writer and no auto-maintenance.
    fn take_received(&mut self) -> Result<Option<ReceivedPack>> {
        let pack_directory = self.dir.join("objects/pack");
        let mut received = Vec::new();
        for entry in fs::read_dir(&pack_directory)? {
            crate::object::check_transfer()?;
            let entry = entry?;
            let name = entry.file_name();
            let Some(text) = name.to_str() else {
                bail!("invalid received pack filename");
            };
            if let Some((hash, _)) = text
                .strip_prefix("pack-")
                .and_then(|name| name.split_once('.'))
            {
                if hash.len() == 40 && Oid::from_str(hash).is_ok() && entry.file_type()?.is_file() {
                    received.push((name, entry.path()));
                }
            }
        }
        if received.is_empty() {
            return Ok(None);
        }
        let staging = Arc::new(
            tempfile::Builder::new()
                .prefix("received-pack-")
                .tempdir_in(&self.dir)?,
        );
        let mut options = git2::RepositoryInitOptions::new();
        options.bare(true).external_template(false);
        let staging_repo = Repository::init_opts(staging.path(), &options)?;
        let objects = staging.path().join("objects/pack");
        // The decoder sees only this newly received pack set, never all prior
        // demands. Companion indexes, including optional .rev, move together.
        for (name, path) in &received {
            crate::object::check_transfer()?;
            if path
                .extension()
                .is_some_and(|extension| extension == "pack")
            {
                self.native_received_pack_bytes = self
                    .native_received_pack_bytes
                    .saturating_add(fs::metadata(path)?.len());
            }
            fs::rename(path, objects.join(name))?;
        }
        Ok(Some(ReceivedPack {
            repo: staging_repo,
            directory: staging,
        }))
    }
    fn stage_materialization(&mut self) -> Result<()> {
        let Some(received) = self.take_received()? else {
            return Ok(());
        };
        let destination = Repository::open_bare(&self.dir)?;
        let destination_odb = destination.odb()?;
        let odb = received.repo.odb()?;
        let mut failure = None;
        let iteration = odb.foreach(|oid| {
            let outcome = (|| {
                crate::object::check_transfer()?;
                let (size, kind) = odb.read_header(*oid)?;
                if kind == git2::ObjectType::Blob {
                    self.raw_blobs.remember_packed(*oid, &received.directory);
                } else if !destination_odb.exists(*oid) && self.raw_blobs.get(*oid)?.is_none() {
                    let object = odb.read(*oid)?;
                    self.publish_received(&destination, *oid, kind, size, object.data())?;
                }
                Ok(())
            })();
            if let Err(error) = outcome {
                failure = Some(error);
                return false;
            }
            true
        });
        if let Some(error) = failure {
            return Err(error);
        }
        iteration?;
        Ok(())
    }
    fn import_received(&mut self, wanted: &[Oid]) -> Result<()> {
        let Some(received) = self.take_received()? else {
            return Ok(());
        };
        let destination = Repository::open_bare(&self.dir)?;
        let destination_odb = destination.odb()?;
        let mut requests = Vec::new();
        let mut failure = None;
        let received_odb = received.repo.odb()?;
        let iteration = received_odb.foreach(|oid| {
            let outcome = (|| -> Result<()> {
                crate::object::check_transfer()?;
                if self.raw_blobs.get(*oid)?.is_none() && !destination_odb.exists(*oid) {
                    let (size, kind) = received_odb.read_header(*oid)?;
                    if kind == git2::ObjectType::Blob {
                        self.raw_blobs.remember_packed(*oid, &received.directory);
                        if wanted.contains(oid) {
                            writeln!(requests, "{oid}")?;
                        }
                    } else {
                        let object = received_odb.read(*oid)?;
                        self.publish_received(&destination, *oid, kind, size, object.data())?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = outcome {
                failure = Some(error);
                return false;
            }
            true
        });
        if let Some(error) = failure {
            return Err(error);
        }
        iteration?;
        // Release ODB/pack descriptors before launching the native decoder.
        drop(received_odb);
        drop(received.repo);
        drop(destination_odb);
        if requests.is_empty() {
            return Ok(());
        }
        drop(destination);
        self.decode_received(received.directory.path(), &mut requests.as_slice())
    }
    fn import_packed_object(&mut self, oid: Oid, directory: &Path) -> Result<()> {
        let request = format!("{oid}\n");
        self.decode_received(directory, &mut request.as_bytes())?;
        anyhow::ensure!(
            self.raw_blobs.get(oid)?.is_some()
                || Repository::open_bare(&self.dir)?.odb()?.exists(oid),
            "received pack omitted requested object {oid}"
        );
        Ok(())
    }
    fn decode_received(&mut self, directory: &Path, requests: &mut dyn Read) -> Result<()> {
        let destination = Repository::open_bare(&self.dir)?;
        let mut command = self.git.command(Some(&self.dir));
        command
            .env("GIT_OBJECT_DIRECTORY", directory.join("objects"))
            .args([
                "-c",
                "core.bigFileThreshold=8m",
                "cat-file",
                "--batch",
                "--buffer",
            ]);
        let remote = &mut *self;
        let (output, ()) = crate::git::run_with_output(command, Some(requests), move |input| {
            remote.import_raw_output(&destination, input)
        })
        .context("decode received objects into run-local raw storage")?;
        if !output.status.success() {
            bail!(
                "cannot decode received Git objects: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        self.decode_operations += 1;
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
        let Some(repo) = self.native_repository() else {
            // Commit/tag headers can be read directly from the verified buffer.
            self.raw_blobs
                .remember_verified([object], transfer.as_ref());
            return Ok(());
        };
        let oid = object.oid();
        let odb = repo.odb()?;
        let mut writer = odb.writer(object.data().len(), kind)?;
        crate::object::write_verified_buffer(object.data(), &mut writer)?;
        anyhow::ensure!(writer.finalize()? == oid, "received metadata OID changed");
        self.raw_blobs.forget_received(oid);
        if let Some(transfer) = transfer {
            transfer.mark_served();
        }
        Ok(())
    }
    fn publish_received(
        &mut self,
        destination: &Repository,
        oid: Oid,
        kind: git2::ObjectType,
        size: usize,
        input: impl Read,
    ) -> Result<()> {
        if kind == git2::ObjectType::Blob {
            self.raw_blobs.insert_fetched(oid, size, input, None)?;
        } else if kind == git2::ObjectType::Tree {
            self.raw_blobs.insert_tree(oid, size, input)?;
        } else {
            // Native signature verification needs the selected commit or tag.
            let odb = destination.odb()?;
            let mut writer = odb.writer(size, kind)?;
            crate::object::copy_verified(input, &mut writer, kind, size, oid)?;
            anyhow::ensure!(writer.finalize()? == oid, "received metadata OID changed");
            self.raw_blobs.forget_received(oid);
        }
        Ok(())
    }
    fn import_raw_output(&mut self, destination: &Repository, input: impl Read) -> Result<()> {
        let mut input = BufReader::new(input);
        loop {
            crate::object::check_transfer()?;
            let mut header = Vec::new();
            let length = Read::by_ref(&mut input)
                .take(128)
                .read_until(b'\n', &mut header)?;
            if length == 0 {
                break;
            }
            anyhow::ensure!(
                header.last() == Some(&b'\n'),
                "invalid native Git batch header"
            );
            let header = std::str::from_utf8(&header[..length - 1])?;
            let fields: Vec<_> = header.split(' ').collect();
            anyhow::ensure!(
                fields.len() == 3 && fields[0].len() == 40,
                "invalid native Git object header"
            );
            let oid = Oid::from_str(fields[0])?;
            let kind = match fields[1] {
                "blob" => git2::ObjectType::Blob,
                "tree" => git2::ObjectType::Tree,
                "commit" => git2::ObjectType::Commit,
                "tag" => git2::ObjectType::Tag,
                _ => bail!("unknown received Git object type"),
            };
            let size: usize = fields[2].parse().context("native Git object size")?;
            let payload = Read::by_ref(&mut input).take(size as u64);
            self.publish_received(destination, oid, kind, size, payload)?;
            let mut delimiter = [0];
            input.read_exact(&mut delimiter)?;
            anyhow::ensure!(delimiter == *b"\n", "invalid native Git object delimiter");
        }
        Ok(())
    }
    /// Release the owned master even when the run directory must remain for recovery.
    pub fn stop_transport(&self) -> Result<()> {
        if let Some(pool) = &self.demand_pool {
            pool.stop()?;
        }
        if let Some(session) = &self.session {
            if self.socket.is_some() && self.pack_verification.complete() {
                if let Err(error) = session.confirm_ssh_exit() {
                    self.failed.get_or_init(|| format!("{error:#}"));
                }
            }
            session.stop()?;
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
            .open(socket.parent().unwrap().join(SSH_CONTROL_OWNER_FILE))
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
            pool.streamed_packs + u64::from(self.snapshot_transport.is_some()),
            pool.prefetches,
            pool.prefetched_blobs,
        )
    }
    pub(crate) fn fetch_operations(&self) -> u64 {
        self.fetch_operations
    }
    pub(crate) fn decode_operations(&self) -> u64 {
        self.decode_operations
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
    pub(crate) fn received_pack_bytes(&self) -> (u64, u64) {
        (
            self.native_received_pack_bytes,
            self.session_received_pack_bytes + self.pool_metrics().bytes,
        )
    }
    pub(crate) fn eager_verified_pack_capacity(&self) -> usize {
        self.eager_verified_pack_capacity
    }
    pub(crate) fn http2_metrics(&self) -> (u64, u64) {
        let (fetches, peak) = self
            .demand_pool
            .as_ref()
            .map_or((0, 0), crate::demand::Pool::http2_metrics);
        if matches!(
            self.snapshot_transport,
            Some(SnapshotTransport::Https { http2: true })
        ) {
            return (fetches + 1, peak.max(1));
        }
        (fetches, peak)
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if let Err(error) = self.stop_transport() {
            let _drawing = crate::terminal_ui::native_terminal_lease(true);
            eprintln!("git-on: owned remote transport shutdown failed: {error:#}");
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
    if [ "$GIT_ON_SSH_INITIAL" != 1 ] && [ "$GIT_ON_SSH_ADDITIONAL" != 1 ]; then
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
    elif [ "$GIT_ON_SSH_ADDITIONAL" = 1 ]; then
        # Ask the master to open the session. A rejected channel in raw mux
        # proxy mode can disconnect the master. The false proxy forbids
        # a replacement connection if ordinary session multiplexing fails.
        exec "$executable" -o "ControlPath=$GIT_ON_SSH_CONTROL_PATH" -o ControlMaster=no -o BatchMode=yes -o ProxyCommand=/usr/bin/false "$@"
    else
        exec "$executable" -o "ControlPath=$GIT_ON_SSH_CONTROL_PATH" -o ControlMaster=no -o BatchMode=yes "$@"
    fi
}"#;

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
}
