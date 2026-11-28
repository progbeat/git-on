//! Native Git configuration and signature policy. No replacement objects.
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid};

mod index;
use std::{
    ffi::{OsStr, OsString},
    io::{BufRead, Read, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

#[derive(Debug, Clone)]
pub struct Git {
    pub cwd: PathBuf,
    config: Vec<(OsString, Option<OsString>)>,
    original_dir: Option<PathBuf>,
    remote: Option<OsString>,
    runtime: Option<PathBuf>,
}

pub(crate) struct HttpsOptions {
    pub(crate) ca: Option<PathBuf>,
    pub(crate) extra_header: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) follow_initial_redirects: bool,
}

fn remote_property<'a>(name: &OsStr, key: &'a OsStr) -> Option<&'a [u8]> {
    key.as_bytes()
        .strip_prefix(b"remote.")?
        .strip_prefix(name.as_bytes())?
        .strip_prefix(b".")
}

fn supports_disk_alternates(objects: &Path) -> bool {
    let list = objects.join("info/alternates");
    let metadata = match list.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    if !metadata.is_file() {
        return false;
    }
    let Ok(contents) = std::fs::read_to_string(list) else {
        return false;
    };
    // Shared clones can list several plain, absolute paths. Native Git handles
    // quoting, relative paths and nested alternate lists.
    let contents = contents.strip_suffix('\n').unwrap_or(&contents);
    contents.split('\n').all(|alternate| {
        alternate.starts_with('/')
            && !alternate.contains(['\r', '\0'])
            && !alternate.split('/').any(|part| matches!(part, "." | ".."))
            && Path::new(alternate)
                .join("info/alternates")
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

pub(crate) fn read_only(command: &mut Command) -> &mut Command {
    command
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    for (name, _) in std::env::vars_os() {
        if name.as_bytes().starts_with(b"GIT_TRACE") {
            command.env_remove(name);
        }
    }
    // Trace2 also reads target paths from effective Git configuration.
    command
        .env("GIT_TRACE2", "0")
        .env("GIT_TRACE2_EVENT", "0")
        .env("GIT_TRACE2_PERF", "0")
}

fn native_command(cwd: &Path, runtime: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    if let Some(runtime) = runtime {
        // Native launchers may cache their tool lookup (macOS xcrun).
        command.env("TMPDIR", runtime);
    }
    command
}

const RUN_LOCK_PREFIX: &str = ".lock-";

#[doc(hidden)]
pub fn run_lock_path(run: &Path, id: &str) -> PathBuf {
    run.join(format!("{RUN_LOCK_PREFIX}{id}"))
}

#[doc(hidden)]
pub fn run_marker(run: &Path) -> std::io::Result<Option<(PathBuf, uuid::Uuid)>> {
    if let Some(registration) = crate::run_directory::run_registration(run)? {
        return Ok(Some(registration));
    }
    let Some(short) = run
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.len() == 8)
    else {
        return Ok(None);
    };
    let mut marker = None;
    for entry in std::fs::read_dir(run)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix(RUN_LOCK_PREFIX))
        else {
            continue;
        };
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            continue;
        };
        if !id.starts_with(short) || id != uuid.to_string() || !entry.file_type()?.is_file() {
            continue;
        }
        if marker.replace((entry.path(), uuid)).is_some() {
            return Err(std::io::Error::other("ambiguous git-on run marker"));
        }
    }
    Ok(marker)
}

pub(crate) struct NativeScratch {
    directory: Option<PathBuf>,
    marker: Option<PathBuf>,
}

impl NativeScratch {
    pub(crate) fn path(&self) -> &Path {
        self.directory.as_deref().unwrap()
    }

    fn close(&mut self) -> Result<()> {
        if let Some(path) = self.directory.take() {
            remove_native_scratch(&path, self.marker.as_deref())?;
        }
        Ok(())
    }
}

impl Drop for NativeScratch {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn remove_native_scratch(path: &Path, marker: Option<&Path>) -> Result<()> {
    use std::{fs, io::ErrorKind};
    crate::platform::ensure_unmounted(path)?;
    if let Err(error) = fs::remove_dir_all(path) {
        if error.kind() != ErrorKind::NotFound {
            return Err(error).context("remove native Git scratch");
        }
    }
    if let Some(marker) = marker {
        if fs::read_link(marker).is_ok_and(|target| target == path) {
            fs::remove_file(marker).context("remove native Git scratch record")?;
        }
    }
    Ok(())
}

pub(crate) fn native_scratch<T>(
    run: &Path,
    prefix: &str,
    minimum_bytes: usize,
    mut prepare: impl FnMut(&Path) -> Result<T>,
) -> Result<(NativeScratch, T)> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o700);
    #[cfg(not(target_os = "linux"))]
    let _ = minimum_bytes;
    #[cfg(target_os = "linux")]
    if let Some((_, id)) = run_marker(run)? {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let mut attempted = None;
        for parent in runtime
            .iter()
            .map(PathBuf::as_path)
            .chain([Path::new("/dev/shm")])
            .filter(|path| path.is_absolute())
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| tmpfs_has_room(path, minimum_bytes))
        {
            if attempted.as_ref() == Some(&parent) {
                continue;
            }
            attempted = Some(parent.clone());
            let name = format!("git-on-native-{id}");
            let marker = run.join(".native-scratch");
            // Register the exact path before it can contain anything. The
            // symlink preserves byte paths and is created atomically.
            match std::os::unix::fs::symlink(parent.join(&name), &marker) {
                Ok(()) => {}
                Err(error) if native_scratch_marker_unavailable(&error) => break,
                Err(error) => return Err(error.into()),
            }
            let mut builder = tempfile::Builder::new();
            builder
                .prefix(&name)
                .rand_bytes(0)
                .permissions(permissions.clone());
            match builder.tempdir_in(&parent) {
                Ok(directory) => {
                    let mut scratch = NativeScratch {
                        directory: Some(directory.keep()),
                        marker: Some(marker),
                    };
                    match prepare(scratch.path()) {
                        Ok(value)
                            if minimum_bytes == 0
                                || tmpfs_has_room(scratch.path(), minimum_bytes) =>
                        {
                            return Ok((scratch, value));
                        }
                        Err(error) if crate::object::is_cancelled(&error) => return Err(error),
                        // A writable tmpfs can still run out of blocks during setup.
                        _ => scratch
                            .close()
                            .context("remove incomplete native Git scratch")?,
                    }
                }
                Err(error) => {
                    std::fs::remove_file(marker)?;
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        break;
                    }
                }
            };
        }
    }
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(permissions)
        .tempdir_in(run)?;
    let scratch = NativeScratch {
        directory: Some(directory.keep()),
        marker: None,
    };
    let value = prepare(scratch.path())?;
    Ok((scratch, value))
}

#[cfg(any(test, target_os = "linux"))]
fn native_scratch_marker_unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::AlreadyExists
            | std::io::ErrorKind::Unsupported
            | std::io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
