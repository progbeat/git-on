//! Explicit exact-object fetches. Git's promisor auto-fetch is never the loader.
use crate::{git::Git, pack_stream, transport};
use anyhow::{bail, Context, Result};
use git2::{Oid, Repository};
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
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub const SSH_CONTROL_OWNER_FILE: &str = "ssh-control-owner";
const SSH_STANDBY_DELAY: Duration = Duration::from_millis(250);
const SSH_STANDBY_CHANNELS: usize = 2;

fn require_native_git(cwd: &Path) -> Result<()> {
    let mut command = Command::new("git");
    crate::git::read_only(&mut command)
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
    run: PathBuf,
    https_alternative: Option<String>,
    completed_fetches: u64,
    session_fetch_operations: u64,
    session_decode_operations: u64,
    session_starts: u64,
    session_received_pack_bytes: u64,
    eager_http2_fetches: u64,
    session: Option<crate::transport::Session>,
    forecast_seen: HashSet<Oid>,
    demand_pool: Option<crate::demand::Pool>,
    pub(crate) response_completion: Arc<crate::demand::ResponseCompletion>,
    demand_url: OsString,
    socket: Option<PathBuf>,
    socket_owner: Option<SocketOwner>,
    ssh: Option<OsString>,
    ssh_directory: PathBuf,
    control_client: Option<ControlClient>,
    failed: Option<String>,
    eager_loaded: bool,
    pub(crate) raw_blobs: crate::raw_blob::RawBlobs,
    #[cfg(feature = "test-metrics")]
    metadata_oids: HashSet<Oid>,
}
pub(crate) enum Source {
    Local(PathBuf),
    Network { git: Git, url: OsString },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SshMode {
    Copy,
    Master,
    Channel,
}

impl SshMode {
    fn name(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Master => "master",
            Self::Channel => "channel",
        }
    }
}

pub(crate) fn resolve_source(cwd: &Path, source: &OsStr) -> Result<Source> {
    let mut git = Git::capture(cwd).map_err(|error| {
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
    let named = git.select_remote(source);
    let url = git.source_url(source)?;
    let url = url.strip_suffix(b"\n").unwrap_or(&url);
    let path = if let Some(file) = url.strip_prefix(b"file://") {
        let start = file
            .iter()
            .position(|&byte| byte == b'/')
            .context("file URL requires an absolute repository path")?;
        decode_url_path(&file[start..])?
    } else {
        let path = Path::new(OsStr::from_bytes(url));
        let leading_segment = url.split(|&byte| byte == b'/').next().unwrap();
        if leading_segment.contains(&b':') && !cwd.join(path).is_dir() {
            return Ok(Source::Network {
                git,
                url: path.as_os_str().to_os_string(),
            });
        }
        anyhow::ensure!(!url.is_empty(), "missing local repository path");
        url.to_vec()
    };
    let worktree = if named && Path::new(OsStr::from_bytes(&path)).is_relative() {
        git.worktree_root()?
    } else {
        None
    };
    // Native Git uses the invocation directory when there is no worktree.
    let base = worktree.as_deref().unwrap_or(cwd);
    let path = if path.starts_with(b"~") && !base.join(OsStr::from_bytes(&path)).exists() {
        let mut entry = b"git-on.sourcePath=".to_vec();
        entry.extend(path);
        let path = git.output(
            None,
            &[
                OsStr::new("-c"),
                OsStr::from_bytes(&entry),
                OsStr::new("config"),
                OsStr::new("--path"),
                OsStr::new("--get"),
                OsStr::new("git-on.sourcePath"),
            ],
        )?;
        path.strip_suffix(b"\n").unwrap_or(&path).to_vec()
    } else {
        path
    };
    let path = Path::new(OsStr::from_bytes(&path));
    let path: PathBuf = base.join(path).components().collect();
    let path = if !path.exists() {
        let mut suffixed = path.as_os_str().to_os_string();
        suffixed.push(".git");
        let suffixed = PathBuf::from(suffixed);
        if suffixed.exists() {
            suffixed
        } else {
            path
        }
    } else {
        path
    };
    let path = if path.is_file() {
        let resolved = git.output(
            None,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--resolve-git-dir"),
                path.as_os_str(),
            ],
        )?;
        cwd.join(OsStr::from_bytes(
            resolved.strip_suffix(b"\n").unwrap_or(&resolved),
        ))
    } else {
        path
    };
    anyhow::ensure!(
        path.is_dir(),
        "local repository path is unavailable: {}",
        path.display()
    );
    Ok(Source::Local(path))
}

fn decode_url_path(mut bytes: &[u8]) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(bytes.len());
    while let Some((&byte, tail)) = bytes.split_first() {
        if byte == b'%' && tail.len() >= 2 {
            if let (Some(high), Some(low)) = (
                char::from(tail[0]).to_digit(16),
                char::from(tail[1]).to_digit(16),
            ) {
                let value = (high * 16 + low) as u8;
                anyhow::ensure!(value != 0, "repository URL contains a NUL byte");
                result.push(value);
                bytes = &tail[2..];
                continue;
            }
        }
        result.push(byte);
        bytes = tail;
    }
    Ok(result)
}

