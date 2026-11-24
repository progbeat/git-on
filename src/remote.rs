//! Explicit exact-object fetches. Git's promisor auto-fetch is never the loader.
use crate::{
    git::Git,
    raw_blob::{ReceivedObject, Transfer},
    service_storage::{io_out_of_space, Directory},
    transport,
};
use anyhow::{bail, Context, Result};
use git2::{Binding, IntoCString, Odb, Oid, Repository};
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
        process::CommandExt,
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
const SSH_LAUNCHER: &str = "git-on-ssh-launcher";
const SSH_READ_ONLY: &str = "StrictHostKeyChecking=yes UpdateHostKeys=no AddKeysToAgent=no ClearAllForwardings=yes ForwardAgent=no ForwardX11=no PermitLocalCommand=no Tunnel=no";

mod ssh_host_files;
mod ssh_jump;

#[doc(hidden)]
pub fn invoke_ssh_launcher() -> Option<i32> {
    let mut arguments = std::env::args_os().peekable();
    let launcher = PathBuf::from(arguments.next()?);
    if launcher.file_name()? != SSH_LAUNCHER
        || std::env::var_os("GIT_ON_SSH_LAUNCHER").as_deref() != Some(launcher.as_os_str())
    {
        return None;
    }
    let launching = arguments.next_if(|arg| arg == "--launch").is_some();
    let (program, git_arguments) = if launching {
        let program = arguments.next()?;
        let count = arguments.next()?.to_str()?.parse::<usize>().ok()?;
        (program, count)
    } else {
        // OpenSSH's implicit ProxyJump puts the destination last.
        (std::env::var_os("GIT_ON_SSH_CLIENT_PROGRAM")?, 1)
    };
    let mut options = Vec::new();
    if !launching {
        for option in ["ControlMaster=no", "ControlPath=none", "ControlPersist=no"]
            .into_iter()
            .chain(SSH_READ_ONLY.split_ascii_whitespace())
        {
            options.extend([OsString::from("-o"), OsString::from(option)]);
        }
    }
    options.extend(arguments);
    let mut command = Command::new(&program);
    let settings = options
        .len()
        .checked_sub(git_arguments)
        .context("SSH launcher is missing the Git arguments")
        .and_then(|mut before_host| {
            if !launching && before_host > 0 && options[before_host - 1] == "--" {
                before_host -= 1;
            }
            ssh_host_files::launch(&program, &launcher, options, before_host)
        });
    let error = match settings {
        Err(error) => error,
        Ok(settings) => {
            // OpenSSH uses argv[0] for its implicit ProxyJump command. Each
            // jump applies the same options and reads native config files.
            command.arg0(ssh_jump::LAUNCHER_COMMAND);
            command.env("GIT_ON_SSH_CLIENT_PROGRAM", settings.jump_client);
            command.args(settings.args).exec().into()
        }
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

struct ReceivedPack {
    // Release the reader and ODB/index descriptors before deleting the pack.
    decoder: Option<transport::BatchProcess>,
    odb: Odb<'static>,
    directory: Arc<tempfile::TempDir>,
}
impl ReceivedPack {
    fn objects(&self) -> PathBuf {
        self.directory.path().join("objects")
    }
}
struct NativeSelection {
    pack: ReceivedPack,
    metadata: HashSet<Oid>,
}
enum PrivateRepository {
    Context(Repository),
    Objects(Repository),
}
impl PrivateRepository {
    fn objects(&self) -> Option<&Repository> {
        match self {
            Self::Objects(repo) => Some(repo),
            Self::Context(_) => None,
        }
    }
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

pub struct Remote {
    pub git: Arc<Git>,
    native_git_version: String,
    pub dir: PathBuf,
    repo: Option<PrivateRepository>,
    run: PathBuf,
    source: OsString,
    https_alternative: Option<String>,
    completed_fetches: u64,
    fetch_operations: u64,
    decode_operations: u64,
    session_fetch_operations: u64,
    session_decode_operations: u64,
    session_starts: u64,
    native_received_pack_bytes: u64,
    #[cfg(feature = "test-metrics")]
    pub(crate) repositories_created: u64,
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
    ssh_storage: Option<Directory>,
    ssh_upload_pack_args: OnceLock<Vec<OsString>>,
    local_git_environment: OnceLock<Vec<u8>>,
    control_client: Option<ControlClient>,
    failed: OnceLock<String>,
    materializing: bool,
    snapshot_transport: Option<SnapshotTransport>,
    selection_pack: Option<NativeSelection>,
    active_materialized_pack: Option<ReceivedPack>,
    pub(crate) raw_blobs: crate::raw_blob::RawBlobs,
    // This must outlive the native repository and its staged readers.
    native_storage: Option<Directory>,
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
        let run = std::path::absolute(run).context("resolve private run directory")?;
        let run = run.as_path();
        // Capture effective configuration first: even `git --version` can read
        // it. Preserve the first native operation's cancellation/TTY lifecycle.
        // Config includes use the caller's context. Discover its repository
        // for native verification. Transport still uses the new run's ODB.
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
            dir,
            repo: None,
            run: run.to_path_buf(),
            source: source.to_os_string(),
            https_alternative,
            completed_fetches: 0,
            fetch_operations: 0,
            decode_operations: 0,
            session_fetch_operations: 0,
            session_decode_operations: 0,
            session_starts: 0,
            native_received_pack_bytes: 0,
            #[cfg(feature = "test-metrics")]
            repositories_created: 0,
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
            ssh_storage,
            ssh_upload_pack_args: OnceLock::new(),
            local_git_environment: OnceLock::new(),
            control_client,
            failed: OnceLock::new(),
            // Eager execution needs the whole selected tree in one transfer.
            materializing: materialized,
            snapshot_transport: None,
            selection_pack: None,
            active_materialized_pack: None,
            raw_blobs,
            native_storage: None,
        };
        let oid = remote.select_for_execution(revision, multi_remote)?;
        Ok((remote, oid))
    }

    fn select_for_execution(&mut self, revision: &OsStr, multi_remote: bool) -> Result<Oid> {
        if multi_remote {
            // A remote group has no single advertisement. Native Git handles
            // the explicit revision before opening any member's transport.
            let oid = self.fetch_selected(NativeRevision::Query(revision), true, false)?;
            return Ok(oid);
        }
        // Preserve native selection for unsupported settings and custom
        // ref maps. Default tracking destinations are unnecessary here.
        let native_selection = self
            .git
            .fetch_settings_require_native_git(self.materializing, self.socket.is_some())
            || self.git.has_nonstandard_fetch_mapping(&self.source)?;
        // Force controls destination updates. An unambiguous source-only
        // selector needs no private ref update with the standard tracking map.
        // Keep the original operand for all native compatibility paths.
        let stream_revision = revision
            .as_bytes()
            .strip_prefix(b"+")
            .map_or(revision, OsStr::from_bytes);
        let exact = (!native_selection
            && stream_revision.as_bytes().len() == 40
            && stream_revision.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .then(|| {
            stream_revision
                .to_str()
                .and_then(|value| Oid::from_str(value).ok())
        })
        .flatten();
        // On Darwin the private repository determines how native Git matches
        // non-ASCII refs.
        let select_ref_directly = !native_selection
            && exact.is_none()
            && (!cfg!(target_os = "macos") || stream_revision.as_bytes().is_ascii());
        let reference = select_ref_directly
            .then(|| transport::RefQuery::for_snapshot(stream_revision))
            .flatten();
        let stream_target = exact
            .map(SnapshotTarget::Object)
            .or_else(|| reference.as_ref().map(SnapshotTarget::Ref));
        let mut selected_for_native = None;
        if self.materializing {
            // Native fetch owns refspec selection and configured ref updates.
            // Plain refs can be selected directly when the server advertises
            // one unambiguous match.
            if let Some(target) = stream_target {
                match self.fetch_snapshot(target)? {
                    SnapshotPreparation::Ready(oid) => return Ok(oid),
                    SnapshotPreparation::Native(selected) => selected_for_native = selected,
                }
            }
            let revision = selected_for_native
                .map_or(NativeRevision::Query(revision), NativeRevision::Selected);
            let oid = self.fetch_selected(revision, self.socket_owner.is_none(), false)?;
            return Ok(oid);
        }
        // Select an exact ID or unambiguous reference on the same channel used
        // for mounting and later demands. Other refspecs use native Git.
        let reference = reference.as_ref().filter(|_| stream_revision != "HEAD");
        if exact.is_some() || reference.is_some() {
            let selection = reference
                .cloned()
                .map_or(crate::http2::Selection::Exact, crate::http2::Selection::Ref);
            let selected = self.open_http2_pool(selection)?.or(exact);
            if let (Some(oid), Some(pool)) = (selected, &self.demand_pool) {
                self.http2_checked = true;
                let batch = pool.request(oid, false)?.wait();
                if !batch.requires_native_preparation() {
                    self.pool_checked = true;
                    self.finish_demand(&batch)?;
                    return Ok(oid);
                }
                // The first fetch produced no objects. Native Git can
                // handle another HTTP version or its own authentication.
                drop(self.demand_pool.take());
                selected_for_native = Some(oid);
            }
            if let Some(oid) = self.select_ssh(exact, reference)? {
                return Ok(oid);
            }
        }
        if let Some(target) = stream_target {
            if let Some(oid) = self.select_file(target)? {
                return Ok(oid);
            }
        }
        self.prepare_native_repository()?;
        // Authentication and host-key interaction happen here, before mounting.
        let mut command = self.command(self.socket_owner.is_none())?;
        command
            .args(["-c", "protocol.version=2"])
            .arg("ls-remote")
            .arg("--symref")
            .arg("--")
            .arg(&self.source)
            .stdin(Stdio::inherit());
        let outcome =
            crate::git::run_ref_advertisement(command, &self.dir).context("advertise remote refs");
        // Record an established master even if upload-pack or cancellation then
        // fails. The in-memory identity protects Drop; the small private record
        // lets validated stale cleanup identify the same socket after SIGKILL.
        self.record_socket_owner()?;
        let (out, filtering) = outcome?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "remote ref advertisement/authentication failed: {}{}",
                stderr.trim(),
                authentication_advice(&stderr, self.https_alternative.as_deref())
            );
        }
        if self.socket.is_some() && self.socket_owner.is_none() {
            bail!("OpenSSH did not create the owned control socket");
        }
        if !filtering {
            bail!("remote server does not advertise object filtering; full-clone fallback is forbidden");
        }
        let _ = self.failed.take();
        if let Some(oid) = exact.filter(|_| self.socket_owner.is_some()) {
            self.session = self.start_session(false)?;
            if self.session.is_some() {
                self.session_checked = true;
                if self.fetch_in_session(oid)? {
                    return Ok(oid);
                }
            }
        }
        let revision =
            selected_for_native.map_or(NativeRevision::Query(revision), NativeRevision::Selected);
        let oid = self.fetch_selected(revision, false, false)?;
        Ok(oid)
    }
    pub(crate) fn native_repository(&self) -> Option<&Repository> {
        self.repo.as_ref().and_then(PrivateRepository::objects)
    }
    pub(crate) fn selection_odb(&self, oid: Oid) -> Option<&Odb<'_>> {
        self.selection_pack
            .as_ref()
            .filter(|selection| selection.metadata.contains(&oid))
            .map(|selection| &selection.pack.odb)
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn selection_metadata(&self) -> impl Iterator<Item = Oid> + '_ {
        self.selection_pack
            .iter()
            .flat_map(|selection| selection.metadata.iter().copied())
    }
    pub(crate) fn verify_selection(&self, oid: Oid, kind: git2::ObjectType) -> Result<()> {
        if let Some(ReceivedObject::Verified { object, .. }) = self.raw_blobs.received(oid) {
            return self.git.verify_streamed_object(&self.run, &object);
        }
        let objects = self
            .selection_pack
            .as_ref()
            .filter(|selection| selection.metadata.contains(&oid))
            .map(|selection| selection.pack.objects());
        self.git
            .verify_in_object_directory(&self.dir, oid, kind, objects.as_deref())
    }
    fn odb(&self) -> Result<Option<Odb<'_>>> {
        Ok(self.native_repository().map(Repository::odb).transpose()?)
    }
    fn prepare_native_repository(&mut self) -> Result<()> {
        let repo = match self.repo.take() {
            Some(PrivateRepository::Context(repo) | PrivateRepository::Objects(repo)) => repo,
            None => self.create_native_repository()?,
        };
        self.repo = Some(PrivateRepository::Objects(repo));
        Ok(())
    }

    fn create_native_repository(&mut self) -> Result<Repository> {
        let dir = self.dir.clone();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .context("create private Git repository directory")?;
        #[cfg(feature = "test-metrics")]
        {
            self.repositories_created += 1;
        }
        let mut options = git2::RepositoryInitOptions::new();
        options.bare(true).external_template(false);
        let repo = Repository::init_opts(&dir, &options)?;
        let repo = if self.run.join("REMOTE-REPOSITORY").try_exists()? {
            // Keep the writable libgit2 object database. Native Git manages
            // refs with reftable so their names retain case-sensitive semantics.
            let objects = Repository::from_odb(repo.odb()?)?;
            drop(repo);
            fs::remove_file(dir.join("HEAD"))?;
            fs::remove_file(dir.join("config"))?;
            fs::remove_dir_all(dir.join("refs"))?;
            let mut command = self.git.command(Some(&dir));
            command.env("TMPDIR", &self.run).args([
                "init",
                "--bare",
                "--quiet",
                "--template=",
                "--initial-branch=master",
                "--object-format=sha1",
                "--ref-format=reftable",
            ]);
            let output = crate::git::run_builtin(command)?;
            anyhow::ensure!(
                output.status.success(),
                "initialize private reftable repository: {}",
                String::from_utf8_lossy(&output.stderr).trim(),
            );
            objects
        } else {
            repo
        };
        // A source path can be a valid fetch operand but an invalid
        // promisor remote name. This separate promisor has no URL.
        git2::Config::open(&dir.join("config"))?.set_bool(
            &format!("remote.git-on-owned-{}.promisor", uuid::Uuid::new_v4()),
            true,
        )?;
        // FETCH_HEAD and refs stay on the filesystem of the run.
        // Git can write the incoming objects on a different one.
        match Directory::new_for_run("pack-", &self.run) {
            Ok(storage) => {
                std::os::unix::fs::symlink(storage.path(), dir.join("pack-staging"))?;
                self.native_storage = Some(storage);
            }
            Err(error)
                if std::env::var_os("GIT_ON_SERVICE_TMPDIR").is_none()
                    && io_out_of_space(&error) =>
            {
                self.use_run_pack_storage()?;
            }
            Err(error) => return Err(error),
        }
        Ok(repo)
    }

    fn use_run_pack_storage(&self) -> Result<()> {
        let directory = self.dir.join("native-packs");
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let link = self.dir.join("next-pack-staging");
        std::os::unix::fs::symlink(directory, &link)?;
        fs::rename(link, self.dir.join("pack-staging"))?;
        Ok(())
    }

    fn staged_pack_location(&self) -> Result<PathBuf> {
        let directory = fs::read_link(self.dir.join("pack-staging"))
            .context("open native pack staging directory")?;
        anyhow::ensure!(
            self.native_storage
                .as_ref()
                .is_some_and(|store| store.path() == directory)
                || directory == self.dir.join("native-packs"),
            "native pack staging directory changed"
        );
        Ok(directory)
    }

    fn can_move_pack_storage(&self, parent: &Path) -> bool {
        self.native_storage
            .as_ref()
            .is_some_and(|storage| storage.path() == parent && storage.can_fall_back_to(&self.run))
    }

    fn object_command(&self) -> Command {
        let mut command = self.git.command(Some(&self.dir));
        if let Some(storage) = self.native_storage.as_ref().or(self.ssh_storage.as_ref()) {
            command.env("TMPDIR", storage.path());
        }
        command
    }

    fn native_command(&self) -> Command {
        let mut command = self.object_command();
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
        let mut command = self.native_command();
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

    fn run_native_fetch(
        &mut self,
        mut initial: bool,
        input: Option<&[u8]>,
        writes_fetch_head: bool,
        prepare: impl Fn(&Self, bool) -> Result<Command>,
    ) -> Result<(std::process::Output, tempfile::TempDir)> {
        let alternate = self
            .selection_pack
            .as_ref()
            .map(|selection| git_alternate_path(&selection.pack.objects()));
        loop {
            let parent = self.staged_pack_location()?;
            let directory = match received_pack_directory(&parent) {
                Err(error) if io_out_of_space(&error) && self.can_move_pack_storage(&parent) => {
                    self.use_run_pack_storage()?;
                    continue;
                }
                result => result?,
            };
            let mut command = prepare(self, initial)?;
            command
                .env("TMPDIR", &parent)
                .env("GIT_OBJECT_DIRECTORY", directory.path().join("objects"));
            if let Some(alternate) = &alternate {
                // The pack's object directory has no repository or refs.
                command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
            }
            let mut input = input;
            self.fetch_operations += 1;
            let output =
                crate::git::run_fetch(command, input.as_mut().map(|input| input as &mut dyn Read));
            let mut retry = false;
            if let Ok(output) = &output {
                if let Ok(bytes) = received_pack_bytes(directory.path()) {
                    self.native_received_pack_bytes =
                        self.native_received_pack_bytes.saturating_add(bytes);
                }
                retry = !output.status.success()
                    && self.can_move_pack_storage(&parent)
                    && pack_write_out_of_space(&output.stderr, &self.dir)
                    && (!writes_fetch_head || matches!(fetched_head(&self.dir), Ok(None)));
            }
            if retry {
                // Free the failed object files before writing SSH's owner
                // record. The two service directories can share a tmpfs.
                drop(directory);
                if initial {
                    self.record_socket_owner()?;
                }
                self.use_run_pack_storage()?;
                initial = initial && self.socket_owner.is_none();
                continue;
            }
            if initial {
                self.record_socket_owner()?;
            }
            return Ok((output?, directory));
        }
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
        let materializing = self.materializing || refetch;
        let oid_argument;
        let argument = match revision {
            NativeRevision::Query(query) => query,
            NativeRevision::Selected(oid) => {
                oid_argument = oid.to_string();
                OsStr::new(&oid_argument)
            }
        };
        let (out, received) = self
            .run_native_fetch(initial, None, true, |remote, first| {
                let mut command = remote.command(first)?;
                command.args(["fetch", "--no-tags", "--depth=1"]);
                command.args(refetch.then_some("--refetch"));
                if materializing {
                    // Keep the received pack for final-path materialization.
                    command.args(["--keep", "--no-filter"]);
                } else {
                    command.arg("--filter=tree:0");
                }
                crate::terminal_ui::configure_fetch(&mut command);
                command
                    .arg("--")
                    .arg(&remote.source)
                    .arg(argument)
                    .stdin(Stdio::null());
                Ok(command)
            })
            .context("fetch selected remote revision")?;
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
        let oid =
            fetched_head(&self.dir)?.context("native Git fetch selected no object to mount")?;
        if let NativeRevision::Selected(expected) = revision {
            anyhow::ensure!(oid == expected, "remote changed the selected snapshot");
        }
        self.import_received(
            received,
            (!materializing).then_some(std::slice::from_ref(&oid)),
            true,
        )?;
        if !self.raw_blobs.available(oid)
            && self.selection_odb(oid).is_none()
            && !self.native_repository().unwrap().odb()?.exists(oid)
        {
            bail!("native Git fetch did not supply selected object {oid}");
        }
        self.completed_fetches += 1;
        Ok(oid)
    }
    pub(crate) fn fetch_copy_objects(&mut self, selected: Oid) -> Result<()> {
        self.materializing = true;
        // The revision has already been resolved and possibly verified.
        // Request that exact object again without the lazy fetch filter.
        if !self
            .git
            .fetch_settings_require_native_git(true, self.socket.is_some())
            && !self.git.has_nonstandard_fetch_mapping(&self.source)?
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

    pub(crate) fn finish_materialization(&mut self) -> Result<()> {
        self.finish_materialized_pack()?;
        self.selection_pack = None;
        Ok(())
    }

    fn finish_materialized_pack(&mut self) -> Result<()> {
        if let Some(pack) = self.active_materialized_pack.take() {
            if let Some(decoder) = &pack.decoder {
                decoder.finish()?;
            }
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
    ) -> Result<(u64, bool)> {
        let outcome = (|| {
            let (directory, size) = match self
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
                ReceivedObject::Packed { directory, size } => (directory, size),
            };
            if self
                .active_materialized_pack
                .as_ref()
                .is_none_or(|pack| !Arc::ptr_eq(&pack.directory, &directory))
            {
                self.finish_materialized_pack()?;
                self.active_materialized_pack = Some(ReceivedPack {
                    decoder: None,
                    odb: received_pack_database(directory.path())?,
                    directory: directory.clone(),
                });
            }
            drop(directory);
            let pack = self.active_materialized_pack.as_mut().unwrap();
            let kind = git2::ObjectType::Blob;
            let streamed = size > crate::raw_blob::BLOCK;
            if streamed {
                if pack.decoder.is_none() {
                    let mut command = self.git.command(Some(&self.dir));
                    command
                        .env("TMPDIR", &self.dir)
                        .env("GIT_OBJECT_DIRECTORY", pack.objects());
                    command.args([
                        "-c",
                        "core.bigFileThreshold=8m",
                        "cat-file",
                        "--batch-command",
                    ]);
                    command.env("GIT_ALLOW_PROTOCOL", "");
                    pack.decoder = Some(transport::BatchProcess::spawn(command)?);
                    self.decode_operations += 1;
                }
                let request = format!("contents {oid}\n");
                pack.decoder
                    .as_mut()
                    .unwrap()
                    .exchange(request.as_bytes(), |input| {
                        crate::git::receive_batch_object(
                            oid,
                            crate::git::BatchRead::Materialize,
                            input,
                            |reported_size, reported_kind, data| {
                                anyhow::ensure!(
                                    (reported_size, reported_kind) == (size, kind),
                                    "packed Git object header changed during transfer"
                                );
                                crate::object::copy_verified(data, output, kind, size, oid)
                            },
                        )
                    })??;
            } else {
                let object = pack.odb.read(oid)?;
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
                self.finish_materialized_pack()?;
            }
        }
        self.record_result(outcome)
    }
    pub fn ensure(&mut self, oid: Oid) -> Result<()> {
        self.ensure_object(oid, false)
    }

    pub(crate) fn ensure_object(&mut self, oid: Oid, is_tree: bool) -> Result<()> {
        let outcome = (|| {
            if self.raw_blobs.claim(oid)? || self.odb()?.is_some_and(|odb| odb.exists(oid)) {
                return Ok(());
            }
            match self.raw_blobs.received(oid) {
                Some(ReceivedObject::Verified { object, transfer }) => {
                    self.publish_verified(object, transfer)
                }
                Some(ReceivedObject::Packed { directory, .. }) if !self.materializing => {
                    self.import_packed_object(oid, directory.path())
                }
                Some(ReceivedObject::Packed { .. }) => Ok(()),
                None if self.selection_odb(oid).is_some() => Ok(()),
                None => self.fetch(&[oid], is_tree),
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
            self.fetch(&missing, false)
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
        is_tree: bool,
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
            let demand = wanted.then(|| pool.request(oid, is_tree)).transpose()?;
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
        let git = self.git.clone();
        let mut scratch = None;
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
                crate::http2::credentials(
                    git.command(None),
                    url,
                    challenges,
                    previous,
                    &mut scratch,
                )
            };
        let Some(configuration) = crate::http2::Configuration::new(
            &self.demand_url,
            &self.native_git_version,
            credential,
        ) else {
            return Ok(None);
        };
        open(configuration, &self.raw_blobs)
    }

    fn fetch_snapshot(&mut self, target: SnapshotTarget<'_>) -> Result<SnapshotPreparation> {
        let reference = target.reference();
        let selection = reference
            .cloned()
            .map_or(crate::http2::Selection::Exact, crate::http2::Selection::Ref);
        let https = self.with_http2_configuration(|configuration, scratch| {
            let ready = crate::http2::Client::negotiate(
                configuration,
                Default::default(),
                selection,
                transport::FetchMode::Snapshot {
                    selecting_ref: reference.is_some(),
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
            selecting_ref: target.reference().is_some(),
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
            selecting_ref: target.reference().is_some(),
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
            self.repo = Some(PrivateRepository::Context(self.create_native_repository()?));
        }
        let Some(command) = self.non_http_command(false)? else {
            return Ok(None);
        };
        let ready = match transport::StartingSession::upload_pack(command, None) {
            Ok(session) => session.negotiate_selection(target.reference(), mode),
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
            selecting_ref: target.reference().is_some(),
        };
        let ready = match transport::StartingSession::spawn(command, false, None) {
            Ok(session) => session.negotiate_selection(target.reference(), mode),
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
                        Some(ReceivedObject::Packed { directory, .. }) => {
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
    fn fetch(&mut self, oids: &[Oid], is_tree: bool) -> Result<()> {
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
                let batch = pool.request(*oid, is_tree)?.wait();
                return self.finish_demand(&batch);
            }
            if self.fetch_in_session(*oid)? {
                return Ok(());
            }
        }
        self.prepare_native_repository()?;
        let mut requests = Vec::new();
        if oids.len() != 1 {
            for oid in oids {
                writeln!(requests, "{oid}")?;
            }
        }
        let (out, received) = self
            .run_native_fetch(
                false,
                (!requests.is_empty()).then_some(requests.as_slice()),
                false,
                |remote, first| {
                    let mut command = remote.command(first)?;
                    command.args([
                        "fetch",
                        "--no-tags",
                        "--no-write-fetch-head",
                        "--depth=1",
                        "--filter=tree:0",
                    ]);
                    crate::terminal_ui::configure_fetch(&mut command);
                    if let [oid] = oids {
                        command
                            .arg("--")
                            .arg(&remote.source)
                            .arg(oid.to_string())
                            .stdin(Stdio::null());
                    } else {
                        command.arg("--stdin").arg("--").arg(&remote.source);
                    }
                    Ok(command)
                },
            )
            .context("fetch exact remote object")?;
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
        self.import_received(received, (!self.materializing).then_some(oids), false)?;
        let odb = self.native_repository().unwrap().odb()?;
        for oid in oids {
            if !self.raw_blobs.available(*oid) && !odb.exists(*oid) {
                bail!("filtered remote fetch did not supply requested object {oid}");
            }
        }
        drop(odb);
        self.completed_fetches += oids.len() as u64;
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
        reference: Option<&crate::transport::RefQuery>,
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
        if let Some(reference) = reference.filter(|_| !self.git.sets_protocol_version()) {
            return self.select_ssh_ref(reference, command, additional);
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
        let oid = match (exact, reference) {
            (Some(oid), _) => oid,
            (None, Some(reference)) => match session.select_ref(reference) {
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

    fn select_ssh_ref(
        &mut self,
        reference: &transport::RefQuery,
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
                let _ = send.send(starter.advertised_ref(reference).ok().flatten());
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
                                Self::resolve_ssh_ref(primary, &mut candidates, reference, receive);
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

    fn resolve_ssh_ref(
        mut primary: transport::Session,
        spares: &mut Vec<transport::StartingSession>,
        reference: &transport::RefQuery,
        advertisement: mpsc::Receiver<Option<Oid>>,
    ) -> Result<Option<(transport::Session, Oid)>> {
        match advertisement.try_recv() {
            Ok(Some(oid)) => return Ok(Some((primary, oid))),
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                return Ok(primary.select_ref(reference)?.map(|oid| (primary, oid)));
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
        primary.request_ref(reference)?;
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
        Ok(primary.read_ref(reference)?.map(|oid| (primary, oid)))
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
    fn plain_file_path(&self) -> Option<&OsStr> {
        let url = self.demand_url.as_bytes();
        if !url.windows(2).any(|part| part == b"::")
            && !url.windows(3).any(|part| part == b"://")
            && self.git.cwd.join(&self.demand_url).is_dir()
        {
            return Some(&self.demand_url);
        }
        let path = url.strip_prefix(b"file://")?;
        // Use Git's parser for escaping and host syntax, including brackets.
        (path.starts_with(b"/")
            && !path.starts_with(b"//")
            && !path.contains(&b'%')
            && !path.windows(2).any(|part| part == b"@["))
        .then_some(OsStr::from_bytes(path))
    }

    fn file_upload_pack(&self) -> Result<Option<Command>> {
        if let Some(path) = self.plain_file_path() {
            let mut command = crate::git::native_git_command();
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
        let mut command = crate::git::native_git_command();
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
    // The object directory belongs to this fetch alone. Keep its reader and
    // any indexed blobs alive until the snapshot has consumed them.
    fn import_received(
        &mut self,
        directory: tempfile::TempDir,
        wanted: Option<&[Oid]>,
        retain_selection: bool,
    ) -> Result<()> {
        let received = ReceivedPack {
            decoder: None,
            odb: received_pack_database(directory.path())?,
            directory: Arc::new(directory),
        };
        let destination_odb = self
            .repo
            .as_ref()
            .and_then(PrivateRepository::objects)
            .context("missing native Git object database")?
            .odb()?;
        let cache = &mut self.raw_blobs;
        let mut requests = Vec::new();
        let mut metadata = HashSet::new();
        let mut failure = None;
        let iteration = received.odb.foreach(|oid| {
            let outcome = (|| -> Result<()> {
                crate::object::check_transfer()?;
                if cache.get(*oid)?.is_some() {
                    return Ok(());
                }
                let (size, kind) = received.odb.read_header(*oid)?;
                if kind == git2::ObjectType::Blob {
                    cache.remember_packed(*oid, &received.directory, size);
                    if wanted.is_some_and(|wanted| wanted.contains(oid)) {
                        requests.push(*oid);
                    }
                } else if kind == git2::ObjectType::Tree {
                    let object = received.odb.read(*oid)?;
                    cache.insert_odb_tree(*oid, &object)?;
                } else if retain_selection {
                    metadata.insert(*oid);
                } else if !destination_odb.exists(*oid) {
                    let object = received.odb.read(*oid)?;
                    let mut writer = destination_odb.writer(size, kind)?;
                    crate::object::copy_verified(object.data(), &mut writer, kind, size, *oid)?;
                    anyhow::ensure!(writer.finalize()? == *oid, "received metadata OID changed");
                    cache.forget_received(*oid);
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
        let directory = received.directory.clone();
        drop(destination_odb);
        if retain_selection {
            // Reuse the received Git objects for selection and verification.
            // Materialization releases them before launching the command.
            self.selection_pack = Some(NativeSelection {
                pack: received,
                metadata,
            });
        } else {
            drop(received);
        }
        if !requests.is_empty() {
            self.decode_received(directory.path(), &requests)?;
        }
        Ok(())
    }
    fn import_packed_object(&mut self, oid: Oid, directory: &Path) -> Result<()> {
        self.decode_received(directory, &[oid])?;
        anyhow::ensure!(
            self.raw_blobs.get(oid)?.is_some() || self.odb()?.is_some_and(|odb| odb.exists(oid)),
            "received pack omitted requested object {oid}"
        );
        Ok(())
    }
    fn decode_received(&mut self, directory: &Path, requests: &[Oid]) -> Result<()> {
        let mut command = self.object_command();
        command
            .env("GIT_OBJECT_DIRECTORY", directory.join("objects"))
            .args([
                "-c",
                "core.bigFileThreshold=8m",
                "cat-file",
                "--batch",
                "--buffer",
            ]);
        let mut stdin = Vec::new();
        for oid in requests {
            writeln!(stdin, "{oid}")?;
        }
        let cache = &mut self.raw_blobs;
        let (output, omitted) =
            crate::git::run_with_output(command, Some(&mut stdin.as_slice()), move |input| {
                let mut input = BufReader::new(input);
                for &oid in requests {
                    crate::object::check_transfer()?;
                    if input.fill_buf()?.is_empty() {
                        return Ok(Some(oid));
                    }
                    crate::git::receive_batch_object(
                        oid,
                        crate::git::BatchRead::Materialize,
                        &mut input,
                        |size, kind, data| {
                            anyhow::ensure!(
                                kind == git2::ObjectType::Blob,
                                "requested object is not a blob"
                            );
                            cache.insert_fetched(oid, size, &mut *data, None)?;
                            // An overlapping fetch may already have published this OID.
                            // Consume its response before reading the next header.
                            while let length @ 1.. = data.fill_buf()?.len() {
                                data.consume(length);
                            }
                            Ok(())
                        },
                    )??;
                }
                anyhow::ensure!(
                    input.fill_buf()?.is_empty(),
                    "native decoder sent an extra object"
                );
                Ok(None)
            })
            .context("decode received objects into run-local raw storage")?;
        if !output.status.success() {
            bail!(
                "cannot decode received Git objects: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if let Some(oid) = omitted {
            bail!("received pack omitted requested object {oid}");
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
    /// Release the owned master even when the run directory must remain for recovery.
    pub fn stop_transport(&self) -> Result<()> {
        if let Some(decoder) = self
            .active_materialized_pack
            .as_ref()
            .and_then(|pack| pack.decoder.as_ref())
        {
            decoder.stop()?;
        }
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
            if let Some(storage) = &mut self.ssh_storage {
                storage.keep_for_owner();
            }
            let _drawing = crate::terminal_ui::native_terminal_lease(true);
            let _ = writeln!(
                std::io::stderr().lock(),
                "git-on: owned remote transport shutdown failed: {error:#}"
            );
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

fn fetched_head(repository: &Path) -> Result<Option<Oid>> {
    let head = match fs::read(repository.join("FETCH_HEAD")) {
        Ok(head) => head,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read native Git fetch result"),
    };
    anyhow::ensure!(
        head.is_empty() || head.ends_with(b"\n"),
        "incomplete native Git fetch result"
    );
    let mut selected = None;
    for line in head
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let oid = line.split(|byte| *byte == b'\t').next().unwrap();
        anyhow::ensure!(oid.len() == 40, "native Git fetched a non-SHA-1 object");
        let oid =
            Oid::from_str(std::str::from_utf8(oid)?).context("read native Git selected OID")?;
        anyhow::ensure!(
            selected.is_none_or(|selected| selected == oid),
            "native Git fetch selected multiple objects; a snapshot requires exactly one selected object"
        );
        selected = Some(oid);
    }
    Ok(selected)
}

fn pack_write_out_of_space(stderr: &[u8], repository: &Path) -> bool {
    let repository = repository.as_os_str().as_bytes();
    let failure_lines = || {
        stderr.split(|byte| *byte == b'\n').filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            (!line.starts_with(b"remote: ")
                && !line.first().is_some_and(u8::is_ascii_digit)
                && !line
                    .windows(repository.len())
                    .any(|path| path == repository))
            .then_some(line)
        })
    };
    let matches_errno = |message: &[u8]| {
        !message.is_empty()
            && failure_lines().any(|line| {
                line.strip_suffix(message)
                    .is_some_and(|line| line.ends_with(b": "))
            })
    };
    if matches_errno(b"No space left on device")
        || failure_lines().any(|line| line.ends_with(b"write error. Out of diskspace"))
    {
        return true;
    }
    // Native Git reports strerror(ENOSPC). Match the caller's message
    // locale without changing Git's environment or other threads.
    let locale =
        unsafe { libc::newlocale(libc::LC_MESSAGES_MASK, c"".as_ptr(), std::ptr::null_mut()) };
    if locale.is_null() {
        return false;
    }
    let previous = unsafe { libc::uselocale(locale) };
    let result = if previous.is_null() {
        false
    } else {
        let error = std::io::Error::from_raw_os_error(libc::ENOSPC).to_string();
        unsafe { libc::uselocale(previous) };
        matches_errno(
            error
                .split_once(" (os error ")
                .map_or(error.as_str(), |(message, _)| message)
                .as_bytes(),
        )
    };
    unsafe { libc::freelocale(locale) };
    result
}

fn received_pack_directory(parent: &Path) -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("received-pack-")
        .tempdir_in(parent)?;
    fs::create_dir_all(directory.path().join("objects/pack"))?;
    Ok(directory)
}

fn git_alternate_path(path: &Path) -> OsString {
    // Git parses quoted alternate paths before splitting on colons.
    let path = path.as_os_str().as_bytes();
    let mut quoted = Vec::with_capacity(path.len() + 2);
    quoted.push(b'"');
    for &byte in path {
        if matches!(byte, b'"' | b'\\') {
            quoted.push(b'\\');
        }
        quoted.push(byte);
    }
    quoted.push(b'"');
    OsString::from_vec(quoted)
}

fn received_pack_bytes(directory: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in fs::read_dir(directory.join("objects/pack"))? {
        crate::object::check_transfer()?;
        let entry = entry?;
        if (entry
            .path()
            .extension()
            .is_some_and(|suffix| suffix == "pack")
            || entry.file_name().as_bytes().starts_with(b"tmp_pack_"))
            && entry.file_type()?.is_file()
        {
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(bytes)
}

fn received_pack_database(directory: &Path) -> Result<Odb<'static>> {
    let path = directory.join("objects").into_c_string()?;
    let odb = Odb::new()?;
    // SAFETY: both arguments stay alive through this call. libgit2 copies
    // the byte path into the alternate backend it attaches to the ODB.
    let status = unsafe { libgit2_sys::git_odb_add_disk_alternate(odb.raw(), path.as_ptr()) };
    if status < 0 {
        return Err(git2::Error::last_error(status)).context("open received pack object directory");
    }
    Ok(odb)
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
        # Mux proxy mode requires the existing socket, including with -J.
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