#[test]
fn unavailable_scratch_marker_allows_the_local_fallback() {
    for error in [
        std::io::Error::from(std::io::ErrorKind::Unsupported),
        std::io::Error::from_raw_os_error(libc::EPERM),
    ] {
        assert!(native_scratch_marker_unavailable(&error), "{error}");
    }
}

#[cfg(target_os = "linux")]
fn is_tmpfs(path: &Path) -> bool {
    tmpfs_stats(path).is_some()
}

#[cfg(target_os = "linux")]
#[cfg(test)]
thread_local! {
    static TMPFS_EXHAUSTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_os = "linux")]
fn tmpfs_has_room(path: &Path, minimum_bytes: usize) -> bool {
    #[cfg(test)]
    if TMPFS_EXHAUSTED.get() {
        return false;
    }
    tmpfs_stats(path).is_some_and(|stats| {
        (stats.f_files == 0 || stats.f_ffree > 0)
            && (stats.f_blocks == 0
                || (stats.f_bavail as u128) * (stats.f_bsize as u128)
                    >= minimum_bytes.max(1) as u128)
    })
}

#[cfg(target_os = "linux")]
fn tmpfs_stats(path: &Path) -> Option<libc::statfs> {
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    unsafe {
        if libc::statfs(name.as_ptr(), stats.as_mut_ptr()) != 0 {
            return None;
        }
        let stats = stats.assume_init();
        (stats.f_type == libc::TMPFS_MAGIC).then_some(stats)
    }
}

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn cleanup_native_scratch(run: &Path, id: &str) -> Result<()> {
    use std::{fs, os::unix::fs::MetadataExt};
    let marker = run.join(".native-scratch");
    let target = match fs::read_link(&marker) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read native Git scratch record"),
    };
    let name = format!("git-on-native-{}", uuid::Uuid::parse_str(id)?);
    anyhow::ensure!(
        target.is_absolute() && target.file_name() == Some(OsStr::new(&name)),
        "invalid native Git scratch record"
    );
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect native Git scratch"),
    };
    let parent = target
        .parent()
        .context("missing native Git scratch parent")?;
    anyhow::ensure!(
        fs::canonicalize(parent)? == parent
            && is_tmpfs(parent)
            && metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "native Git scratch ownership changed"
    );
    remove_native_scratch(&target, Some(&marker))
}

impl Git {
    pub fn capture(cwd: &Path) -> Result<Self> {
        Self::capture_in(cwd, None)
    }