fn scp_separator(bytes: &[u8]) -> Option<usize> {
    let mut bracketed = false;
    let boundary = bytes.iter().position(|&byte| {
        match byte {
            b'[' => bracketed = true,
            b']' => bracketed = false,
            _ => (),
        }
        byte == b'/' || byte == b':' && !bracketed
    })?;
    let path = &bytes[boundary + 1..];
    (bytes[boundary] == b':' && !path.starts_with(b"//") && !path.starts_with(b":"))
        .then_some(boundary)
}

fn ssh_target(url: &OsStr) -> Result<(OsString, Option<u16>, Vec<u8>)> {
    let bytes = url.as_bytes();
    let (endpoint, port, path) = if let Some(uri) = bytes.strip_prefix(b"ssh://") {
        let slash = uri
            .iter()
            .position(|&byte| byte == b'/')
            .context("SSH URL requires a repository path")?;
        // Keep Git's original path. URL normalization must not remove a
        // parent component on the far side of a symlink.
        let authority = std::str::from_utf8(&uri[..slash])?;
        let parsed = url::Url::parse(&format!("ssh://{authority}/"))?;
        anyhow::ensure!(
            parsed.password().is_none(),
            "SSH URLs cannot include a password"
        );
        let host = parsed.host_str().context("SSH URL requires a host")?;
        let mut endpoint = decode_url_path(parsed.username().as_bytes())?;
        if !endpoint.is_empty() {
            endpoint.push(b'@');
        }
        endpoint.extend_from_slice(host.as_bytes());
        let path = decode_url_path(&uri[slash..])?;
        let path = if path.starts_with(b"/~") {
            path[1..].to_vec()
        } else {
            path
        };
        (endpoint, parsed.port(), path)
    } else {
        let separator = scp_separator(bytes).context("SSH source requires a host and path")?;
        (
            bytes[..separator].to_vec(),
            None,
            bytes[separator + 1..].to_vec(),
        )
    };
    let host_start = endpoint
        .iter()
        .rposition(|&byte| byte == b'@')
        .map_or(0, |index| index + 1);
    let host = &endpoint[host_start..];
    let host = host
        .strip_prefix(b"[")
        .and_then(|host| host.strip_suffix(b"]"))
        .unwrap_or(host);
    anyhow::ensure!(
        !host.is_empty() && !path.is_empty(),
        "SSH source requires a host and path"
    );
    let mut arguments = endpoint[..host_start].to_vec();
    arguments.extend_from_slice(host);
    anyhow::ensure!(
        !arguments.starts_with(b"-"),
        "OpenSSH host cannot be an option"
    );
    Ok((OsString::from_vec(arguments), port, path))
}

