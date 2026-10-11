//! Explicit one-object fetches. Git's promisor auto-fetch is never the loader.
use crate::git::Git;
use anyhow::{bail, Context, Result};
use git2::{Oid, Repository};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const DISK_CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const DISK_CACHE_ENTRIES: usize = 4096;

pub struct Remote {
    pub git: Git,
    pub dir: PathBuf,
    source: OsString,
    cache: DiskCache,
    socket: Option<PathBuf>,
    ssh: Option<OsString>,
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
        let mut remote = Self {
            git,
            dir,
            source: transport_source,
            cache,
            socket,
            ssh,
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
        let out = crate::git::run(command, true).context("advertise remote refs")?;
        if !out.status.success() {
            bail!(
                "remote ref advertisement/authentication failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
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
        let refs = std::str::from_utf8(advertisement).context("remote refs are not UTF-8")?;
        let names = [
            revision.to_owned(),
            format!("refs/heads/{revision}"),
            format!("refs/tags/{revision}"),
        ];
        let mut matches = Vec::new();
        for line in refs.lines() {
            let Some((oid, name)) = line.split_once('\t') else {
                continue;
            };
            if names.iter().any(|n| n == name) && !oid.starts_with("ref:") {
                matches.push((
                    name,
                    Oid::from_str(oid).context("remote SHA-256 repositories are unsupported")?,
                ));
            }
        }
        matches.sort();
        matches.dedup();
        match matches.as_slice() { [(_, oid)] => Ok(*oid), [] => bail!("remote revision {revision:?} is not advertised; use HEAD, an exact ref, branch/tag name, or a full SHA-1 OID (ancestry/reflog syntax unsupported)"), _ => bail!("ambiguous remote revision {revision:?}; use refs/heads/ or refs/tags/") }
    }
    pub fn ensure(&mut self, oid: Oid) -> Result<()> {
        let repo = Repository::open_bare(&self.dir)?;
        if repo.odb()?.exists(oid) {
            return Ok(());
        }
        if self.cache.restore(&repo, oid)? {
            return Ok(());
        }
        let outcome = self.fetch(&repo, oid);
        if let Err(error) = &outcome {
            self.failed = Some(format!("{error:#}"));
            eprintln!("git-on: remote object loading failed: {error:#}");
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
        let out = crate::git::run(command, true).context("fetch exact remote object")?;
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
    pub fn stop_transport(&self) {
        if let Some(socket) = &self.socket {
            stop_connection(socket);
        }
    }
    pub fn failure(&self) -> Option<String> {
        self.failed.clone()
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.stop_transport();
    }
}
/// Stop only the control connection in a validated run's private directory.
/// Recovery callers must validate and lock that run before invoking this.
pub fn stop_connection(socket: &Path) {
    let child = Command::new("ssh")
        .args(["-F", "/dev/null", "-S"])
        .arg(socket)
        .args(["-O", "exit", "git-on-control"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        let start = std::time::Instant::now();
        while child.try_wait().ok().flatten().is_none() {
            if start.elapsed() > std::time::Duration::from_secs(1) {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