    pub(crate) fn capture_in(cwd: &Path, runtime: Option<&Path>) -> Result<Self> {
        let runtime = runtime.map(std::path::absolute).transpose()?;
        let mut command = native_command(cwd, runtime.as_deref());
        read_only(&mut command)
            .env_remove("GIT_CONFIG")
            .args(["config", "--null", "--list", "--includes"])
            .stdin(Stdio::null());
        let output = run(command, true).map_err(|error| {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) {
                error.context("native Git was not found on PATH; revision resolution requires native Git; install Git with your distribution tools or select an existing Git on PATH")
            } else {
                error.context("read effective Git configuration")
            }
        })?;
        if !output.status.success() {
            bail!(
                "read effective Git configuration: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut config = Vec::new();
        for record in output.stdout.split(|b| *b == 0).filter(|x| !x.is_empty()) {
            let (key, value) = record.split_once_byte(b'\n');
            // Includes were expanded in the caller's original repository context.
            if key.starts_with(b"include.") || key.starts_with(b"includeif.") {
                continue;
            }
            config.push((
                OsString::from_vec(key.to_vec()),
                value.map(|v| OsString::from_vec(v.to_vec())),
            ));
        }
        Ok(Self {
            cwd: cwd.to_path_buf(),
            config,
            remote: None,
            original_dir: {
                let mut command = native_command(cwd, runtime.as_deref());
                read_only(&mut command)
                    .args(["rev-parse", "--absolute-git-dir"])
                    .stdin(Stdio::null());
                run(command, true)
                    .ok()
                    .filter(|x| x.status.success())
                    .map(|x| {
                        PathBuf::from(OsString::from_vec(
                            x.stdout.strip_suffix(b"\n").unwrap_or(&x.stdout).to_vec(),
                        ))
                    })
            },
            runtime,
        })
    }
    pub(crate) fn select_remote(&mut self, name: &OsStr) -> bool {
        let named = self
            .config
            .iter()
            .any(|(key, _)| remote_property(name, key) == Some(b"url"));
        self.remote = named.then(|| name.to_os_string());
        named
    }

    pub(crate) fn recognizes_remote(&self, name: &OsStr) -> Result<bool> {
        if self.original_dir.is_none() {
            return Ok(false);
        }
        let mut command = self.command(None);
        read_only(&mut command)
            .args(["remote", "get-url", "--"])
            .arg(name)
            .stdin(Stdio::null());
        Ok(run(command, true)?.status.success())
    }

    pub(crate) fn worktree_root(&self) -> Result<Option<PathBuf>> {
        let mut command = self.command(None);
        read_only(&mut command)
            .args(["rev-parse", "--show-toplevel"])
            .stdin(Stdio::null());
        let output = run(command, true)?;
        Ok(output.status.success().then(|| {
            PathBuf::from(OsStr::from_bytes(
                output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
            ))
        }))
    }

    pub(crate) fn transport_directory(&self) -> Result<PathBuf> {
        if self.original_dir.is_none() {
            return Ok(self.cwd.clone());
        }
        Ok(self.worktree_root()?.unwrap_or_else(|| self.cwd.clone()))
    }

    fn transport_path(&self, path: PathBuf) -> Result<PathBuf> {
        if path.is_absolute() {
            return Ok(path);
        }
        Ok(self.transport_directory()?.join(path))
    }

    fn selected_remote_property<'a>(&self, key: &'a OsStr) -> Option<&'a [u8]> {
        remote_property(self.remote.as_deref()?, key)
    }

    pub fn command(&self, dir: Option<&Path>) -> Command {
        let mut command = native_command(&self.cwd, self.runtime.as_deref());
        command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1");
        if let Some(dir) = dir {
            if self.original_dir.as_deref() != Some(dir) {
                // Replay each effective value exactly once in the private repo.
                command
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env_remove("GIT_CONFIG_PARAMETERS")
                    .env_remove("GIT_CONFIG_COUNT");
                let mut count = 0;
                for (key, value) in &self.config {
                    // Object/ref storage belongs to the private repository,
                    // rather than the caller's format. Replaying format zero
                    // over a private signature repository could make
                    // native Git open that repository in the wrong format.
                    if key == "core.repositoryformatversion"
                        || key.as_bytes().starts_with(b"extensions.")
                        // The deprecated loose-object durability switch can
                        // override core.fsync=none. Private objects are never
                        // resumed after a crash; preserve the caller's setting
                        // only for operations in its own repository.
                        || key == "core.fsyncobjectfiles"
                    {
                        continue;
                    }
                    command.env(format!("GIT_CONFIG_KEY_{count}"), key).env(
                        format!("GIT_CONFIG_VALUE_{count}"),
                        value.as_deref().unwrap_or(OsStr::new("true")),
                    );
                    count += 1;
                }
                command.env("GIT_CONFIG_COUNT", count.to_string());
                for key in [
                    "GIT_DIR",
                    "GIT_WORK_TREE",
                    "GIT_COMMON_DIR",
                    "GIT_OBJECT_DIRECTORY",
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    "GIT_INDEX_FILE",
                ] {
                    command.env_remove(key);
                }
            }
            command
                .arg("--git-dir")
                .arg(dir)
                .args(["-c", "core.bare=true"]);
        }
        if let Some(program) = self.config.iter().rev().find_map(|(key, value)| {
            (key == "core.askpass")
                .then_some(value.as_deref())
                .flatten()
        }) {
            crate::terminal_ui::core_askpass(&mut command, program);
        }
        command
    }
    pub(crate) fn demand_session_compatible(&self) -> Result<bool> {
        // Reject server and validation policies the direct loader cannot honor.
        let protocol = self.config_value("protocol.version");
        let fsck = self
            .config_value("fetch.fsckobjects")
            .or_else(|| self.config_value("transfer.fsckobjects"));
        if protocol.is_some_and(|value| value != Some(OsStr::new("2")))
            || fsck.is_some_and(|value| {
                value
                    .and_then(OsStr::to_str)
                    .is_none_or(|value| git2::Config::parse_bool(value).unwrap_or(true))
            })
            || self.config.iter().any(|(key, _)| {
                matches!(
                    self.selected_remote_property(key),
                    Some(b"uploadpack" | b"vcs" | b"serveroption")
                )
            })
        {
            return Ok(false);
        }

        let Some(inherited) = std::env::var_os("GIT_EXEC_PATH") else {
            return Ok(true);
        };
        // Git exports its standard helper directory to external subcommands.
        // A custom directory can replace the Git helpers we rely on.
        let mut command = self.command(None);
        read_only(&mut command)
            .arg("--exec-path")
            .env_remove("GIT_EXEC_PATH")
            .stdin(Stdio::null());
        let result = run(command, true)?;
        Ok(result.status.success()
            && result.stdout.strip_suffix(b"\n") == Some(inherited.as_bytes()))
    }
    fn config_value(&self, key: &str) -> Option<Option<&OsStr>> {
        self.config
            .iter()
            .rev()
            .find(|(configured, _)| configured == key)
            .map(|(_, value)| value.as_deref())
    }

    pub(crate) fn https_options(&self, url: &OsStr) -> Result<HttpsOptions> {
        anyhow::ensure!(
            self.transport_allowed("https")
                && !self.config.iter().any(|(key, value)| {
                    match self.selected_remote_property(key) {
                        Some(b"proxy") => value.as_deref() != Some(OsStr::new("")),
                        Some(b"proxyauthmethod") => true,
                        _ => false,
                    }
                })
                && !std::env::vars_os().any(|(key, _)| {
                    let key = key.as_bytes();
                    key.starts_with(b"GIT_HTTP_") && key != b"GIT_HTTP_USER_AGENT"
                        || key.starts_with(b"GIT_SSL_") && key != b"GIT_SSL_CAINFO"
                }),
            "unsupported Git HTTPS configuration"
        );
        let mut settings = HttpsOptions {
            ca: std::env::var_os("GIT_SSL_CAINFO")
                .map(|path| self.transport_path(PathBuf::from(path)))
                .transpose()?,
            extra_header: None,
            user_agent: std::env::var_os("GIT_HTTP_USER_AGENT")
                .map(|value| {
                    value
                        .into_string()
                        .ok()
                        .context("invalid Git HTTP user agent")
                })
                .transpose()?,
            follow_initial_redirects: true,
        };
        if !self
            .config
            .iter()
            .any(|(key, _)| key.as_bytes().starts_with(b"http."))
        {
            return Ok(settings);
        }
        // Resolve only settings that apply to this URL using Git's precedence.
        let url_match = |key: &str, path: bool| -> Result<Vec<u8>> {
            let mut command = self.command(None);
            read_only(&mut command)
                .env_remove("GIT_CONFIG")
                .args(["config", "--null"]);
            if path {
                command.arg("--type=path");
            }
            command
                .args(["--get-urlmatch", key])
                .arg(url)
                .stdin(Stdio::null());
            let result = run(command, true)?;
            anyhow::ensure!(
                result.status.success()
                    || result.status.code() == Some(1) && result.stderr.is_empty(),
                "read Git HTTP settings: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
            Ok(result.stdout)
        };
        for record in url_match("http", false)?
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let (key, value) = record.split_once_byte(b'\n');
            match (key, value) {
                (b"http.extraheader", Some(value)) => {
                    let value = std::str::from_utf8(value).context("invalid Git HTTP header")?;
                    anyhow::ensure!(!value.contains(['\r', '\n']), "invalid Git HTTP header");
                    settings.extra_header = (!value.is_empty()).then(|| value.to_owned());
                }
                (b"http.sslcainfo", Some(_)) => {
                    if settings.ca.is_none() {
                        let path = url_match("http.sslCAInfo", true)?;
                        let path = path
                            .strip_suffix(b"\0")
                            .context("invalid Git CA bundle path")?;
                        settings.ca =
                            Some(self.transport_path(PathBuf::from(OsStr::from_bytes(path)))?);
                    }
                }
                (b"http.useragent", Some(value)) => {
                    if settings.user_agent.is_none() {
                        settings.user_agent = Some(std::str::from_utf8(value)?.to_owned());
                    }
                }
                (b"http.followredirects", Some(b"initial")) => {}
                (b"http.followredirects", value) => {
                    settings.follow_initial_redirects = value
                        .map(git2::Config::parse_bool)
                        .transpose()?
                        .unwrap_or(true);
                }
                // Git's post buffer controls its HTTP upload framing.
                // Our HTTP/2 requests already have a known length.
                (b"http.postbuffer", Some(value)) if git2::Config::parse_i64(value).is_ok() => {}
                (b"http.sslverify", value)
                    if value.is_none_or(|value| {
                        std::str::from_utf8(value)
                            .is_ok_and(|value| git2::Config::parse_bool(value).unwrap_or(false))
                    }) => {}
                (b"http.uploadpack" | b"http.receivepack", _)
                | (b"http.version", Some(b"HTTP/2"))
                | (b"http.proxy", Some(b"")) => {}
                _ => bail!("unsupported Git HTTPS configuration"),
            }
        }
        Ok(settings)
    }
    pub(crate) fn transport_allowed(&self, protocol: &str) -> bool {
        if let Some(allowed) = std::env::var_os("GIT_ALLOW_PROTOCOL") {
            return allowed
                .as_bytes()
                .split(|byte| *byte == b':')
                .any(|entry| entry == protocol.as_bytes());
        }
        let key = format!("protocol.{protocol}.allow");
        let setting = |key: &str| {
            self.config_value(key)
                .map(|value| value.and_then(OsStr::to_str).unwrap_or(""))
        };
        match setting(&key)
            .or_else(|| setting("protocol.allow"))
            .unwrap_or("always")
        {
            "always" => true,
            "user" => std::env::var_os("GIT_PROTOCOL_FROM_USER").is_none_or(|value| {
                value
                    .to_str()
                    .is_some_and(|value| git2::Config::parse_bool(value).unwrap_or(false))
            }),
            _ => false,
        }
    }

    pub fn output(&self, dir: Option<&Path>, args: &[&OsStr]) -> Result<Vec<u8>> {
        let mut command = self.command(dir);
        read_only(&mut command)
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .stdin(Stdio::null());
        let out = run(command, true)?;
        if !out.status.success() {
            bail!(
                "Git {} failed: {}",
                args[0].to_string_lossy(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }
    pub(crate) fn resolve(&self, revision: &OsStr, run_directory: &Path) -> Result<Oid> {
        if self.config.iter().any(|(key, value)| {
            let key = key.as_bytes();
            key == b"extensions.partialclone"
                || key.starts_with(b"remote.")
                    && key.ends_with(b".promisor")
                    && value
                        .as_ref()
                        .and_then(|value| value.to_str())
                        .is_none_or(|value| git2::Config::parse_bool(value).unwrap_or(true))
        }) {
            let mut probe = self.command(None);
            read_only(&mut probe)
                .args(["--no-lazy-fetch", "--version"])
                .stdin(Stdio::null());
            anyhow::ensure!(
                run(probe, true)?.status.success(),
                "local partial clones require a Git version that supports --no-lazy-fetch"
            );
        }
        if index::selects_from_index(revision) {
            return index::resolve(self, revision, run_directory);
        }
        let out = self.output(
            None,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                revision,
            ],
        )?;
        Oid::from_str(std::str::from_utf8(&out)?.trim())
            .context("resolve exact SHA-1 revision (SHA-256 repositories unsupported)")
    }
    pub(crate) fn object_repository(&self) -> Result<git2::Repository> {
        if ["GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"]
            .into_iter()
            .all(|key| std::env::var_os(key).is_none())
        {
            if let Ok(repo) = git2::Repository::open(self.repository_dir()?) {
                if supports_disk_alternates(&repo.commondir().join("objects")) && repo.odb().is_ok()
                {
                    return Ok(repo);
                }
                return Ok(git2::Repository::from_odb(git2::Odb::new()?)?);
            }
        }
        // Git resolves relative overrides in its source context.
        let path = self.output(
            None,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ]
            .map(OsStr::new),
        )?;
        let odb = git2::Odb::new()?;
        let path = OsStr::from_bytes(path.strip_suffix(b"\n").unwrap_or(&path));
        if let Some(path) = path
            .to_str()
            .filter(|path| supports_disk_alternates(Path::new(path)))
        {
            let _ = odb.add_disk_alternate(path);
        }
        Ok(git2::Repository::from_odb(odb)?)
    }

    pub fn repository_dir(&self) -> Result<&Path> {
        self.original_dir
            .as_deref()
            .context("native Git did not identify the selected repository directory")
    }
    pub(crate) fn verify(
        &self,
        oid: Oid,
        kind: ObjectType,
        size: usize,
        remote: Option<&[u8]>,
        run_directory: &Path,
    ) -> Result<()> {
        let operation = match kind {
            ObjectType::Commit => "verify-commit",
            ObjectType::Tag => "verify-tag",
            _ => bail!("--verify requires a signed commit or annotated tag; raw trees and blobs have no signature"),
        };
        // Git's detached signature file is at most the size of the selected
        // object. Choose scratch with room before starting the verifier.
        let (scratch, ()) = native_scratch(run_directory, "git-on-signature-", size, |path| {
            // Only a remote object needs a private native Git context.
            if let Some(object) = remote {
                let mut options = git2::RepositoryInitOptions::new();
                options.bare(true).external_template(false);
                let verification = git2::Repository::init_opts(path, &options)?;
                anyhow::ensure!(
                    verification.odb()?.write(kind, object)? == oid,
                    "selected signature object changed"
                );
            }
            Ok(())
        })?;
        let directory = std::path::absolute(scratch.path())?;
        let mut command = self.command(remote.map(|_| directory.as_path()));
        if remote.is_some() {
            // Native Git resolves relative verifier settings from the caller's
            // worktree root. The private bare directory cannot discover it.
            command.current_dir(self.transport_directory()?);
        }
        command
            .env("TMPDIR", directory)
            .arg(operation)
            .arg(oid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // Native Git owns acceptance, including the caller's configured trust threshold.
        // Its default can accept unknown-trust OpenPGP; a git-on threshold here
        // would silently replace that policy.
        let status = run(command, false)?.status;
        if !status.success() {
            bail!("native Git {operation} failed for {oid}; execution forbidden");
        }
        Ok(())
    }
    pub fn ssh_command(&self) -> Option<OsString> {
        std::env::var_os("GIT_SSH_COMMAND")
            .or_else(|| {
                self.config.iter().rev().find_map(|(key, value)| {
                    (key == "core.sshcommand").then(|| value.clone()).flatten()
                })
            })
            .or_else(|| {
                std::env::var_os("GIT_SSH").map(|path| {
                    // GIT_SSH is an executable path, not a shell command.
                    let mut quoted = vec![b'\''];
                    for byte in path.as_bytes() {
                        if *byte == b'\'' {
                            quoted.extend(b"'\\''");
                        } else {
                            quoted.push(*byte);
                        }
                    }
                    quoted.push(b'\'');
                    OsString::from_vec(quoted)
                })
            })
    }
    pub fn ssh_variant(&self) -> Option<OsString> {
        std::env::var_os("GIT_SSH_VARIANT").or_else(|| {
            self.config
                .iter()
                .rev()
                .find_map(|(key, value)| (key == "ssh.variant").then(|| value.clone()).flatten())
        })
    }
}
pub(crate) struct LocalObjects {
    pub(crate) git: Git,
    headers: Mutex<Option<crate::transport::NativeProcess>>,
    contents: Mutex<Option<crate::transport::NativeProcess>>,
}

impl LocalObjects {
    pub(crate) fn new(git: Git) -> Self {
        Self {
            git,
            headers: Mutex::new(None),
            contents: Mutex::new(None),
        }
    }

    pub(crate) fn object_header(&self, oid: Oid) -> Result<(usize, ObjectType)> {
        self.cat_file(oid, false, |size, kind, _| Ok((size, kind)))
    }

    pub(crate) fn read_object<T>(
        &self,
        oid: Oid,
        consume: impl FnOnce(usize, ObjectType, &mut dyn Read) -> Result<T>,
    ) -> Result<T> {
        self.cat_file(oid, true, consume)
    }

    fn cat_file<T>(
        &self,
        oid: Oid,
        contents: bool,
        consume: impl FnOnce(usize, ObjectType, &mut dyn Read) -> Result<T>,
    ) -> Result<T> {
        let (slot, option) = if contents {
            (&self.contents, "--batch")
        } else {
            (&self.headers, "--batch-check")
        };
        let mut batch = slot.lock().unwrap_or_else(|error| error.into_inner());
        if batch.is_none() {
            let mut command = self.git.command(None);
            read_only(&mut command).args(["-c", "core.fsmonitor=false", "cat-file", option]);
            *batch = Some(crate::transport::NativeProcess::spawn(
                command, None, false,
            )?);
        }
        let result = (|| {
            let (request, response) = batch.as_mut().unwrap().pipes();
            writeln!(request, "{oid}")?;
            let (actual, kind, size) = read_batch_header(response)?
                .context("native Git did not return the requested object")?;
            anyhow::ensure!(actual == oid, "native Git returned an unexpected object");
            if !contents {
                return consume(size, kind, &mut std::io::empty());
            }
            let mut payload = Read::by_ref(response).take(size as u64);
            let value = consume(size, kind, &mut payload)?;
            anyhow::ensure!(payload.limit() == 0, "incomplete Git object");
            let mut delimiter = [0];
            response.read_exact(&mut delimiter)?;
            anyhow::ensure!(delimiter == *b"\n", "invalid native Git object delimiter");
            Ok(value)
        })();
        result.with_context(|| {
            // A failed consumer may have left a partial record on the pipe.
            let diagnostics = batch.take().unwrap().error_text();
            format!("native Git cat-file failed: {}", diagnostics.trim())
        })
    }

    pub(crate) fn stop(&self) -> Result<()> {
        for slot in [&self.headers, &self.contents] {
            if let Some(batch) = slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                batch.stop()?;
            }
        }
        Ok(())
    }
}