fn selection(revision: &OsStr) -> Result<(Option<Oid>, Option<transport::BranchQuery>)> {
    let exact = revision
        .to_str()
        .filter(|revision| revision.len() == 40)
        .and_then(|revision| Oid::from_str(revision).ok());
    let branch = if exact.is_none() {
        transport::BranchQuery::new(revision)
    } else {
        None
    };
    anyhow::ensure!(
        exact.is_some() || branch.is_some(),
        "select a single remote ref or full object ID"
    );
    Ok((exact, branch))
}

fn memory_repository() -> Result<Repository> {
    let odb = git2::Odb::new()?;
    odb.add_new_mempack_backend(100)?;
    Ok(Repository::from_odb(odb)?)
}

impl Remote {
    pub(crate) fn open_for_execution(
        git: Git,
        url: OsString,
        revision: &OsStr,
        run: &Path,
        materialized: bool,
    ) -> Result<(Self, Repository, Oid)> {
        let cwd = &git.cwd;
        require_native_git(cwd)?;
        let text = url.to_string_lossy();
        let ssh_transport = text.starts_with("ssh://") || scp_separator(url.as_bytes()).is_some();
        anyhow::ensure!(
            ssh_transport || text.starts_with("https://"),
            "remote transport must be direct HTTPS or OpenSSH"
        );
        anyhow::ensure!(
            git.demand_session_compatible()?,
            "Git configuration requires a transport outside the Git protocol v2 profile"
        );
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
        let ssh_directory = if ssh_transport {
            git.transport_directory()?
        } else {
            cwd.clone()
        };
        let control_client = ssh
            .as_deref()
            .map(|ssh| ControlClient::resolve(&ssh_directory, ssh))
            .transpose()?;
        let mut remote = Self {
            git,
            run: run.to_path_buf(),
            https_alternative: github_https_alternative(&text),
            completed_fetches: 0,
            session_fetch_operations: 0,
            session_decode_operations: 0,
            session_starts: 0,
            session_received_pack_bytes: 0,
            eager_http2_fetches: 0,
            session: None,
            forecast_seen: HashSet::new(),
            demand_pool: None,
            response_completion: Arc::default(),
            demand_url: url,
            socket,
            socket_owner: None,
            ssh,
            ssh_directory,
            control_client,
            failed: None,
            eager_loaded: false,
            raw_blobs: crate::raw_blob::RawBlobs::new(run)?,
            #[cfg(feature = "test-metrics")]
            metadata_oids: HashSet::new(),
        };
        let (exact, branch) = selection(revision)?;
        if materialized {
            let (repo, oid) = remote
                .fetch_eager(exact, branch.as_ref())?
                .context("copy requires direct Git v2 and a single available revision")?;
            return Ok((remote, repo, oid));
        }
        let repo = memory_repository()?;
        let oid = if ssh_transport {
            remote
                .select_ssh(&repo, exact, branch.as_ref())
                .map_err(|error| remote.ssh_error(error))?
                .context("OpenSSH could not select a revision using filtered Git protocol v2")?
        } else {
            let selected = remote.open_http2_pool(branch)?;
            let oid = exact
                .or(selected)
                .context("remote did not select a revision")?;
            remote.fetch(&repo, &[oid])?;
            oid
        };
        Ok((remote, repo, oid))
    }
    fn ssh_error(&self, error: anyhow::Error) -> anyhow::Error {
        let advice =
            authentication_advice(&format!("{error:#}"), self.https_alternative.as_deref());
        if advice.is_empty() {
            error
        } else {
            anyhow::anyhow!("{error:#}{advice}")
        }
    }