trait SplitByte {
    fn split_once_byte(&self, b: u8) -> (&[u8], Option<&[u8]>);
}
static CANCELLED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static CANCELLATION_HANDLERS: std::sync::OnceLock<std::io::Result<()>> = std::sync::OnceLock::new();
static OPERATIONS_STOPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// End owned native operations after the command finishes without inventing a
/// termination signal. The runner process handles one command invocation.
pub fn stop_operations() {
    OPERATIONS_STOPPED.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn operations_stopped() -> bool {
    OPERATIONS_STOPPED.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn cancelled_signal() -> Option<i32> {
    let signal = CANCELLED.load(std::sync::atomic::Ordering::Relaxed);
    (signal != 0).then_some(signal)
}
/// Stop owned native operations when the foreground command is interrupted.
pub fn cancel_by_signal(signal: i32) {
    if matches!(
        signal,
        libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
    ) {
        CANCELLED.store(signal, std::sync::atomic::Ordering::Relaxed);
    }
}
/// Register the cancellation latch before other subscribers can consume a
/// signal. Concurrent first callers wait for all four handlers, including a
/// registration failure, rather than observing partly initialized handlers.
pub fn initialize_cancellation() -> std::io::Result<()> {
    CANCELLATION_HANDLERS
        .get_or_init(|| {
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
                unsafe {
                    signal_hook::low_level::register(signal, move || {
                        CANCELLED.store(signal, std::sync::atomic::Ordering::Relaxed);
                    })?;
                }
            }
            Ok(())
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))
}
fn read_batch_header(input: &mut impl BufRead) -> Result<Option<(Oid, ObjectType, usize)>> {
    let mut header = Vec::new();
    if Read::by_ref(input)
        .take(128)
        .read_until(b'\n', &mut header)?
        == 0
    {
        return Ok(None);
    }
    let header = std::str::from_utf8(
        header
            .strip_suffix(b"\n")
            .context("invalid native Git batch header")?,
    )?;
    let mut fields = header.split(' ');
    let id = fields
        .next()
        .filter(|id| id.len() == 40)
        .context("invalid native Git object ID")?;
    let oid = Oid::from_str(id)?;
    let kind = match fields.next() {
        Some("blob") => ObjectType::Blob,
        Some("tree") => ObjectType::Tree,
        Some("commit") => ObjectType::Commit,
        Some("tag") => ObjectType::Tag,
        Some("missing") => bail!("native Git could not find object {oid}"),
        _ => bail!("unknown native Git object type"),
    };
    let size = fields
        .next()
        .context("missing native Git object size")?
        .parse()?;
    anyhow::ensure!(fields.next().is_none(), "invalid native Git batch header");
    Ok(Some((oid, kind, size)))
}

pub fn run(command: Command, capture: bool) -> Result<std::process::Output> {
    let managed = crate::terminal_ui::owns_terminal();
    let mut output = run_captured(command, capture || managed, capture || managed, None, None)?;
    if managed && !capture {
        use std::io::Write;
        let _drawing = crate::terminal_ui::native_terminal_lease(true);
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        output.stdout.clear();
        output.stderr.clear();
    }
    Ok(output)
}
/// Stream native stdout to its consumer while the caller retains process,
/// cancellation and terminal supervision. The consumer must read to EOF or fail;
/// its errors stop and reap the owned operation before they are returned.
pub(crate) fn run_with_output<T: Send>(
    mut command: Command,
    input: Option<&mut dyn std::io::Read>,
    consume: impl FnOnce(&mut dyn std::io::Read) -> Result<T> + Send,
) -> Result<(std::process::Output, T)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
    command.stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
        writer,
    )));
    let failed = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    reader.set_read_timeout(Some(std::time::Duration::from_millis(10)))?;
    std::thread::scope(|scope| {
        let transfer = std::thread::Builder::new()
            .name("git-output".into())
            .spawn_scoped(scope, || {
                let mut output = NativeOutput {
                    reader,
                    finished: &finished,
                    start_drain: Some(|input: &mut std::os::unix::net::UnixStream| {
                        input.set_nonblocking(true)
                    }),
                };
                let result = consume(&mut output);
                failed.store(result.is_err(), Ordering::Release);
                result
            })?;
        let result = run_captured(command, false, true, Some(&failed), input);
        finished.store(true, Ordering::Release);
        let value = transfer
            .join()
            .map_err(|_| anyhow::anyhow!("native output consumer panicked"))??;
        Ok((result?, value))
    })
}

#[cfg(all(test, target_os = "linux"))]
fn run_to_writer<W: std::io::Write + Send>(
    command: Command,
    mut output: W,
    input: Option<&mut dyn std::io::Read>,
) -> Result<std::process::Output> {
    run_with_output(command, input, |reader| {
        let mut bytes = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut bytes)?;
            if count == 0 {
                return Ok(());
            }
            output.write_all(&bytes[..count])?;
        }
    })
    .map(|(result, ())| result)
}

struct NativeOutput<'a, R, F> {
    reader: R,
    finished: &'a std::sync::atomic::AtomicBool,
    start_drain: Option<F>,
}
impl<R: std::io::Read, F: FnOnce(&mut R) -> std::io::Result<()>> std::io::Read
    for NativeOutput<'_, R, F>
{
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // Preserve typed cancellation rather than reporting truncated Git
            // data after the supervisor closes an interrupted decoder's pipe.
            crate::object::check_transfer().map_err(std::io::Error::other)?;
            match self.reader.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.finished.load(std::sync::atomic::Ordering::Acquire) {
                        // A timeout can precede the final write/exit. Observe
                        // completion before freshly draining queued bytes.
                        match self.start_drain.take() {
                            Some(prepare) => prepare(&mut self.reader)?,
                            None => return Ok(0),
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

pub(crate) fn controlling_terminal() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

fn run_captured(
    mut command: Command,
    capture_stdout: bool,
    capture_stderr: bool,
    failed_output: Option<&std::sync::atomic::AtomicBool>,
    input: Option<&mut dyn std::io::Read>,
) -> Result<std::process::Output> {
    use std::{
        os::unix::{io::AsRawFd, process::CommandExt},
        time::Duration,
    };
    initialize_cancellation()?;
    if let Some(signal) = cancelled_signal() {
        return Err(crate::object::OperationCancelled)
            .context(format!("Git operation cancelled by signal {signal}"));
    }
    if operations_stopped() {
        return Err(crate::object::OperationCancelled)
            .context("Git operation stopped after command completion");
    }
    let mut input = input
        .map(|source| NativeInput::new(&mut command, source))
        .transpose()?;
    let mut captured = [None, None];
    if capture_stdout {
        let (output, writer) = Capture::new(None)?;
        command.stdout(writer);
        captured[0] = Some(output);
    }
    if capture_stderr {
        let (output, writer) = Capture::new(None)?;
        command.stderr(writer);
        captured[1] = Some(output);
    }
    let managed = crate::terminal_ui::owns_terminal();
    crate::terminal_ui::configure(&mut command);
    if managed && input.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        command.stdin(Stdio::null());
    }
    let tty = (!managed).then(controlling_terminal).flatten();
    let tty_fd = tty.as_ref().map(AsRawFd::as_raw_fd);
    let parent_group = unsafe { libc::getpgrp() };
    let transfer = tty_fd.is_some_and(|fd| unsafe { libc::tcgetpgrp(fd) == parent_group });
    let _progress_lease = crate::terminal_ui::native_terminal_lease(transfer);
    unsafe {
        command.pre_exec(move || {
            if if managed {
                libc::setsid()
            } else {
                libc::setpgid(0, 0)
            } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if transfer {
                set_foreground(tty_fd.unwrap(), libc::getpgrp())?;
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if transfer {
                let _ = set_foreground(tty_fd.unwrap(), parent_group);
            }
            return Err(error).context("start native Git operation");
        }
    };
    drop(command);
    let _operation = crate::terminal_ui::NativeOperation::new(child.id());
    let wakeup = ChildExitWakeup::new(child.id());
    let outcome = loop {
        match crate::terminal_ui::try_wait(&mut child) {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error.into()),
            Ok(None) => {}
        }
        if failed_output.is_some_and(|failed| failed.load(std::sync::atomic::Ordering::Acquire)) {
            break stop_request_operation(&mut child)
                .and(Err(anyhow::anyhow!("native Git output consumer failed")));
        }
        if let Some(signal) = cancelled_signal() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = crate::terminal_ui::reap(&mut child);
            break Err(crate::object::OperationCancelled)
                .context(format!("Git operation cancelled by signal {signal}"));
        }
        if operations_stopped() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = crate::terminal_ui::reap(&mut child);
            break Err(crate::object::OperationCancelled)
                .context("Git operation stopped after command completion");
        }
        if let Some(stdin) = &mut input {
            match stdin.advance() {
                Ok(true) => input = None,
                Ok(false) => {}
                Err(error) => {
                    break stop_request_operation(&mut child)
                        .and(Err(error).context("write native Git input"));
                }
            }
        }
        if let Err(error) = captured.iter_mut().flatten().try_for_each(Capture::pump) {
            break stop_request_operation(&mut child)
                .and(Err(error).context("read native Git output"));
        }
        if let Some(fd) = tty_fd {
            if let Err(error) = relay_native_job_control(child.id(), fd, parent_group) {
                break stop_request_operation(&mut child).and(Err(error));
            }
        }
        wakeup.wait(
            Duration::from_millis(10),
            input.as_ref().map(|input| input.pipe.as_raw_fd()),
            captured
                .each_ref()
                .map(|output| output.as_ref().and_then(Capture::fd)),
        );
    };
    // A background resume must leave the terminal with the invoking shell.
    if let Some(fd) = tty_fd {
        if unsafe { libc::tcgetpgrp(fd) } == child.id() as libc::pid_t {
            let _ = set_foreground(fd, parent_group);
        }
    }
    let status = outcome?;
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        // Terminal-generated signals go to the temporarily foreground Git
        // group, so the runner's own signal handler may never receive them.
        cancel_by_signal(signal);
    }
    let [stdout, stderr] = captured.map(|output| output.map(Capture::finish).transpose());
    Ok(std::process::Output {
        status,
        stdout: stdout?.unwrap_or_default(),
        stderr: stderr?.unwrap_or_default(),
    })
}