    fn command(&self, mode: SshMode) -> Result<Command> {
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        let mut command = self.git.command(None);
        if let (Some(socket), Some(ssh)) = (&self.socket, &self.ssh) {
            // Git appends argv to the outer function, so its argument count
            // gives the inner function a structural user/Git boundary. User
            // shell expansion cannot impersonate that boundary. Remove short
            // -M (which bypasses first-value -o precedence) and place the owned
            // -S after user options and before Git's host.
            let mut text = OsString::from(SSH_WRAPPER);
            text.push("; git_on_ssh() { git_on_git_argc=$#; git_on_ssh_command ");
            text.push(ssh);
            text.push(" \"$@\"; }; git_on_ssh");
            command
                .env("GIT_ON_SSH_CONTROL_PATH", socket)
                .env("GIT_ON_SSH_MODE", mode.name());
            command
                .env("GIT_SSH_COMMAND", text)
                .env("GIT_SSH_VARIANT", "ssh");
        }
        if mode == SshMode::Channel
            && !(crate::terminal_ui::preparing() && crate::terminal_ui::owns_terminal())
        {
            command
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "/bin/false")
                .env("SSH_ASKPASS", "/bin/false");
        }
        Ok(command)
    }
    fn fetch_eager(
        &mut self,
        exact: Option<Oid>,
        branch: Option<&transport::BranchQuery>,
    ) -> Result<Option<(Repository, Oid)>> {
        self.raw_blobs
            .enable_copy_cache(crate::raw_blob::COPY_MEMORY_BUDGET);
        let selection = if self.socket.is_some() {
            self.fetch_eager_ssh(exact, branch)
                .map_err(|error| self.ssh_error(error))?
        } else {
            self.fetch_eager_http2(exact, branch)?
        };
        let Some((oid, pack)) = selection else {
            return Ok(None);
        };
        let repo = memory_repository()?;
        for object in pack.objects.into_values() {
            crate::object::check_transfer()?;
            self.publish_received(&repo, object.decode()?)?;
        }
        self.eager_loaded = true;
        self.eager_http2_fetches += u64::from(self.socket.is_none());
        self.session_received_pack_bytes += pack.bytes;
        self.session_starts += 1;
        self.session_fetch_operations += 1;
        self.session_decode_operations += 1;
        self.completed_fetches += 1;
        Ok(Some((repo, oid)))
    }

    pub(crate) fn prepare_copy(&mut self, oid: Oid) -> Result<Repository> {
        self.stop_demand_channels()?;
        let (repo, _) = self
            .fetch_eager(Some(oid), None)?
            .context("cannot materialize the pinned remote revision")?;
        Ok(repo)
    }

    fn fetch_eager_http2(
        &self,
        exact: Option<Oid>,
        branch: Option<&transport::BranchQuery>,
    ) -> Result<Option<(Oid, crate::pack_stream::Received)>> {
        let configuration = crate::http2::Configuration::new(&self.demand_url, &self.git)?;
        let Some((mut client, selected)) =
            crate::http2::Client::negotiate(configuration, Default::default(), branch)?
        else {
            return Ok(None);
        };
        let oid = exact
            .or(selected)
            .context("remote did not select a revision")?;
        let pack = client.fetch_eager(oid, self.raw_blobs.eager_response(oid))?;
        Ok(Some((oid, pack)))
    }

    fn fetch_eager_ssh(
        &self,
        exact: Option<Oid>,
        branch: Option<&transport::BranchQuery>,
    ) -> Result<Option<(Oid, crate::pack_stream::Received)>> {
        if !self.git.transport_allowed("ssh") {
            return Ok(None);
        }
        let mut starting = if let Some(socket) = self.ssh_socket() {
            let command = self.ssh_command(SshMode::Channel)?;
            transport::Connection::new(command, socket).start_additional()?
        } else {
            transport::StartingSession::spawn_foreground(self.ssh_command(SshMode::Copy)?)?
        };
        starting.queue_selection(exact, branch, true)?;
        let Some(mut session) = starting.negotiate()? else {
            return Ok(None);
        };
        let selected = if let Some(query) = branch {
            session.select_branch(query)?
        } else {
            None
        };
        let oid = exact
            .or(selected)
            .context("remote did not select a revision")?;
        let pack = session.fetch_eager(oid, self.raw_blobs.eager_response(oid))?;
        Ok(Some((oid, pack)))
    }

    pub(crate) fn verify(&self, repo: &Repository, oid: Oid, kind: git2::ObjectType) -> Result<()> {
        self.git.verify(oid, kind, Some((repo, &self.run)))
    }

    pub(crate) fn eager_loaded(&self) -> bool {
        self.eager_loaded
    }
    pub(crate) fn ensure_in(&mut self, repo: &Repository, oid: Oid) -> Result<()> {
        self.ensure_batch_in(repo, &[oid])
    }

    pub(crate) fn ensure_batch_in(&mut self, repo: &Repository, oids: &[Oid]) -> Result<()> {
        let outcome = (|| {
            let odb = repo.odb()?;
            let mut missing = Vec::new();
            for &oid in oids {
                if self.raw_blobs.get(oid)?.is_some() || odb.exists(oid) {
                    if let Some(pool) = &self.demand_pool {
                        pool.observe_cached(oid);
                    }
                    continue;
                }
                if let Some(object) = self.raw_blobs.packed_object(oid) {
                    self.publish_received(repo, object.decode()?)?;
                } else {
                    missing.push(oid);
                }
            }
            if missing.is_empty() {
                return Ok(());
            }
            anyhow::ensure!(!self.eager_loaded, "eager pack omitted {}", missing[0]);
            self.fetch(repo, &missing)
        })();
        self.record_result(outcome)
    }

    pub(crate) fn reusable_transport(&self) -> bool {
        self.demand_pool.is_some() || self.session.is_some()
    }

    pub(crate) fn start_demand<I: Iterator<Item = Oid>>(
        &mut self,
        repo: &Repository,
        oid: Oid,
        candidates: Option<impl FnOnce(bool) -> I>,
    ) -> Result<Option<crate::demand::Demand>> {
        let outcome = (|| {
            if !self.reusable_transport() || self.failure().is_some() {
                return Ok(None);
            }
            let odb = repo.odb()?;
            let wanted = !self.raw_blobs.available(oid) && !odb.exists(oid);
            if !wanted && candidates.is_none() {
                return Ok(None);
            }
            self.prepare_pool()?;
            let Some(pool) = &self.demand_pool else {
                return Ok(None);
            };
            let demand = wanted.then(|| pool.request(oid)).transpose()?;
            if let Some(candidates) = candidates.filter(|_| self.forecast_seen.insert(oid)) {
                pool.prefetch(oid, candidates, |oid| odb.exists(oid));
            }
            Ok(demand)
        })();
        self.record_result(outcome)
    }

    fn prepare_pool(&mut self) -> Result<()> {
        if self.demand_pool.is_none() && !crate::terminal_ui::preparing() {
            self.open_ssh_pool()?;
        }
        Ok(())
    }

    fn open_ssh_pool(&mut self) -> Result<()> {
        let Some(session) = self.session.take() else {
            return Ok(());
        };
        let command = self.ssh_command(SshMode::Channel)?;
        let socket = self
            .ssh_socket()
            .context("missing owned OpenSSH connection")?;
        let connection = transport::Connection::new(command, socket);
        self.demand_pool = Some(crate::demand::Pool::new(
            session,
            connection,
            &self.raw_blobs,
            &self.response_completion,
        )?);
        Ok(())
    }

    fn open_http2_pool(
        &mut self,
        branch: Option<crate::transport::BranchQuery>,
    ) -> Result<Option<Oid>> {
        let configuration = crate::http2::Configuration::new(&self.demand_url, &self.git)?;
        let (pool, selected) = crate::demand::Pool::http2(
            configuration,
            branch,
            &self.raw_blobs,
            &self.response_completion,
        )?
        .context("HTTPS requires filtered Git protocol v2 over HTTP/2")?;
        self.demand_pool = Some(pool);
        self.session_starts += 1;
        Ok(selected)
    }

    pub(crate) fn finish_demand(
        &mut self,
        repo: &Repository,
        batch: &crate::demand::Batch,
    ) -> Result<()> {
        batch.publish(|acquired| {
            let outcome = acquired.and_then(|acquired| match acquired {
                crate::demand::Acquired::Stored => Ok(()),
                crate::demand::Acquired::Packed(object) => {
                    self.publish_received(repo, object.decode()?)
                }
                crate::demand::Acquired::Object(object) => self.publish_received(repo, object),
            });
            self.demand_pool.as_ref().unwrap().retire(&batch.oids);
            self.record_result(outcome)
        })
    }
    fn record_result<T>(&mut self, outcome: Result<T>) -> Result<T> {
        if let Err(error) = &outcome {
            // ODB failures are just as incomplete as a failed fetch. Keep
            // the original failure even when later demands retry that operation.
            if !crate::object::is_cancelled(error)
                && !crate::raw_blob::is_retryable_spill(error)
                && self.failed.is_none()
                && self.raw_blobs.failure().is_none()
            {
                self.failed = Some(format!("{error:#}"));
                let _drawing = crate::terminal_ui::native_terminal_lease(true);
                eprintln!("git-on: remote object loading failed: {error:#}");
            }
        }
        outcome
    }
    fn fetch(&mut self, repo: &Repository, oids: &[Oid]) -> Result<()> {
        if let Some(error) = self.failure() {
            bail!("remote object transport previously failed: {error}");
        }
        if oids.len() > 1 && self.demand_pool.is_none() {
            self.open_ssh_pool()?;
        }
        if let Some(pool) = &self.demand_pool {
            let pending: Vec<_> = oids
                .iter()
                .map(|&oid| pool.request(oid))
                .collect::<Result<_>>()?;
            for request in pending {
                let batch = request.wait(&self.response_completion);
                self.finish_demand(repo, &batch)?;
            }
        } else {
            for &oid in oids {
                self.fetch_in_session(repo, oid)?;
            }
        }
        Ok(())
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

    fn select_ssh(
        &mut self,
        repo: &Repository,
        exact: Option<Oid>,
        branch: Option<&crate::transport::BranchQuery>,
    ) -> Result<Option<Oid>> {
        if !self.git.transport_allowed("ssh") {
            return Ok(None);
        }
        let command = self.ssh_command(SshMode::Master)?;
        let started = Instant::now();
        let mut additional = transport::copy_command(&command);
        additional
            .env("GIT_ON_SSH_MODE", SshMode::Channel.name())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false");
        let mut candidates = vec![transport::StartingSession::spawn_foreground(command)?];
        let mut connection = None;
        let session = (|| -> Result<_> {
            if let Some(primary) = candidates.first_mut() {
                // Queue the first request while the server advertises v2.
                primary.queue_selection(exact, branch, false)?;
            }
            let mut failure = None;
            while !candidates.is_empty() {
                if let Some(index) =
                    transport::StartingSession::wait_for_advertisement(&mut candidates)?
                {
                    match candidates.swap_remove(index).negotiate() {
                        Ok(Some(session)) => return Ok(Some(session)),
                        Ok(None) => (),
                        Err(error) => failure = Some(error),
                    }
                } else if connection.is_none() && started.elapsed() >= SSH_STANDBY_DELAY {
                    self.record_socket_owner()?;
                    if let Some(socket) = self.ssh_socket() {
                        let owned = transport::Connection::new(
                            transport::copy_command(&additional),
                            socket,
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
            failure.map_or(Ok(None), Err)
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
        // A candidate with an outstanding selection is not a clean demand
        // channel if another candidate won startup.
        candidates.retain(|candidate| !candidate.has_queued_selection());
        self.fetch_selected_ssh(repo, oid, session, connection, candidates)
    }

    fn fetch_selected_ssh(
        &mut self,
        repo: &Repository,
        oid: Oid,
        session: transport::Session,
        connection: Option<transport::Connection>,
        mut candidates: Vec<transport::StartingSession>,
    ) -> Result<Option<Oid>> {
        self.session = Some(session);
        self.session_starts += 1;
        let fetch_started = Instant::now();
        self.fetch_in_session(repo, oid)?;
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
            let mut pool = crate::demand::Pool::prepare_native(
                &connection,
                &self.raw_blobs,
                &self.response_completion,
                candidates,
            )?;
            let primary = self
                .session
                .take()
                .context("missing selected SSH channel")?;
            pool.attach_primary(primary, connection)?;
            self.demand_pool = Some(pool);
        }
        Ok(Some(oid))
    }

    fn ssh_command(&self, mode: SshMode) -> Result<Command> {
        let (host, port, path) = ssh_target(&self.demand_url)?;
        let native = self.command(mode)?;
        let ssh = native
            .get_envs()
            .find_map(|(key, value)| (key == "GIT_SSH_COMMAND").then_some(value).flatten())
            .context("no OpenSSH command configured")?;
        let mut script = ssh.to_os_string();
        script.push(" \"$@\"");
        let mut command = Command::new("/bin/sh");
        command.args([OsStr::new("-c"), &script, OsStr::new("git-on-ssh")]);
        command.args(["-o", "SendEnv=GIT_PROTOCOL"]);
        if let Some(port) = port {
            command.arg("-p").arg(port.to_string());
        }
        command.arg(host);
        let mut remote = b"git-upload-pack ".to_vec();
        remote.extend_from_slice(&gix_quote::single(path.as_slice().into()));
        command.arg(OsString::from_vec(remote));
        command.current_dir(&self.ssh_directory);
        for (key, value) in native.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        // Match native Git's local transport child environment for this exact
        // installed Git version, including config/object-location overrides.
        let local = self.git.output(
            None,
            &[OsStr::new("rev-parse"), OsStr::new("--local-env-vars")],
        )?;
        for key in local
            .split(|byte| *byte == b'\n')
            .filter(|key| !key.is_empty())
        {
            command.env_remove(OsStr::from_bytes(key));
        }
        command.env("GIT_PROTOCOL", "version=2");
        Ok(command)
    }
    fn fetch_in_session(&mut self, repo: &Repository, oid: Oid) -> Result<()> {
        let ready = Arc::new(std::sync::Mutex::new(None));
        let received = ready.clone();
        let response = pack_stream::Response::new(
            vec![oid],
            Box::new(move |object| {
                if object.kind() != git2::ObjectType::Blob {
                    *received.lock().unwrap() = Some(object);
                }
                Ok(())
            }),
        )
        .with_output(self.raw_blobs.pack_output());
        let session = self
            .session
            .as_mut()
            .context("no open Git v2 demand channel")?;
        let mut pack = session.fetch_stream(&[oid], response)?;
        if let Some(error) = pack.object_errors.remove(&oid) {
            return Err(error);
        }
        if let Some(object) = ready.lock().unwrap().take() {
            self.publish_received(repo, object)?;
        }
        for object in pack.objects.into_values() {
            self.raw_blobs.remember_packed(object);
        }
        self.completed_fetches += 1;
        self.session_fetch_operations += 1;
        self.session_decode_operations += 1;
        self.session_received_pack_bytes += pack.bytes;
        Ok(())
    }

    #[cfg(feature = "test-metrics")]
    pub(crate) fn received_oids(&self) -> impl Iterator<Item = Oid> + '_ {
        self.raw_blobs
            .packed_oids()
            .into_iter()
            .chain(self.metadata_oids.iter().copied())
    }

    fn retain_metadata(
        &mut self,
        odb: &git2::Odb<'_>,
        object: &pack_stream::VerifiedObject,
    ) -> Result<()> {
        anyhow::ensure!(
            odb.write(object.kind(), object.data())? == object.oid(),
            "received metadata OID changed"
        );
        #[cfg(feature = "test-metrics")]
        self.metadata_oids.insert(object.oid());
        Ok(())
    }

    fn publish_received(
        &mut self,
        repo: &Repository,
        object: pack_stream::VerifiedObject,
    ) -> Result<()> {
        let oid = object.oid();
        match object.kind() {
            git2::ObjectType::Blob => self.raw_blobs.insert_verified_blob(&object)?,
            git2::ObjectType::Tree => self.raw_blobs.insert_verified_tree(&object)?,
            _ => self.retain_metadata(&repo.odb()?, &object)?,
        }
        self.raw_blobs.forget_packed(oid);
        Ok(())
    }
    fn stop_demand_channels(&self) -> Result<()> {
        if let Some(pool) = &self.demand_pool {
            pool.stop()?;
        }
        if let Some(session) = &self.session {
            session.stop()?;
        }
        Ok(())
    }

    pub fn stop_transport(&self) -> Result<()> {
        self.stop_demand_channels()?;
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
        self.failed
            .clone()
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
        (pool.streamed_packs, pool.prefetches, pool.prefetched_blobs)
    }
    pub(crate) fn fetch_operations(&self) -> u64 {
        0
    }
    pub(crate) fn omitted_wants(&self) -> u64 {
        self.pool_metrics().omitted_wants
    }
    pub(crate) fn decode_operations(&self) -> u64 {
        0
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
            0,
            self.session_received_pack_bytes + self.pool_metrics().bytes,
        )
    }
    pub(crate) fn http2_metrics(&self) -> (u64, u64) {
        let (demands, peak) = self
            .demand_pool
            .as_ref()
            .map_or((0, 0), crate::demand::Pool::http2_metrics);
        (
            demands + self.eager_http2_fetches,
            peak.max(u64::from(self.eager_http2_fetches != 0)),
        )
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
    remaining=$(($# - git_on_git_argc)); next_option_arg=
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1)); drop=0
        if [ -n "$next_option_arg" ]; then
            if [ "$next_option_arg" = drop ]; then drop=1; fi
            next_option_arg=
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
                            E|O)
                                echo 'git-on: SSH control commands (-O) and log files (-E) are unsupported' >&2
                                exit 125 ;;
                            B|b|c|D|e|F|I|i|J|L|l|m|o|p|P|Q|R|S|W|w)
                                if [ "$flag" = J ] && [ "$GIT_ON_SSH_MODE" = channel ]; then
                                    # The jump has already been applied to our master.
                                    if [ -z "$flags" ]; then next_option_arg=drop; fi
                                else
                                    kept=$kept$flag$flags
                                    if [ -z "$flags" ]; then next_option_arg=keep; fi
                                fi
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
    if [ -n "$next_option_arg" ]; then
        echo 'git-on: SSH option is missing its argument' >&2
        exit 125
    fi
    control_path=$GIT_ON_SSH_CONTROL_PATH
    master=no
    persist=no
    case "$GIT_ON_SSH_MODE" in
        copy) control_path=none ;;
        master) master=auto; persist=yes ;;
    esac
    set -- "$@" -S "$control_path"
    remaining=$git_on_git_argc
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift; remaining=$((remaining - 1))
        set -- "$@" "$argument"
    done
    if [ "$GIT_ON_SSH_MODE" = channel ]; then
        # Ask the master to open the session. A rejected channel in raw mux
        # proxy mode can disconnect the master. The false proxy forbids
        # a replacement connection if ordinary session multiplexing fails.
        exec "$executable" -o "ControlPath=$control_path" -o ControlMaster=no -o BatchMode=yes -o ProxyCommand=/usr/bin/false "$@"
    fi
    exec "$executable" -o "ControlPath=$control_path" -o "ControlMaster=$master" -o "ControlPersist=$persist" -o StrictHostKeyChecking=yes -o UpdateHostKeys=no -o AddKeysToAgent=no -o PermitLocalCommand=no -o ClearAllForwardings=yes "$@"
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scp_transport_locates_the_separator_after_a_bracketed_host() {
        let url = OsStr::new("git@[2001:db8::1]:owner/repo.git");
        assert_eq!(scp_separator(url.as_bytes()), Some(17));
        let (host, port, path) = ssh_target(url).unwrap();
        assert_eq!(host, "git@2001:db8::1");
        assert_eq!(port, None);
        assert_eq!(path, b"owner/repo.git");
        assert_eq!(scp_separator(b"ext::example"), None);
        assert_eq!(scp_separator(b"https://example.invalid/a.git"), None);
    }
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