pub(crate) struct Capture {
    stream: Option<std::os::unix::net::UnixStream>,
    pub(crate) bytes: Vec<u8>,
    tail: Option<usize>,
}

impl Capture {
    pub(crate) fn new(tail: Option<usize>) -> std::io::Result<(Self, Stdio)> {
        use std::{os::fd::OwnedFd, os::unix::net::UnixStream};
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        Ok((
            Self {
                stream: Some(reader),
                bytes: Vec::new(),
                tail,
            },
            Stdio::from(OwnedFd::from(writer)),
        ))
    }

    pub(crate) fn fd(&self) -> Option<i32> {
        self.stream.as_ref().map(std::os::fd::AsRawFd::as_raw_fd)
    }

    pub(crate) fn pump(&mut self) -> std::io::Result<()> {
        use std::io::ErrorKind;
        let Some(input) = self.stream.as_mut() else {
            return Ok(());
        };
        let mut bytes = [0; 8192];
        for _ in 0..8 {
            match input.read(&mut bytes) {
                Ok(0) => {
                    self.stream = None;
                    break;
                }
                Ok(count) => {
                    self.bytes.extend_from_slice(&bytes[..count]);
                    if let Some(tail) = self.tail {
                        let extra = self.bytes.len().saturating_sub(tail);
                        self.bytes.drain(..extra);
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> std::io::Result<Vec<u8>> {
        // Drain queued socket data without waiting for inherited writers in
        // a verifier's subprocesses.
        for _ in 0..8 {
            self.pump()?;
        }
        Ok(self.bytes)
    }
}
struct NativeInput<'a> {
    source: &'a mut dyn std::io::Read,
    pipe: std::os::unix::net::UnixStream,
    bytes: [u8; 8192],
    pending: std::ops::Range<usize>,
}
impl<'a> NativeInput<'a> {
    fn new(command: &mut Command, source: &'a mut dyn std::io::Read) -> std::io::Result<Self> {
        use std::os::fd::OwnedFd;
        let (pipe, child) = std::os::unix::net::UnixStream::pair()?;
        pipe.set_nonblocking(true)?;
        command.stdin(Stdio::from(OwnedFd::from(child)));
        Ok(Self {
            source,
            pipe,
            bytes: [0; 8192],
            pending: 0..0,
        })
    }
    fn advance(&mut self) -> std::io::Result<bool> {
        use std::io::{ErrorKind, Write};
        // Bound work between cancellation checks, including for a decoder's
        // large segmented request stream. No input worker or full copy is needed.
        for _ in 0..8 {
            if self.pending.is_empty() {
                let count = match self.source.read(&mut self.bytes) {
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    value => value?,
                };
                if count == 0 {
                    return Ok(true);
                }
                self.pending = 0..count;
            }
            match self.pipe.write(&self.bytes[self.pending.clone()]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(count) => self.pending.start += count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
}
/// Exit notification removes the artificial polling floor for short native
/// operations. Cancellation/TTY checks keep the same maximum polling interval.
/// Unsupported kernels or descriptor pressure retain the existing fallback.
pub(crate) struct ChildExitWakeup {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fd: Option<std::os::fd::OwnedFd>,
}
impl ChildExitWakeup {
    pub(crate) fn new(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
            Self {
                fd: (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
            }
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::FromRawFd;

            // Darwin creates the descriptor close-on-exec. Poll wakes when
            // the notification arrives; the existing try_wait reaps the child.
            let queue = unsafe { libc::kqueue() };
            if queue < 0 {
                return Self { fd: None };
            }
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(queue) };
            let exit = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let registered =
                unsafe { libc::kevent(queue, &exit, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            Self {
                fd: (registered == 0).then_some(fd),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            Self {}
        }
    }
    pub(crate) fn wait(
        &self,
        duration: std::time::Duration,
        input: Option<i32>,
        output: [Option<i32>; 2],
    ) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let exit = self.fd.as_ref().map_or(-1, std::os::fd::AsRawFd::as_raw_fd);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let exit = -1;
        if exit >= 0 || input.is_some() || output.iter().any(Option::is_some) {
            let mut descriptors = [
                libc::pollfd {
                    fd: exit,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: input.unwrap_or(-1),
                    events: libc::POLLOUT,
                    revents: 0,
                },
                libc::pollfd {
                    fd: output[0].unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: output[1].unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let result = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as _,
                    duration.as_millis() as i32,
                )
            };
            if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                std::thread::sleep(duration);
            }
            return;
        }
        std::thread::sleep(duration);
    }
}
/// Native Git owns a separate group while authenticating/verifying. Observe
/// stops without consuming its exit status, then suspend the caller's job so
/// its shell can report the stop and perform ordinary `fg`/`bg` transitions.
fn relay_native_job_control(pid: u32, fd: i32, parent_group: libc::pid_t) -> Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result =
        unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WSTOPPED | libc::WNOHANG) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        // WSTOPPED alone can report an exited child as ECHILD. Child::try_wait
        // remains the sole owner of the exit status on the next iteration.
        if error.raw_os_error() == Some(libc::ECHILD)
            || error.kind() == std::io::ErrorKind::Interrupted
        {
            return Ok(());
        }
        return Err(error.into());
    }
    let group = pid as libc::pid_t;
    let foreground = unsafe { libc::tcgetpgrp(fd) };
    if foreground == parent_group {
        // `fg` can foreground an already-running background job without
        // sending SIGCONT. Pass ownership on during the normal polling loop.
        set_foreground(fd, group)?;
    }
    if crate::platform::siginfo_pid(&info) == 0 {
        return Ok(());
    }
    let stop = crate::platform::siginfo_status(&info);
    if foreground == parent_group && matches!(stop, libc::SIGTTIN | libc::SIGTTOU) {
        // A pending background-I/O stop raced the shell's `fg` transition.
        unsafe { libc::kill(-group, libc::SIGCONT) };
        return Ok(());
    }
    if unsafe { libc::tcgetpgrp(fd) } == group {
        set_foreground(fd, parent_group)?;
    }
    unsafe {
        libc::kill(-parent_group, libc::SIGSTOP);
    }
    // Only `fg` grants the caller's job the terminal. A `bg` resume leaves
    // it with the shell, so another terminal read can stop the job normally.
    if unsafe { libc::tcgetpgrp(fd) } == parent_group {
        set_foreground(fd, group)?;
    }
    unsafe { libc::kill(-group, libc::SIGCONT) };
    Ok(())
}
fn stop_request_operation(child: &mut std::process::Child) -> Result<()> {
    use std::time::{Duration, Instant};
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    // Let native Git roll back its locks. WNOWAIT preserves leader ownership
    // until every remaining group member has been killed, preventing PID reuse.
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            break;
        }
        if (result == 0 && crate::platform::siginfo_pid(&info) != 0) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    crate::terminal_ui::reap(child).context("reap interrupted native Git operation")?;
    Ok(())
}
/// Transfer terminal ownership while blocking SIGTTOU in the calling thread.
pub fn set_foreground(fd: i32, group: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let result = libc::tcsetpgrp(fd, group);
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if result != 0 {
            return Err(error);
        }
        Ok(())
    }
}
impl SplitByte for [u8] {
    fn split_once_byte(&self, b: u8) -> (&[u8], Option<&[u8]>) {
        match self.iter().position(|x| *x == b) {
            Some(n) => (&self[..n], Some(&self[n + 1..])),
            None => (self, None),
        }
    }
}

#[cfg(test)]
mod transport_policy_tests {
    use super::*;

    #[test]
    fn direct_https_respects_explicit_transport_denial() {
        for key in ["protocol.allow", "protocol.https.allow"] {
            let git = Git {
                cwd: PathBuf::new(),
                config: vec![(key.into(), Some("never".into()))],
                original_dir: None,
                remote: None,
                runtime: None,
            };
            assert!(
                git.https_options(OsStr::new("https://example.invalid/repo.git"))
                    .is_err(),
                "{key}"
            );
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn recovery_removes_external_native_scratch() {
    const SECOND_TMPFS_FIXTURE: &str = "GIT_ON_TEST_SECOND_TMPFS";
    let Some(runtime) = std::env::var_os(SECOND_TMPFS_FIXTURE).map(PathBuf::from) else {
        if !is_tmpfs(Path::new("/dev/shm")) {
            return;
        }
        let Ok(first) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "git::recovery_removes_external_native_scratch"])
            .env("XDG_RUNTIME_DIR", first.path())
            .env(SECOND_TMPFS_FIXTURE, first.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        return;
    };
    let runtime = runtime.canonicalize().unwrap();
    let other_tmpfs = Path::new("/dev/shm").canonicalize().unwrap();
    let root = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4();
    let id = id.to_string();
    let run = root.path().join(&id[..8]);
    std::fs::create_dir(&run).unwrap();
    std::fs::File::create(run_lock_path(&run, &id)).unwrap();
    let (scratch, ()) = native_scratch(&run, "scratch-", 0, |path| {
        std::fs::write(path.join("data"), b"native Git temporary bytes")?;
        Ok(())
    })
    .unwrap();
    let record = run.join(".native-scratch");
    let path = scratch.path().to_owned();
    assert_eq!(std::fs::read_link(&record).unwrap(), path);
    crate::platform::TEST_MOUNTPOINT.set(Some(path.join("mounted")));
    let result = cleanup_native_scratch(&run, &id);
    drop(scratch);
    crate::platform::TEST_MOUNTPOINT.set(None);
    assert!(result.unwrap_err().to_string().contains("a mount remains"));
    assert_eq!(
        std::fs::read(path.join("data")).unwrap(),
        b"native Git temporary bytes"
    );
    assert_eq!(std::fs::read_link(&record).unwrap(), path);
    cleanup_native_scratch(&run, &id).unwrap();
    assert!(!path.exists());
    cleanup_native_scratch(&run, &id).unwrap();
    assert!(!record.is_symlink());

    let (second, ()) = native_scratch(&run, "scratch-", 0, |path| {
        if path.starts_with(&runtime) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(second.path().parent(), Some(other_tmpfs.as_path()));
    assert!(!runtime.join(second.path().file_name().unwrap()).exists());
    drop(second);
    assert!(!run.join(".native-scratch").is_symlink());

    let mut attempts = Vec::new();
    let (fallback, ()) = native_scratch(&run, "scratch-", 0, |path| {
        attempts.push(path.to_path_buf());
        std::fs::File::create(path.join("data"))?;
        if !path.starts_with(&run) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
        std::fs::write(path.join("data"), b"native Git temporary bytes")?;
        Ok(())
    })
    .unwrap();
    assert!(attempts.len() >= 2);
    assert!(attempts[..attempts.len() - 1]
        .iter()
        .all(|path| !path.exists()));
    assert_eq!(fallback.path().parent(), Some(run.as_path()));
    assert!(!run.join(".native-scratch").is_symlink());

    let (spacious, ()) = native_scratch(&run, "signature-", usize::MAX, |path| {
        assert_eq!(path.parent(), Some(run.as_path()));
        Ok(())
    })
    .unwrap();
    assert_eq!(spacious.path().parent(), Some(run.as_path()));

    let (spacious, ()) = native_scratch(&run, "signature-", 1, |path| {
        if path.parent() != Some(run.as_path()) {
            TMPFS_EXHAUSTED.set(true);
        }
        Ok(())
    })
    .unwrap();
    TMPFS_EXHAUSTED.set(false);
    assert_eq!(spacious.path().parent(), Some(run.as_path()));
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn darwin_child_exit_notifies_the_wait_loop() {
    use std::os::fd::AsRawFd;

    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let wakeup = ChildExitWakeup::new(child.id());
    drop(child.stdin.take());
    let mut event = libc::pollfd {
        fd: wakeup.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd),
        events: libc::POLLIN,
        revents: 0,
    };
    let notified = event.fd >= 0 && unsafe { libc::poll(&mut event, 1, 5000) } == 1;
    assert!(child.wait().unwrap().success());
    assert!(notified && event.revents & libc::POLLIN != 0);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn native_input_and_output_progress_with_pipe_backpressure() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat"]);
        let bytes = vec![b'x'; 1024 * 1024];
        let mut input = bytes.as_slice();
        let mut output = Vec::new();
        let result = run_to_writer(command, &mut output, Some(&mut input)).unwrap();
        assert!(result.status.success());
        assert_eq!(&output[..262144], vec![0; 262144]);
        assert_eq!(&output[262144..], bytes);
    }

    #[test]
    fn native_output_writer_error_stops_the_owned_operation() {
        struct Failed;
        impl std::io::Write for Failed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("not-stopped");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf payload; sleep 3; printf late > \"$1\"",
                "fixture",
            ])
            .arg(&marker);
        let error = run_to_writer(command, Failed, None).unwrap_err();
        assert!(error.to_string().contains("fixture output failure"));
        assert!(
            !marker.exists(),
            "native operation continued after output failed"
        );
    }

    #[test]
    fn streamed_output_retains_the_native_exit_status() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf payload; exit 42"]);
        let (result, bytes) = run_with_output(command, None, |reader| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(bytes, b"payload");
        assert_eq!(result.status.code(), Some(42));
    }

    fn wait_for_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|pid| pid.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "owned operation never started");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn interrupted_owned_group_stops_even_with_ignored_term_or_early_leader_exit() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        for leader_exits in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let started = temporary.path().join("stubborn-pid");
            let script = if leader_exits {
                "sh -c 'trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300' stubborn \"$1\" & wait"
            } else {
                "trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300"
            };
            let mut child = Command::new("sh")
                .args(["-c", script, "owned-group-test"])
                .arg(&started)
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = wait_for_pid(&started);
            stop_request_operation(&mut child).unwrap();
            assert_eq!(
                child.wait().unwrap().signal(),
                Some(if leader_exits {
                    libc::SIGTERM
                } else {
                    libc::SIGKILL
                })
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
                if state.is_err() || state.unwrap().split_whitespace().nth(2) == Some("Z") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "owned descendant survived SIGKILL"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod output_race_tests {
    use super::*;
    use std::{
        io::{self, Read, Write},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };
    struct StaleTimeout<'a> {
        input: UnixStream,
        writer: UnixStream,
        finished: &'a AtomicBool,
        raced: bool,
    }
    impl Read for StaleTimeout<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if !self.raced {
                self.raced = true;
                let stale = self.input.read(output);
                assert!(
                    matches!(&stale, Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
                );
                // A native write and completed-child notification race a read
                // that has already returned a timeout to the transfer thread.
                self.writer
                    .write_all(b"verified output queued before exit")?;
                self.finished.store(true, Ordering::Release);
                return stale;
            }
            self.input.read(output)
        }
    }
    fn input(finished: &AtomicBool) -> StaleTimeout<'_> {
        let (input, writer) = UnixStream::pair().unwrap();
        input
            .set_read_timeout(Some(std::time::Duration::from_millis(1)))
            .unwrap();
        StaleTimeout {
            input,
            writer,
            finished,
            raced: false,
        }
    }
    #[test]
    fn finished_output_is_drained_after_a_stale_timeout() {
        let finished = AtomicBool::new(false);
        let mut input = input(&finished);
        let mut output = Vec::new();
        let mut reader = NativeOutput {
            reader: &mut input,
            finished: &finished,
            start_drain: Some(|input: &mut &mut StaleTimeout<'_>| {
                input.input.set_nonblocking(true)
            }),
        };
        io::copy(&mut reader, &mut output).unwrap();
        assert_eq!(output, b"verified output queued before exit");
    }
    #[test]
    fn final_output_drain_keeps_staging_failures_visible() {
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("staging failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let finished = AtomicBool::new(false);
        let mut input = input(&finished);
        let mut reader = NativeOutput {
            reader: &mut input,
            finished: &finished,
            start_drain: Some(|input: &mut &mut StaleTimeout<'_>| {
                input.input.set_nonblocking(true)
            }),
        };
        assert!(io::copy(&mut reader, &mut Failed).is_err());
    }
}
