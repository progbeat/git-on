//! Immutable, demand-loaded objects from the pinned Git tree.
use crate::tree::Directory;
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid, Repository, RepositoryOpenFlags};
use serde::Serialize;
#[cfg(feature = "test-metrics")]
use std::collections::BTreeMap;
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, BufRead, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::symlink,
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
};

pub const BLOB_CACHE_BYTES: usize = crate::raw_blob::MEMORY_BUDGET;

/// Recognize complete pointer files, not ordinary source/documentation which
/// happens to start with the LFS version line. Keep this bounded even for large
/// blobs. Format: https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md
fn is_lfs_pointer(bytes: &[u8]) -> bool {
    if bytes.len() >= 1024 || bytes.contains(&0) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    // Like Git LFS's decoder, also recognize CRLF and a missing final newline.
    let mut lines = text.lines();
    if !matches!(
        lines.next(),
        Some(
            "version https://git-lfs.github.com/spec/v1"
                | "version https://hawser.github.com/spec/v1"
                | "version http://git-media.io/v/2"
        )
    ) {
        return false;
    }
    let valid_oid = |value: &str| {
        value.strip_prefix("sha256:").is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    };
    let mut previous = None;
    let mut oid = false;
    let mut size = false;
    let mut extension_priorities = [false; 10];
    for line in lines {
        let Some((key, value)) = line.split_once(' ') else {
            return false;
        };
        if key.is_empty()
            || !key.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte)
            })
            || value.is_empty()
            || value.starts_with(' ')
            || value.contains('\r')
            || key == "version"
            || previous.is_some_and(|last| last >= key)
        {
            return false;
        }
        previous = Some(key);
        match key {
            "oid" => {
                if !valid_oid(value) {
                    return false;
                }
                oid = true;
            }
            "size" => {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) || value.parse::<i64>().is_err()
                {
                    return false;
                }
                size = true;
            }
            key if key.starts_with("ext-") => {
                let Some((priority, name)) = key[4..].split_once('-') else {
                    return false;
                };
                if priority.len() != 1
                    || !priority.as_bytes()[0].is_ascii_digit()
                    || name.is_empty()
                    || !valid_oid(value)
                {
                    return false;
                }
                let priority = usize::from(priority.as_bytes()[0] - b'0');
                if std::mem::replace(&mut extension_priorities[priority], true) {
                    return false;
                }
            }
            // The specification permits unknown keys; their syntax and order
            // still have to be valid, and they cannot replace required fields.
            _ => {}
        }
    }
    oid && size
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: EntryKind,
    pub mode: u32,
    pub oid: Oid,
    /// Directory listings deliberately omit blob sizes. `entry` obtains the
    /// correct size from an ODB header, without requesting the file payload.
    pub size: Option<u64>,
}

enum LocalObjectAnchor {
    Native(Box<NativeLocalObjects>),
    Temporary {
        _directory: tempfile::TempDir,
    },
    #[cfg(target_os = "linux")]
    Descriptor {
        _file: fs::File,
    },
}

/// libgit2 treats ordinary relative entries as cwd-relative and cannot
/// decode Git's C-quoted entries. Keep this compatibility path in native Git
/// so a similarly named directory in the caller's cwd cannot supply objects.
struct NativeLocalObjects {
    batch: Mutex<NativeObjectBatch>,
    #[cfg(feature = "test-metrics")]
    git: crate::git::Git,
    #[cfg(feature = "test-metrics")]
    primary: PathBuf,
    #[cfg(feature = "test-metrics")]
    alternates: OsString,
}

struct NativeObjectBatch {
    contents: crate::transport::BatchProcess,
    // Git before 2.36 needs a separate header-only process.
    headers: Option<crate::transport::BatchProcess>,
}

impl NativeLocalObjects {
    fn new(git: &crate::git::Git, primary: PathBuf, alternates: OsString) -> Result<Self> {
        let version = git.output(None, &[OsStr::new("--version")])?;
        let version = std::str::from_utf8(&version)
            .ok()
            .and_then(|text| text.strip_prefix("git version "))
            .and_then(|text| text.split_whitespace().next())
            .and_then(|text| text.split_once('.'))
            .and_then(|(major, rest)| {
                Some((
                    major.parse::<u32>().ok()?,
                    rest.split('.').next()?.parse().ok()?,
                ))
            });
        let start = |mode| {
            let mut command = Self::command(git, &primary, &alternates);
            command.arg(mode);
            crate::transport::BatchProcess::spawn(command)
        };
        let batch = if version.is_some_and(|version| version >= (2, 36)) {
            NativeObjectBatch {
                contents: start("--batch-command")?,
                headers: None,
            }
        } else {
            let mut contents = start("--batch")?;
            let headers = start("--batch-check")?;
            // Resolve both Git object stores during startup. The contents
            // process would otherwise see later changes to an alternate path.
            let absent = Oid::zero();
            let _ = contents.exchange(format!("{absent}\n").as_bytes(), |input| {
                Self::receive(absent, true, input, |_, _, _| Ok(()))
            })?;
            NativeObjectBatch {
                contents,
                headers: Some(headers),
            }
        };
        Ok(Self {
            batch: Mutex::new(batch),
            #[cfg(feature = "test-metrics")]
            git: git.clone(),
            #[cfg(feature = "test-metrics")]
            primary,
            #[cfg(feature = "test-metrics")]
            alternates,
        })
    }

    fn command(git: &crate::git::Git, primary: &Path, alternates: &OsStr) -> std::process::Command {
        let mut command = git.command(None);
        command
            .arg("cat-file")
            .env("GIT_OBJECT_DIRECTORY", primary)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates)
            // Older Git versions do not honor GIT_NO_LAZY_FETCH.
            .env("GIT_ALLOW_PROTOCOL", "");
        command
    }

    fn header(&self, oid: Oid) -> Result<(usize, ObjectType)> {
        self.cat_file(oid, false, |size, kind, _| Ok((size, kind)))
    }

    fn read<T>(
        &self,
        oid: Oid,
        consume: impl FnOnce(usize, ObjectType, &mut dyn BufRead) -> Result<T>,
    ) -> Result<T> {
        self.cat_file(oid, true, consume)
    }

    fn bytes(&self, oid: Oid, expected: ObjectType) -> Result<Vec<u8>> {
        self.read(oid, |size, kind, input| {
            anyhow::ensure!(kind == expected, "Git object type changed during transfer");
            let mut payload = Vec::new();
            payload.try_reserve_exact(size)?;
            let mut verified = crate::object::VerifiedWriter::new(&mut payload, kind, size, oid);
            loop {
                crate::object::check_transfer()?;
                let bytes = input.fill_buf()?;
                if bytes.is_empty() {
                    break;
                }
                verified.write_all(bytes)?;
                let count = bytes.len();
                input.consume(count);
            }
            verified.finish()?;
            Ok(payload)
        })
    }

    fn cat_file<T>(
        &self,
        oid: Oid,
        body: bool,
        consume: impl FnOnce(usize, ObjectType, &mut dyn BufRead) -> Result<T>,
    ) -> Result<T> {
        let mut batch = self.batch.lock().unwrap();
        let NativeObjectBatch { contents, headers } = &mut *batch;
        let (process, request) = if let Some(headers) = headers {
            (if body { contents } else { headers }, format!("{oid}\n"))
        } else {
            let verb = if body { "contents" } else { "info" };
            (contents, format!("{verb} {oid}\n"))
        };
        process.exchange(request.as_bytes(), |input| {
            Self::receive(oid, body, input, consume)
        })?
    }

    fn receive<T>(
        oid: Oid,
        body: bool,
        mut input: &mut dyn BufRead,
        consume: impl FnOnce(usize, ObjectType, &mut dyn BufRead) -> Result<T>,
    ) -> Result<Result<T>> {
        let mut header = String::new();
        (&mut input).take(128).read_line(&mut header)?;
        anyhow::ensure!(header.ends_with('\n'), "invalid native Git object header");
        let mut fields = header.split_ascii_whitespace();
        anyhow::ensure!(
            fields.next().and_then(|value| Oid::from_str(value).ok()) == Some(oid),
            "native Git returned a different object"
        );
        let kind = fields.next().context("missing native Git object type")?;
        if kind == "missing" && fields.next().is_none() {
            return Ok(Err(anyhow::anyhow!("missing local Git object {oid}")));
        }
        let kind = ObjectType::from_str(kind).context("invalid native Git object type")?;
        let size: usize = fields.next().context("missing Git object size")?.parse()?;
        anyhow::ensure!(fields.next().is_none(), "invalid native Git object header");
        let mut data = (&mut input).take(if body { size as u64 } else { 0 });
        let value = consume(size, kind, &mut data);
        if body {
            // A consumer error need not invalidate the local object store.
            // Finish the response so the next request starts at its header.
            if let Err(error) = &value {
                if crate::object::is_cancelled(error) {
                    return Err(anyhow::anyhow!(crate::object::OperationCancelled));
                }
                io::copy(&mut data, &mut io::sink())?;
            }
            anyhow::ensure!(data.limit() == 0, "truncated native Git object");
            let mut newline = [0];
            input.read_exact(&mut newline)?;
            anyhow::ensure!(newline == *b"\n", "invalid native Git object delimiter");
        }
        Ok(value)
    }

    fn stop(&self) -> Result<()> {
        let batch = self.batch.lock().unwrap();
        let contents = batch.contents.stop();
        let headers = batch
            .headers
            .as_ref()
            .map_or(Ok(()), crate::transport::BatchProcess::stop);
        contents.and(headers)
    }

    #[cfg(feature = "test-metrics")]
    fn objects(&self) -> Result<Vec<Oid>> {
        let mut command = Self::command(&self.git, &self.primary, &self.alternates);
        command
            .args(["--batch-all-objects", "--batch-check=%(objectname)"])
            .stdin(std::process::Stdio::null());
        let output = crate::git::run_builtin(command)?;
        anyhow::ensure!(
            output.status.success(),
            "native Git could not enumerate objects"
        );
        std::str::from_utf8(&output.stdout)?
            .lines()
            .map(|line| Ok(Oid::from_str(line)?))
            .collect()
    }
}

fn needs_native_alternates(path: &Path, depth: usize, visited: &mut HashSet<PathBuf>) -> bool {
    // Git and libgit2 both bound recursion. Inspect up to the same number
    // of links even when a deeper alternate would repeat an earlier location.
    if depth > 6 {
        return false;
    }
    let bytes = match fs::read(path.join("info/alternates")) {
        Ok(bytes) => bytes,
        Err(error) => return error.kind() != io::ErrorKind::NotFound,
    };
    let Ok(path) = path.canonicalize() else {
        return true;
    };
    if !visited.insert(path.clone()) {
        return false;
    }
    bytes.split(|&byte| byte == b'\n').any(|line| {
        if line.is_empty() || line.starts_with(b"#") {
            return false;
        }
        if !matches!(line[0], b'/' | b'.') || line.contains(&b'\r') || line.contains(&0) {
            return true;
        }
        needs_native_alternates(&path.join(OsStr::from_bytes(line)), depth + 1, visited)
    })
}

fn primary_has_disk_alternates(path: &Path) -> bool {
    match fs::read(path.join("info/alternates")) {
        // libgit2 also splits at carriage returns. Keep ambiguous lists on
        // the native path even if every LF-separated line appears to be a comment.
        Ok(bytes) => bytes
            .split(|&byte| byte == b'\n')
            .any(|line| line.contains(&b'\r') || (!line.is_empty() && !line.starts_with(b"#"))),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// libgit2 handles relative object environments before Git's repository setup
/// and splits alternate paths on every colon. Configure its ODB with the
/// object locations selected by native Git.
fn configure_local_object_database(
    git: &crate::git::Git,
    repo: &Repository,
    run_directory: &Path,
) -> Result<Option<LocalObjectAnchor>> {
    let alternates =
        std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES").filter(|value| !value.is_empty());
    let object_directory = std::env::var_os("GIT_OBJECT_DIRECTORY");
    if alternates.is_none()
        && object_directory.as_deref().is_none_or(|path| {
            let path = Path::new(path);
            path.is_absolute()
                && fs::canonicalize(path)
                    .as_deref()
                    .is_ok_and(|name| name == path)
        })
    {
        // FROM_ENV already supplies this resolved primary path byte for byte
        // at the correct depth for its own info/alternates.
        let primary = object_directory
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.commondir().join("objects"));
        return if needs_native_alternates(&primary, 0, &mut HashSet::new()) {
            NativeLocalObjects::new(git, primary, OsString::new())
                .map(Box::new)
                .map(LocalObjectAnchor::Native)
                .map(Some)
        } else {
            Ok(None)
        };
    }
    let primary = git.output(
        None,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-path"),
            OsStr::new("objects"),
        ],
    )?;
    let primary = PathBuf::from(OsString::from_vec(
        primary.strip_suffix(b"\n").unwrap_or(&primary).to_vec(),
    ));
    let mut locations = vec![primary];
    let mut cwd = git.cwd.canonicalize()?;
    if alternates.is_some() {
        let prefix = git.output(
            None,
            &[OsStr::new("rev-parse"), OsStr::new("--show-prefix")],
        )?;
        // Git moves from a worktree subdirectory to its top level. Bare
        // invocations and explicit GIT_DIR calls outside a worktree retain the
        // invocation cwd, even when --show-cdup prints a worktree path.
        if !prefix.strip_suffix(b"\n").unwrap_or(&prefix).is_empty() {
            let cdup = git.output(None, &[OsStr::new("rev-parse"), OsStr::new("--show-cdup")])?;
            cwd = cwd
                .join(PathBuf::from(OsString::from_vec(
                    cdup.strip_suffix(b"\n").unwrap_or(&cdup).to_vec(),
                )))
                .canonicalize()?;
        }
    }
    let mut remaining = alternates
        .as_ref()
        .map_or(&[][..], |value| value.as_bytes());
    while !remaining.is_empty() {
        let end = remaining
            .iter()
            .position(|byte| *byte == b':')
            .unwrap_or(remaining.len());
        let decoded = remaining
            .starts_with(b"\"")
            .then(|| gix_quote::ansi_c::undo(remaining.into()))
            .and_then(Result::ok)
            .filter(|(_, end)| closed_c_quote(&remaining[..*end]));
        let (value, consumed) = if remaining.starts_with(b"#") {
            (Vec::new(), end)
        } else if let Some((value, consumed)) = decoded {
            (value.as_ref().to_vec(), consumed)
        } else {
            // Native Git likewise falls back to the ordinary component when
            // quoting is broken; missing paths are ignored by its ODB loader.
            (remaining[..end].to_vec(), end)
        };
        remaining = &remaining[consumed..];
        if !remaining.is_empty() {
            remaining = &remaining[1..];
        }
        // Git's path normalization takes a NUL-terminated pathname.
        let value = value.split(|&byte| byte == 0).next().unwrap_or_default();
        if !value.is_empty() {
            let path = Path::new(OsStr::from_bytes(value));
            if let Ok(path) = cwd.join(path).canonicalize() {
                locations.push(path);
            }
        }
    }
    let mut visited = HashSet::new();
    let primary_needs_own_depth =
        alternates.is_some() && primary_has_disk_alternates(&locations[0]);
    if primary_needs_own_depth
        || locations.iter().enumerate().any(|(index, path)| {
            needs_native_alternates(path, usize::from(index != 0), &mut visited)
        })
    {
        let primary = locations[0].canonicalize()?;
        return NativeLocalObjects::new(
            git,
            primary,
            normalized_alternate_environment(&locations[1..])
                .unwrap_or_else(|| alternates.unwrap_or_default()),
        )
        .map(Box::new)
        .map(LocalObjectAnchor::Native)
        .map(Some);
    }
    if alternates.is_none() && locations[0].to_str().is_none() {
        return configure_byte_primary(repo, &locations[0], run_directory).map(Some);
    }
    if let Some(paths) = locations
        .iter()
        .map(|path| path.to_str())
        .collect::<Option<Vec<_>>>()
        // The primary store starts at depth zero. Inherited alternates start
        // one level deeper, so nested environment entries still need a bridge.
        .filter(|_| {
            alternates.is_none()
                || locations
                    .iter()
                    .skip(1)
                    .all(|path| matches!(path.join("info/alternates").try_exists(), Ok(false)))
        })
    {
        let database = git2::Odb::new()?;
        for path in paths {
            database.add_disk_alternate(path)?;
        }
        repo.set_odb(&database)?;
        return Ok(None);
    }
    // A repository of aliases preserves the extra traversal step for nested
    // locations and adapts byte paths to git2's UTF-8 alternate API.
    let bridge = tempfile::Builder::new()
        .prefix("local-object-bridge-")
        .tempdir_in(run_directory)?;
    let private = Repository::init_bare(bridge.path())?;
    let mut aliases = String::new();
    for (index, location) in locations.into_iter().enumerate() {
        let name = format!("alternate-{index}");
        symlink(location, bridge.path().join(&name))?;
        aliases.push_str(&format!("../{name}\n"));
    }
    fs::write(bridge.path().join("objects/info/alternates"), aliases)?;
    repo.set_odb(&private.odb()?)?;
    Ok(Some(LocalObjectAnchor::Temporary { _directory: bridge }))
}

fn closed_c_quote(value: &[u8]) -> bool {
    value.len() > 1
        && value.ends_with(b"\"")
        && value[..value.len() - 1]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count()
            % 2
            == 0
}

fn normalized_alternate_environment(paths: &[PathBuf]) -> Option<OsString> {
    // Expanded paths can exceed exec's per-string limit. Keep the original
    // Git environment in that case so native Git visits the graph only once
    // and preserves its traversal order and nesting limit.
    const MAX_ALTERNATE_BYTES: usize = 32 * 1024;
    let mut list = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        if !seen.insert(path) {
            continue;
        }
        if !list.is_empty() {
            list.push(b':');
        }
        list.push(b'"');
        for &byte in path.as_os_str().as_bytes() {
            if matches!(byte, b'"' | b'\\') {
                list.push(b'\\');
            }
            list.push(byte);
        }
        list.push(b'"');
        if list.len() > MAX_ALTERNATE_BYTES {
            return None;
        }
    }
    Some(OsString::from_vec(list))
}

fn configure_byte_primary(
    repo: &Repository,
    primary: &Path,
    run_directory: &Path,
) -> Result<LocalObjectAnchor> {
    #[cfg(target_os = "linux")]
    {
        use std::os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        };

        let file = fs::File::options()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(primary)?;
        // The binding takes a UTF-8 string. This name addresses the resolved
        // byte path at primary depth without modifying any filesystem entry.
        let alias = format!("/proc/self/fd/{}", file.as_raw_fd());
        let identity = file.metadata()?;
        if fs::metadata(&alias)
            .is_ok_and(|alias| (alias.dev(), alias.ino()) == (identity.dev(), identity.ino()))
        {
            let database = git2::Odb::new()?;
            database.add_disk_alternate(&alias)?;
            repo.set_odb(&database)?;
            return Ok(LocalObjectAnchor::Descriptor { _file: file });
        }
    }

    // Make the selected store the object directory itself. Registering it
    // through an alternates list would lose one level of primary traversal.
    let bridge = tempfile::Builder::new()
        .prefix("local-object-bridge-")
        .tempdir_in(run_directory)?;
    fs::create_dir(bridge.path().join("refs"))?;
    fs::write(bridge.path().join("HEAD"), b"ref: refs/heads/main\n")?;
    symlink(primary, bridge.path().join("objects"))?;
    repo.set_odb(&Repository::open_bare(bridge.path())?.odb()?)?;
    Ok(LocalObjectAnchor::Temporary { _directory: bridge })
}
/// Application-level demand requests, excluding initial tree-ish resolution.
/// libgit2's necessary pack/delta decoding is not separately observable here.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SnapshotMetrics {
    /// Successful explicit native fetches, including initial ref resolution.
    pub remote_fetches: u64,
    pub remote_fetch_operations: u64,
    pub remote_decode_operations: u64,
    pub remote_session_fetch_operations: u64,
    pub remote_session_decode_operations: u64,
    pub remote_session_starts: u64,
    pub remote_http2_fetch_operations: u64,
    pub remote_http2_peak_requests: u64,
    pub remote_streamed_packs: u64,
    pub remote_prefetch_packs: u64,
    pub remote_prefetched_blobs: u64,
    /// Native Git's retained received packs, possibly normalized by index-pack.
    pub remote_native_pack_bytes: u64,
    /// Exact received Git pack payloads on invocation-owned demand sessions.
    /// Neither counter includes protocol framing, TLS or other wire overhead.
    pub remote_session_pack_bytes: u64,
    /// Peak verified allocation capacity retained by the single eager pack.
    pub remote_eager_verified_pack_capacity: usize,
    pub tree_reads: u64,
    pub prepared_tree_entries: u64,
    pub verification_metadata_bytes: usize,
    pub header_reads: u64,
    pub blob_loads: u64,
    pub blob_bytes: u64,
    pub cache_hits: u64,
    pub blob_cache_bytes: usize,
    pub blob_cache_entries: usize,
    pub hot_blob_cache_bytes: usize,
    pub hot_blob_cache_entries: usize,
    pub hot_blob_spill_bytes: usize,
    pub verified_decode_buffer_bytes: usize,
    pub blob_cache_limit: usize,
    pub tree_cache_bytes: usize,
    pub tree_cache_entries: usize,
    pub tree_cache_limit: usize,
    pub header_cache_entries: usize,
    pub active_spool_bytes: u64,
    pub active_blob_readers: usize,
    pub streamed_blob_loads: u64,
    /// Raw payload File::write calls, including failed attempts.
    pub spool_writes: u64,
    /// Bytes accepted by raw payload writes, including unpublished partial data.
    /// These counters exclude transport/metadata staging and durability work.
    pub spool_bytes: u64,
    pub retained_raw_blob_bytes: u64,
    pub retained_raw_blob_entries: u64,
    pub remote_raw_blob_imports: u64,
    pub remote_raw_blob_bytes: u64,
    #[cfg(feature = "test-metrics")]
    pub passthrough_handles: u64,
    #[cfg(feature = "test-metrics")]
    pub generated_read_callbacks: u64,
    #[cfg(feature = "test-metrics")]
    pub generated_write_callbacks: u64,
    #[cfg(feature = "test-metrics")]
    pub file_sync_callbacks: u64,
    #[cfg(feature = "test-metrics")]
    pub fuse_requests: BTreeMap<&'static str, u64>,
}

type ReaderRegistry = Mutex<HashMap<Oid, Weak<ActiveBlob>>>;
struct ActiveBlob {
    oid: Oid,
    data: crate::raw_blob::RawBlob,
    registry: Weak<ReaderRegistry>,
}
impl Drop for ActiveBlob {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
            if registry
                .get(&self.oid)
                .is_some_and(|reader| std::ptr::eq(reader.as_ptr(), self))
            {
                registry.remove(&self.oid);
            }
        }
    }
}
/// A live reader of immutable run-local objects. Closing a reader does not
/// release the object's verified contents.
#[derive(Clone)]
pub struct BlobReader(Arc<ActiveBlob>);
impl BlobReader {
    pub fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.0.data.read(offset, size)
    }
    pub(crate) fn read_shared(&self, offset: u64, size: u32) -> Result<crate::raw_blob::ReadData> {
        self.0.data.read_shared(offset, size)
    }
    pub(crate) fn len(&self) -> u64 {
        self.0.data.len()
    }
}

struct State {
    local_repo: Option<Repository>,
    remote: Option<crate::remote::Remote>,
    selected_oid: Oid,
    headers: HashMap<Oid, u64>,
    readers: Arc<ReaderRegistry>,
    raw_blobs: Option<crate::raw_blob::RawBlobs>,
    metrics: SnapshotMetrics,
    #[cfg(test)]
    ancestor_tree_lookups: u64,
    spool_directory: PathBuf,
    _local_object_anchor: Option<LocalObjectAnchor>,
}
impl State {
    fn native_local(&self) -> Option<&NativeLocalObjects> {
        match &self._local_object_anchor {
            Some(LocalObjectAnchor::Native(native)) => Some(native),
            _ => None,
        }
    }
    fn repository(&self) -> Option<&Repository> {
        self.local_repo.as_ref().or_else(|| {
            self.remote
                .as_ref()
                .and_then(|remote| remote.native_repository())
        })
    }
    fn object_kind(&self, oid: Oid) -> Result<ObjectType> {
        if let Some(native) = self.native_local() {
            return native.header(oid).map(|(_, kind)| kind);
        }
        if let Some(remote) = &self.remote {
            if let Some(raw) = remote.raw_blobs.get(oid)? {
                return Ok(raw.kind());
            }
            if let Some(crate::raw_blob::ReceivedObject::Verified { object, transfer }) =
                remote.raw_blobs.received(oid)
            {
                if let Some(transfer) = transfer {
                    transfer.mark_served();
                }
                return Ok(object.kind());
            }
        }
        let repo = self.repository().context("missing verified Git object")?;
        Ok(repo.odb()?.read_header(oid)?.1)
    }
    fn selected_child(&self, oid: Oid, kind: ObjectType) -> Result<Oid> {
        if let Some(native) = self.native_local() {
            return crate::object::selected_child(kind, &native.bytes(oid, kind)?);
        }
        if let Some(repo) = self.repository() {
            return match kind {
                ObjectType::Commit => Ok(repo.find_commit(oid)?.tree_id()),
                ObjectType::Tag => Ok(repo.find_tag(oid)?.target_id()),
                _ => bail!("revision does not resolve to a tree"),
            };
        }
        let remote = self.remote.as_ref().expect("in-memory remote snapshot");
        let Some(crate::raw_blob::ReceivedObject::Verified { object, transfer }) =
            remote.raw_blobs.received(oid)
        else {
            bail!("missing verified Git metadata {oid}");
        };
        let child = crate::object::selected_child(kind, object.data())?;
        if let Some(transfer) = transfer {
            transfer.mark_served();
        }
        Ok(child)
    }
    fn ensure(&mut self, oid: Oid) -> Result<()> {
        if let Some(remote) = &mut self.remote {
            remote.ensure(oid)?;
        }
        Ok(())
    }
    fn raw_store(&self) -> Option<&crate::raw_blob::RawBlobs> {
        self.remote
            .as_ref()
            .map(|remote| &remote.raw_blobs)
            .or(self.raw_blobs.as_ref())
    }
    fn tree(&mut self, oid: Oid) -> Result<Arc<Directory>> {
        if let Some((tree, first)) = self.raw_store().and_then(|raw| raw.directory(oid)) {
            if first {
                self.metrics.tree_reads += 1;
            } else {
                self.metrics.cache_hits += 1;
            }
            return Ok(tree);
        }
        self.ensure(oid)?;
        if let Some((tree, first)) = self.raw_store().and_then(|raw| raw.directory(oid)) {
            self.metrics.tree_reads += u64::from(first);
            return Ok(tree);
        }
        if self.remote.is_none() && self.raw_blobs.is_none() {
            self.raw_blobs = Some(crate::raw_blob::RawBlobs::new(&self.spool_directory)?);
        }
        let (tree, first) = {
            let storage = self.raw_store().expect("directory cache");
            if let Some(native) = self.native_local() {
                let (size, directory) = native.read(oid, |size, kind, input| {
                    anyhow::ensure!(
                        kind == ObjectType::Tree,
                        "snapshot directory does not reference a tree"
                    );
                    Ok((size, Directory::verified(oid, size, input)?))
                })?;
                storage.insert_prepared_tree(oid, size, directory)?;
            } else {
                let odb = self
                    .repository()
                    .context("missing verified directory")?
                    .odb()?;
                if let Ok((reader, size, kind)) = odb.reader(oid) {
                    anyhow::ensure!(
                        kind == ObjectType::Tree,
                        "snapshot directory does not reference a tree"
                    );
                    storage.insert_tree(oid, size, reader)?;
                } else {
                    let object = odb.read(oid).context("decode snapshot tree")?;
                    storage.insert_odb_tree(oid, &object)?;
                };
            }
            storage
                .directory(oid)
                .context("verified directory missing")?
        };
        self.metrics.tree_reads += u64::from(first);
        Ok(tree)
    }
    fn size(&mut self, oid: Oid) -> Result<u64> {
        if let Some(size) = self.headers.get(&oid).copied() {
            self.metrics.cache_hits += 1;
            return Ok(size);
        }
        self.metrics.header_reads += 1;
        self.ensure(oid)?;
        if let Some(raw) = self
            .remote
            .as_ref()
            .map(|remote| remote.raw_blobs.get(oid))
            .transpose()?
            .flatten()
            .or(self
                .raw_blobs
                .as_ref()
                .map(|raw| raw.get(oid))
                .transpose()?
                .flatten())
        {
            anyhow::ensure!(
                raw.kind() == ObjectType::Blob,
                "snapshot file does not reference a blob"
            );
            let size = raw.len();
            self.headers.insert(oid, size);
            return Ok(size);
        }
        let (size, kind) = if let Some(native) = self.native_local() {
            native.header(oid)?
        } else {
            self.repository()
                .context("missing verified Git blob")?
                .odb()?
                .read_header(oid)
                .context("read blob header")?
        };
        if kind != ObjectType::Blob {
            bail!("snapshot file does not reference a blob");
        }
        let size = size as u64;
        self.headers.insert(oid, size);
        Ok(size)
    }
    fn open(&mut self, oid: Oid) -> Result<BlobReader> {
        let existing = self
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&oid)
            .and_then(Weak::upgrade);
        if let Some(reader) = existing {
            self.metrics.cache_hits += 1;
            return Ok(BlobReader(reader));
        }
        let data = self.prepare_blob(oid)?;
        let reader = Arc::new(ActiveBlob {
            oid,
            data,
            registry: Arc::downgrade(&self.readers),
        });
        self.readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(oid, Arc::downgrade(&reader));
        Ok(BlobReader(reader))
    }
    /// Raw descriptors own no payload buffer or per-reader file descriptor.
    /// Only callers retaining a BlobReader need active-reader registration.
    fn prepare_blob(&mut self, oid: Oid) -> Result<crate::raw_blob::RawBlob> {
        let raw = if let Some(remote) = &mut self.remote {
            remote.ensure(oid)?;
            let (raw, first) = remote
                .raw_blobs
                .for_read(oid)?
                .context("snapshot file does not reference a blob")?;
            if first {
                self.metrics.blob_loads += 1;
                self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(raw.len());
            } else {
                self.metrics.cache_hits += 1;
            }
            raw
        } else if let Some(raw) = self
            .raw_blobs
            .as_ref()
            .map(|raw| raw.get(oid))
            .transpose()?
            .flatten()
        {
            self.metrics.cache_hits += 1;
            raw
        } else {
            if self.raw_blobs.is_none() {
                self.raw_blobs = Some(crate::raw_blob::RawBlobs::new(&self.spool_directory)?);
            }
            let raw = self.raw_blobs.as_mut().unwrap();
            let (blob, size, streamed) =
                if let Some(LocalObjectAnchor::Native(native)) = &self._local_object_anchor {
                    let payload = native.bytes(oid, ObjectType::Blob)?;
                    let size = payload.len();
                    (raw.insert_prepared_blob(oid, payload)?, size, true)
                } else {
                    let odb = self.local_repo.as_ref().unwrap().odb()?;
                    let (size, kind) = odb.read_header(oid)?;
                    anyhow::ensure!(
                        kind == ObjectType::Blob,
                        "snapshot file does not reference a blob"
                    );
                    let streamed = odb.reader(oid);
                    if let Ok((reader, length, kind)) = streamed {
                        anyhow::ensure!(
                            kind == ObjectType::Blob && length == size,
                            "Git blob changed during transfer"
                        );
                        (raw.insert(oid, size, reader)?, size, true)
                    } else {
                        let object = odb.read(oid).context("decode Git blob")?;
                        anyhow::ensure!(
                            object.kind() == ObjectType::Blob && object.len() == size,
                            "Git blob changed during transfer"
                        );
                        (raw.insert_odb_blob(oid, object)?, size, false)
                    }
                };
            self.metrics.blob_loads += 1;
            self.metrics.blob_bytes = self.metrics.blob_bytes.saturating_add(size as u64);
            if size > crate::raw_blob::BLOCK {
                self.metrics.streamed_blob_loads += u64::from(streamed);
            }
            blob
        };
        anyhow::ensure!(
            raw.kind() == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        // Only a complete LFS pointer can need this bounded validation. A cold
        // partial read must not hydrate or authenticate the whole object.
        if raw.len() < 1024 {
            raw.check_content_once(Self::validate_blob)?;
        }
        Ok(raw)
    }
    fn validate_blob(bytes: &[u8]) -> Result<()> {
        if is_lfs_pointer(bytes) {
            bail!("Git LFS pointer blobs are unsupported");
        }
        Ok(())
    }
    fn copy_blob_reader(
        oid: Oid,
        size: usize,
        kind: ObjectType,
        input: impl Read,
        output: &mut dyn Write,
    ) -> Result<()> {
        anyhow::ensure!(
            kind == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        if size < 1024 {
            let mut small = Vec::with_capacity(size);
            crate::object::copy_verified(input, &mut small, kind, size, oid)?;
            Self::validate_blob(&small)?;
            output.write_all(&small)?;
        } else {
            crate::object::copy_verified(input, output, kind, size, oid)?;
        }
        Ok(())
    }
    /// Full-object consumers explicitly need the returned bytes (ignore rules
    /// and symlink targets). Ordinary ranged reads use BlobReader directly.
    fn with_blob<T>(&mut self, oid: Oid, consume: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        let reader = self.open(oid)?;
        consume(&reader.0.data.read_all()?.contiguous())
    }
}
struct Inner {
    state: Mutex<State>,
    pack_verification: Option<crate::demand::PackVerification>,
}
/// Clones share the selected tree and its object caches.
#[derive(Clone)]
pub struct Snapshot {
    pub tree_oid: Oid,
    pub git_dir: PathBuf,
    inner: Arc<Inner>,
}

pub(crate) struct PendingObject {
    snapshot: Snapshot,
    demand: crate::demand::Demand,
}

impl PendingObject {
    pub(crate) fn finish(self) {
        let batch = self.demand.wait();
        if let Ok(mut state) = self.snapshot.state() {
            if let Some(remote) = &mut state.remote {
                // The normal filesystem operation returns its usual error
                // and the snapshot retains any transport/import failure.
                let _ = remote.finish_demand(&batch);
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn ready(&self) -> bool {
        self.demand.ready()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn notify_when_ready(&self, notify: &Arc<tokio::sync::Notify>) {
        self.demand.notify_when_ready(notify);
    }
}
impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("tree_oid", &self.tree_oid)
            .field("git_dir", &self.git_dir)
            .finish_non_exhaustive()
    }
}
impl Snapshot {
    pub fn load(repo_path: &Path, treeish: &OsStr, _run_id: &str) -> Result<Self> {
        Self::load_source(
            repo_path,
            None,
            treeish,
            false,
            false,
            &std::env::temp_dir(),
        )
    }

    /// Load a snapshot and retain its resolved tree in memory. The former run
    /// ID and pin callback are accepted for source compatibility.
    #[allow(clippy::too_many_arguments)]
    pub fn load_source_with_pin_record<F>(
        repo_path: &Path,
        source: Option<&OsStr>,
        treeish: &OsStr,
        verify: bool,
        materialized: bool,
        run_directory: &Path,
        _run_id: &str,
        _record: F,
    ) -> Result<Self>
    where
        F: FnMut(&Path, &str, Oid) -> Result<()>,
    {
        Self::load_source(
            repo_path,
            source,
            treeish,
            verify,
            materialized,
            run_directory,
        )
    }

    /// Open the selected tree for one run. Native remote storage, if needed,
    /// belongs to that run and its Git commands disable automatic maintenance.
    pub fn load_source(
        repo_path: &Path,
        source: Option<&OsStr>,
        treeish: &OsStr,
        verify: bool,
        materialized: bool,
        run_directory: &Path,
    ) -> Result<Self> {
        let (local_repo, remote, oid, git, local_anchor) = if let Some(source) = source {
            let (remote, oid) = crate::remote::Remote::open_for_execution(
                repo_path,
                source,
                treeish,
                run_directory,
                materialized,
                verify,
            )?;
            (None, Some(remote), oid, None, None)
        } else {
            let (git, oid) = crate::git::Git::capture_snapshot(repo_path, treeish)?;
            // Native Git resolved the operand in the caller's object namespace.
            // Keep that environment for subsequent lazy libgit2 object reads.
            let repo = Repository::open_ext(
                git.repository_dir()?,
                RepositoryOpenFlags::FROM_ENV,
                &[] as &[&OsStr],
            )
            .context("open selected Git repository and object environment")?;
            let anchor = configure_local_object_database(&git, &repo, run_directory)?;
            (Some(repo), None, oid, Some(git), anchor)
        };
        let mut state = State {
            local_repo,
            remote,
            selected_oid: oid,
            headers: HashMap::new(),
            readers: Arc::new(Mutex::new(HashMap::new())),
            raw_blobs: None,
            metrics: SnapshotMetrics::default(),
            #[cfg(test)]
            ancestor_tree_lookups: 0,
            spool_directory: run_directory.to_path_buf(),
            _local_object_anchor: local_anchor,
        };
        if verify {
            let (git, directory) = if let Some(remote) = &state.remote {
                let repo = remote
                    .native_repository()
                    .context("native Git verification repository missing")?;
                (remote.git.as_ref(), repo.path())
            } else {
                let git = git.as_ref().expect("captured local Git repository");
                (git, git.repository_dir()?)
            };
            git.verify(directory, oid, state.object_kind(oid)?)?;
        }
        let mut selected = oid;
        let tree_oid = loop {
            state.ensure(selected)?;
            match state.object_kind(selected)? {
                ObjectType::Tree => break selected,
                ObjectType::Commit => {
                    let tree = state.selected_child(selected, ObjectType::Commit)?;
                    state.ensure(tree)?;
                    anyhow::ensure!(
                        state.object_kind(tree)? == ObjectType::Tree,
                        "snapshot root is not a Git tree"
                    );
                    break tree;
                }
                ObjectType::Tag => selected = state.selected_child(selected, ObjectType::Tag)?,
                _ => bail!("revision does not resolve to a tree"),
            }
        };
        let git_dir = if let Some(remote) = &state.remote {
            remote.dir.clone()
        } else {
            state.local_repo.as_ref().unwrap().path().to_path_buf()
        };
        let pack_verification = state
            .remote
            .as_ref()
            .map(|remote| remote.pack_verification.clone());
        Ok(Self {
            tree_oid,
            git_dir,
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                pack_verification,
            }),
        })
    }
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.inner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot object cache poisoned"))
    }
    /// A successful command must wait for the packs that supplied its reads.
    pub fn verify_used_packs(&self) -> Result<()> {
        self.inner
            .pack_verification
            .as_ref()
            .map_or(Ok(()), crate::demand::PackVerification::wait)
    }

    /// The native mount is ready and startup can no longer fall back to copy.
    pub fn begin_mounted_reads(&self) {
        if let Some(verification) = &self.inner.pack_verification {
            verification.mounted();
        }
    }

    pub(crate) fn begin_directory_enumeration(&self) {
        if let Some(verification) = &self.inner.pack_verification {
            verification.enumerating();
        }
    }

    pub fn stop_remote_transport(&self) -> Result<()> {
        crate::git::stop_operations();
        let state = self.state()?;
        let transport = state
            .remote
            .as_ref()
            .map_or(Ok(()), crate::remote::Remote::stop_transport);
        let native = state
            .native_local()
            .map_or(Ok(()), NativeLocalObjects::stop);
        let cache = state.raw_store().map_or(Ok(()), |raw| raw.stop());
        transport.and(native).and(cache)
    }
    pub fn remote_failure(&self) -> Option<String> {
        self.state()
            .ok()
            .and_then(|s| s.remote.as_ref().and_then(|r| r.failure()))
    }
    pub(crate) fn reusable_remote_transport(&self) -> bool {
        self.state().is_ok_and(|state| {
            state
                .remote
                .as_ref()
                .is_some_and(|remote| remote.reusable_transport())
        })
    }
    pub(crate) fn start_remote_demand<I: Iterator<Item = Oid>>(
        &self,
        oid: Oid,
        immediate: bool,
        candidates: Option<impl FnOnce(bool) -> I>,
    ) -> Option<PendingObject> {
        let mut state = self.state().ok()?;
        if state.headers.contains_key(&oid) {
            return None;
        }
        let demand = state
            .remote
            .as_mut()?
            .start_demand(oid, immediate, candidates)
            .ok()??;
        Some(PendingObject {
            snapshot: self.clone(),
            demand,
        })
    }
    fn metadata(mode: u32, oid: Oid, display_path: impl FnOnce() -> String) -> Result<Entry> {
        let kind = match mode {
            0o040000 => EntryKind::Directory,
            0o100644 | 0o100755 => EntryKind::File,
            0o120000 => EntryKind::Symlink,
            0o160000 => bail!("submodules are unsupported: {}", display_path()),
            mode => bail!("unsupported Git mode {mode:o}: {}", display_path()),
        };
        Ok(Entry {
            kind,
            mode,
            oid,
            size: (kind == EntryKind::Directory).then_some(0),
        })
    }
    fn lookup(&self, state: &mut State, path: &Path) -> Result<Option<Entry>> {
        let mut entry = Entry {
            kind: EntryKind::Directory,
            mode: 0o040000,
            oid: self.tree_oid,
            size: Some(0),
        };
        for component in path.components() {
            let Component::Normal(name) = component else {
                bail!("snapshot path must be relative and contain no parent components");
            };
            if entry.kind != EntryKind::Directory {
                bail!("snapshot ancestor is not a directory: {}", path.display());
            }
            #[cfg(test)]
            {
                state.ancestor_tree_lookups += 1;
            }
            let tree = state.tree(entry.oid)?;
            let Some((mode, oid)) = tree.lookup(name) else {
                return Ok(None);
            };
            entry = Self::metadata(mode, oid, || path.display().to_string())?;
        }
        Ok(Some(entry))
    }
    #[cfg(test)]
    pub(crate) fn ancestor_tree_lookups_for_test(&self) -> u64 {
        self.state().unwrap().ancestor_tree_lookups
    }
    pub(crate) fn entry_metadata(&self, path: &Path) -> Result<Option<Entry>> {
        self.lookup(&mut *self.state()?, path)
    }
    pub(crate) fn blob_size(&self, oid: Oid) -> Result<u64> {
        self.state()?.size(oid)
    }
    /// Resolve only a path's ancestor trees and requested object's header.
    pub fn entry(&self, path: &Path) -> Result<Option<Entry>> {
        let mut state = self.state()?;
        let Some(mut entry) = self.lookup(&mut state, path)? else {
            return Ok(None);
        };
        if entry.kind != EntryKind::Directory {
            entry.size = Some(state.size(entry.oid)?);
        }
        Ok(Some(entry))
    }
    /// Resolve a child of an already verified directory without walking its
    /// ancestors again. Sizes remain demand-only, just as for pathname lookup.
    pub(crate) fn directory_entry(
        &self,
        directory: &Directory,
        index: usize,
        path: &Path,
    ) -> Result<Entry> {
        let mut entry = Self::directory_entry_metadata(directory, index, path)?;
        if entry.kind != EntryKind::Directory {
            entry.size = Some(self.blob_size(entry.oid)?);
        }
        Ok(entry)
    }
    pub(crate) fn directory_entry_metadata(
        directory: &Directory,
        index: usize,
        path: &Path,
    ) -> Result<Entry> {
        let (mode, oid) = directory
            .metadata_at(index)
            .context("directory entry missing")?;
        Self::metadata(mode, oid, || path.display().to_string())
    }
    /// Share a verified directory without copying names or reading children.
    pub(crate) fn directory(&self, path: &Path) -> Result<Arc<Directory>> {
        let mut state = self.state()?;
        let entry = self
            .lookup(&mut state, path)?
            .context("snapshot directory not found")?;
        if entry.kind != EntryKind::Directory {
            bail!("snapshot path is not a directory: {}", path.display());
        }
        state.tree(entry.oid)
    }
    /// The caller already resolved the parent's identity from a verified tree.
    pub(crate) fn directory_by_oid(&self, oid: Oid) -> Result<Arc<Directory>> {
        self.state()?.tree(oid)
    }

    pub(crate) fn directory_by_oid_if_available(&self, oid: Oid) -> Result<Option<Arc<Directory>>> {
        let mut state = self.state()?;
        if let Some((tree, first)) = state.raw_store().and_then(|raw| raw.directory(oid)) {
            state.metrics.tree_reads += u64::from(first);
            state.metrics.cache_hits += u64::from(!first);
            return Ok(Some(tree));
        }
        if let Some(repo) = state
            .repository()
            .filter(|_| state.native_local().is_none())
        {
            if repo.odb()?.exists(oid) {
                return state.tree(oid).map(Some);
            }
        }
        Ok(None)
    }
    /// Enumerate one directory without reading child tree objects or blob headers.
    pub fn children(&self, path: &Path) -> Result<Vec<(OsString, Entry)>> {
        let directory = self.directory(path)?;
        Self::directory_children(&directory, path)
            .map(|entry| entry.map(|(name, entry)| (name.to_os_string(), entry)))
            .collect()
    }

    pub(crate) fn directory_children<'a>(
        directory: &'a Directory,
        path: &'a Path,
    ) -> impl Iterator<Item = Result<(&'a OsStr, Entry)>> {
        directory.children().map(move |(name, mode, oid)| {
            let entry = if mode == 0o160000 {
                // A gitlink name may be listed, but lookup/stat/open of that
                // name must fail instead of presenting a checkout.
                Entry {
                    kind: EntryKind::Directory,
                    mode,
                    oid,
                    size: Some(0),
                }
            } else {
                Self::metadata(mode, oid, || path.join(name).display().to_string())?
            };
            Ok((name, entry))
        })
    }
    /// Fetch a previously selected remote snapshot for automatic copy fallback.
    pub fn fetch_copy_objects(&self) -> Result<()> {
        let mut state = self.state()?;
        let selected = state.selected_oid;
        if let Some(remote) = &mut state.remote {
            remote.fetch_copy_objects(selected)?;
        }
        Ok(())
    }
    pub(crate) fn begin_materialization(&self) -> Result<()> {
        if let Some(remote) = &mut self.state()?.remote {
            remote.begin_materialization();
        }
        Ok(())
    }
    pub(crate) fn prepare_materialization(
        &self,
        children: &[(&OsStr, Entry)],
        prepared: impl Fn(Oid) -> bool,
    ) -> Result<()> {
        let mut state = self.state()?;
        let Some(remote) = &mut state.remote else {
            return Ok(());
        };
        // The materializer supplies bounded batches and reuses completed
        // destinations across batches/directories instead of fetching again.
        let oids = children
            .iter()
            .filter(|(_, entry)| entry.mode != 0o160000 && !prepared(entry.oid))
            .map(|(_, entry)| entry.oid);
        remote.ensure_batch(oids)
    }
    /// Call on the first actual read, then keep the reader for the open handle.
    pub fn set_spool_directory(&self, path: &Path) -> Result<()> {
        let mut state = self.state()?;
        // Own storage while the validated private directory exists.
        // Later upper-layer failures must not turn policy reads into unrelated
        // object-storage failures merely because that directory was removed.
        if state.remote.is_none() && state.raw_blobs.is_none() {
            // Named parts stay outside the execution view.
            // It remains inside the invocation's owned directory for recovery.
            let storage = path.parent().context("missing private storage parent")?;
            state.raw_blobs = Some(crate::raw_blob::RawBlobs::new(storage)?);
        }
        state.spool_directory = path
            .parent()
            .context("missing private storage parent")?
            .to_path_buf();
        Ok(())
    }
    pub fn open(&self, oid: Oid) -> Result<BlobReader> {
        self.state()?.open(oid)
    }
    /// Check into a private destination before publishing it to consumers.
    /// Reuse prepared contents; new local objects need no second raw copy.
    pub(crate) fn copy_verified_blob(
        &self,
        oid: Oid,
        output: &mut (dyn std::io::Write + Send),
    ) -> Result<u64> {
        let mut state = self.state()?;
        // An extra already has a verified buffer. Writing directly keeps an
        // eager copy from filling the lazy cache and spilling it to disk.
        if !state
            .remote
            .as_ref()
            .is_some_and(|remote| remote.has_received_object(oid))
        {
            state.ensure(oid)?;
        }
        if state
            .remote
            .as_ref()
            .map(|remote| remote.raw_blobs.get(oid))
            .transpose()?
            .flatten()
            .is_some()
            || state
                .raw_blobs
                .as_ref()
                .map(|raw| raw.get(oid))
                .transpose()?
                .flatten()
                .is_some()
        {
            let reader = state.open(oid)?;
            drop(state);
            let size = reader.len();
            let mut offset = 0;
            while offset < size {
                let data = reader.read_shared(offset, (size - offset).min(64 * 1024) as u32)?;
                anyhow::ensure!(!data.is_empty(), "truncated Git blob");
                crate::object::check_transfer()?;
                for chunk in data.chunks() {
                    output.write_all(chunk)?;
                }
                offset += data.len() as u64;
            }
            if size == 0 {
                reader.read_shared(0, 1)?;
            }
            return Ok(size);
        }
        if let Some(remote) = state
            .remote
            .as_mut()
            .filter(|remote| remote.has_received_object(oid))
        {
            let (size, streamed) =
                remote.copy_materialized_blob(oid, output, State::validate_blob)?;
            state.metrics.blob_loads += 1;
            state.metrics.blob_bytes = state.metrics.blob_bytes.saturating_add(size);
            state.metrics.streamed_blob_loads += u64::from(streamed);
            return Ok(size);
        }
        if let Some(native) = state.native_local() {
            let size = native.read(oid, |size, kind, input| {
                State::copy_blob_reader(oid, size, kind, input, output)?;
                Ok(size)
            })?;
            state.metrics.blob_loads += 1;
            state.metrics.blob_bytes = state.metrics.blob_bytes.saturating_add(size as u64);
            state.metrics.streamed_blob_loads += u64::from(size > crate::raw_blob::BLOCK);
            return Ok(size as u64);
        }
        let odb = state
            .repository()
            .context("missing verified Git blob")?
            .odb()?;
        let (size, streamed) = if let Ok((input, size, kind)) = odb.reader(oid) {
            State::copy_blob_reader(oid, size, kind, input, output)?;
            (size, true)
        } else {
            let object = odb.read(oid).context("decode Git blob")?;
            anyhow::ensure!(
                object.id() == oid && object.kind() == ObjectType::Blob,
                "Git blob changed during transfer"
            );
            let size = object.len();
            crate::object::copy_odb_blob(&object, output, |bytes| {
                if size < 1024 {
                    State::validate_blob(bytes)?;
                }
                Ok(())
            })?;
            (size, false)
        };
        drop(odb);
        state.metrics.blob_loads += 1;
        state.metrics.blob_bytes = state.metrics.blob_bytes.saturating_add(size as u64);
        if size > crate::raw_blob::BLOCK {
            state.metrics.streamed_blob_loads += u64::from(streamed);
        }
        Ok(size as u64)
    }
    pub(crate) fn with_materialized_symlink_target<T>(
        &self,
        oid: Oid,
        consume: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        if let Some(object) = self.take_verified_materialization_blob(oid)? {
            return consume(object.data());
        }
        let mut state = self.state()?;
        if let Some(repo) = state
            .local_repo
            .as_ref()
            .filter(|_| state.native_local().is_none())
        {
            let odb = repo.odb()?;
            let object = odb.read(oid).context("decode Git symlink target")?;
            anyhow::ensure!(object.id() == oid, "Git blob changed during transfer");
            crate::object::verify_odb_object(&object, ObjectType::Blob)?;
            State::validate_blob(object.data())?;
            let result = consume(object.data())?;
            let size = object.len() as u64;
            drop(object);
            drop(odb);
            state.metrics.blob_loads += 1;
            state.metrics.blob_bytes = state.metrics.blob_bytes.saturating_add(size);
            return Ok(result);
        }
        drop(state);
        let mut target = Vec::new();
        self.copy_verified_blob(oid, &mut target)?;
        consume(&target)
    }

    /// Move ownership of a verified pack buffer to the eager materializer.
    fn take_verified_materialization_blob(
        &self,
        oid: Oid,
    ) -> Result<Option<crate::pack_stream::VerifiedObject>> {
        let mut state = self.state()?;
        let Some(remote) = &state.remote else {
            return Ok(None);
        };
        let Some(crate::raw_blob::ReceivedObject::Verified { object, transfer }) =
            remote.raw_blobs.received(oid)
        else {
            return Ok(None);
        };
        anyhow::ensure!(
            object.kind() == ObjectType::Blob,
            "materialized file is not a blob"
        );
        State::validate_blob(object.data())?;
        if let Some(transfer) = transfer {
            transfer.mark_served();
        }
        remote.raw_blobs.forget_received(oid);
        state.metrics.blob_loads += 1;
        state.metrics.blob_bytes = state
            .metrics
            .blob_bytes
            .saturating_add(object.data().len() as u64);
        Ok(Some(object))
    }

    pub(crate) fn copy_up(
        &self,
        oid: Oid,
        path: &Path,
        retain: Option<u64>,
    ) -> Result<std::fs::File> {
        let raw = self.state()?.prepare_blob(oid)?;
        let size = retain.unwrap_or(raw.len()).min(raw.len());
        raw.copy_up(path, size)
    }
    pub fn read(&self, oid: Oid, offset: u64, size: u32) -> Result<Vec<u8>> {
        let raw = self.state()?.prepare_blob(oid)?;
        raw.read(offset, size)
    }
    pub(crate) fn read_shared(
        &self,
        oid: Oid,
        offset: u64,
        size: u32,
    ) -> Result<crate::raw_blob::ReadData> {
        // Release Snapshot's lock before I/O, retaining the invocation-owned
        // verified store without allocating/registering a temporary ActiveBlob.
        let raw = self.state()?.prepare_blob(oid)?;
        raw.read_shared(offset, size)
    }
    pub fn read_link(&self, oid: Oid) -> Result<Vec<u8>> {
        self.read_link_shared(oid)
            .map(crate::raw_blob::ReadData::into_vec)
    }
    pub(crate) fn read_link_shared(&self, oid: Oid) -> Result<crate::raw_blob::ReadData> {
        let raw = self.state()?.prepare_blob(oid)?;
        let bytes = raw.read_all()?;
        anyhow::ensure!(
            !bytes.is_empty() && !bytes.chunks().any(|chunk| chunk.contains(&0)),
            "unusable symlink target"
        );
        Ok(bytes)
    }
    /// Parse a demanded ignore blob directly, without retaining a second copy.
    pub(crate) fn with_blob<T>(
        &self,
        oid: Oid,
        consume: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        self.state()?.with_blob(oid, consume)
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn record_generated_io(&self, write: bool) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if write {
            state.metrics.generated_write_callbacks += 1;
        } else {
            state.metrics.generated_read_callbacks += 1;
        }
    }
    #[cfg(all(feature = "test-metrics", target_os = "linux"))]
    pub(crate) fn record_fuse_request(&self, operation: &'static str) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        *state.metrics.fuse_requests.entry(operation).or_default() += 1;
    }
    #[cfg(all(feature = "test-metrics", target_os = "linux"))]
    pub(crate) fn record_passthrough(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metrics
            .passthrough_handles += 1;
    }
    pub fn metrics(&self) -> SnapshotMetrics {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut metrics = state.metrics.clone();
        if let Some(remote) = &state.remote {
            metrics.remote_fetches = remote.completed_fetches();
            metrics.remote_fetch_operations = remote.fetch_operations();
            metrics.remote_decode_operations = remote.decode_operations();
            metrics.remote_eager_verified_pack_capacity = remote.eager_verified_pack_capacity();
            (
                metrics.remote_session_fetch_operations,
                metrics.remote_session_decode_operations,
                metrics.remote_session_starts,
            ) = remote.session_operations();
            (
                metrics.remote_http2_fetch_operations,
                metrics.remote_http2_peak_requests,
            ) = remote.http2_metrics();
            (
                metrics.remote_streamed_packs,
                metrics.remote_prefetch_packs,
                metrics.remote_prefetched_blobs,
            ) = remote.response_metrics();
            (
                metrics.remote_native_pack_bytes,
                metrics.remote_session_pack_bytes,
            ) = remote.received_pack_bytes();
            metrics.remote_raw_blob_imports = remote.raw_blobs.entries();
            metrics.remote_raw_blob_bytes = remote.raw_blobs.bytes();
            let (bytes, entries) = remote.raw_blobs.disk_metrics();
            metrics.retained_raw_blob_bytes = bytes;
            metrics.retained_raw_blob_entries = entries;
        }
        let (memory_bytes, memory_entries) = state
            .raw_store()
            .map_or((0, 0), |store| store.memory_metrics());
        metrics.blob_cache_bytes = memory_bytes;
        metrics.blob_cache_entries = memory_entries;
        metrics.blob_cache_limit = BLOB_CACHE_BYTES;
        if let Some(raw) = state.raw_store() {
            let (entries, bytes, count) = raw.tree_metrics();
            metrics.prepared_tree_entries = entries;
            metrics.tree_cache_bytes = bytes;
            metrics.tree_cache_entries = count;
            metrics.verification_metadata_bytes = raw.verification_bytes();
            (
                metrics.hot_blob_cache_bytes,
                metrics.hot_blob_cache_entries,
                metrics.hot_blob_spill_bytes,
                metrics.verified_decode_buffer_bytes,
            ) = raw.hot_metrics();
        }
        // Parsed metadata is invocation-local and retained for touched OIDs;
        // it is not part of the byte-budgeted payload LRU.
        metrics.tree_cache_limit = 0;
        metrics.header_cache_entries = state.headers.len();
        if let Some(raw) = &state.raw_blobs {
            let (bytes, entries) = raw.disk_metrics();
            metrics.retained_raw_blob_bytes += bytes;
            metrics.retained_raw_blob_entries += entries;
        }
        for raw in state
            .raw_blobs
            .iter()
            .chain(state.remote.iter().map(|remote| &remote.raw_blobs))
        {
            let (calls, written) = raw.write_metrics();
            metrics.spool_writes = metrics.spool_writes.saturating_add(calls);
            metrics.spool_bytes = metrics.spool_bytes.saturating_add(written);
        }
        // Drop the registry lock before dropping any upgraded reader: its last
        // owner removes itself from that registry, even during metrics sampling.
        let readers: Vec<_> = state
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for reader in readers {
            metrics.active_blob_readers += 1;
            if reader.data.on_disk() {
                metrics.active_spool_bytes =
                    metrics.active_spool_bytes.saturating_add(reader.data.len());
            }
        }
        metrics
    }
    #[cfg(feature = "test-metrics")]
    pub fn available_objects(&self) -> Result<Vec<Oid>> {
        let state = self.state()?;
        let mut objects = Vec::new();
        if let Some(native) = state.native_local() {
            objects = native.objects()?;
        } else if let Some(repo) = state.repository() {
            repo.odb()?.foreach(|oid| {
                objects.push(*oid);
                true
            })?;
        }
        if let Some(raw) = &state.raw_blobs {
            objects.extend(raw.oids()?);
        }
        if let Some(remote) = &state.remote {
            objects.extend(remote.raw_blobs.oids()?);
        }
        objects.sort_unstable();
        objects.dedup();
        Ok(objects)
    }
    #[cfg(feature = "test-metrics")]
    pub(crate) fn record_file_sync(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metrics
            .file_sync_callbacks += 1;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use tempfile::TempDir;
    fn fixture() -> (TempDir, Oid) {
        let tmp = TempDir::new().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let blob = repo.blob(b"original\n").unwrap();
        let target = repo.blob(b"script").unwrap();
        let mut nested = repo.treebuilder(None).unwrap();
        nested.insert("child", blob, 0o100644).unwrap();
        let nested_oid = nested.write().unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("script", blob, 0o100755).unwrap();
        builder.insert("naïve space", blob, 0o100644).unwrap();
        builder.insert("link", target, 0o120000).unwrap();
        builder.insert("dir", nested_oid, 0o040000).unwrap();
        let oid = builder.write().unwrap();
        let sig = Signature::now("Test", "test@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "initial",
            &repo.find_tree(oid).unwrap(),
            &[],
        )
        .unwrap();
        (tmp, oid)
    }
    #[test]
    fn packed_blob_does_not_repeat_a_native_verified_oid_pass() {
        if std::env::var_os("GIT_ON_NATIVE_VERIFIED_CHILD").is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "snapshot::tests::packed_blob_does_not_repeat_a_native_verified_oid_pass",
                    "--nocapture",
                ])
                .env("GIT_ON_NATIVE_VERIFIED_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        // Only this isolated test process uses libgit2. Set process-wide
        // policy before opening any repository or starting Git work.
        unsafe { crate::object::configure_pack_descriptors() }.unwrap();
        let (tmp, _) = fixture();
        let (oid, tree) = {
            let repo = Repository::open(tmp.path()).unwrap();
            let oid = repo.blob(&vec![b'z'; 256 * 1024]).unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("large", oid, 0o100644).unwrap();
            let tree = builder.write().unwrap();
            repo.reference("refs/tests/native-verified", tree, true, "fixture")
                .unwrap();
            (oid, tree)
        };
        assert!(std::process::Command::new("git")
            .args(["repack", "-a", "-d", "--window=0"])
            .current_dir(tmp.path())
            .status()
            .unwrap()
            .success());
        let snapshot =
            Snapshot::load(tmp.path(), OsStr::new(&tree.to_string()), "native-verified").unwrap();
        let probe = crate::object::HashProbe::start(oid);
        assert_eq!(snapshot.read(oid, 0, 129).unwrap(), vec![b'z'; 129]);
        assert_eq!(snapshot.read(oid, 128_000, 129).unwrap(), vec![b'z'; 129]);
        assert_eq!(snapshot.metrics().blob_loads, 1);
        assert_eq!(
            probe.bytes(),
            0,
            "already native-verified packed bytes were hashed against their OID again"
        );
        drop(probe);
        let loose = vec![b'l'; 128 * 1024];
        let loose_oid = Repository::open(tmp.path()).unwrap().blob(&loose).unwrap();
        let probe = crate::object::HashProbe::start(loose_oid);
        assert_eq!(snapshot.read(loose_oid, 0, 129).unwrap(), vec![b'l'; 129]);
        assert_eq!(
            probe.bytes(),
            loose.len(),
            "a stream skipped its OID verification"
        );
        // Native verification only authenticates the owned import buffer.
        // A later disk read must still reject changed bytes against its
        // trusted block digests, even when the original OID was checked in C.
        let mut state = snapshot.state().unwrap();
        let store = state.raw_blobs.as_mut().unwrap();
        store.evict_all_for_test().unwrap();
        let raw = store.get(oid).unwrap().unwrap();
        drop(state);
        raw.corrupt_for_test().unwrap();
        assert!(snapshot.read(oid, 0, 1).is_err());
    }
    #[test]
    fn corrupt_packed_oid_is_rejected_with_native_or_embedding_verification() {
        let mode = std::env::var("GIT_ON_NATIVE_CORRUPT_CHILD").ok();
        if mode.is_none() {
            for mode in ["native", "embedding"] {
                let result = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "snapshot::tests::corrupt_packed_oid_is_rejected_with_native_or_embedding_verification",
                        "--nocapture",
                    ])
                    .env("GIT_ON_NATIVE_CORRUPT_CHILD", mode)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{mode}: {}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            return;
        }
        let native = mode.as_deref() == Some("native");
        if native {
            unsafe { crate::object::configure_pack_descriptors() }.unwrap();
        } else {
            // Fault injection in this isolated process only: an embedding
            // without CLI policy must retain the full Rust OID check even
            // when its dependency does not verify a returned ODB buffer.
            git2::opts::strict_hash_verification(false);
        }
        use sha1_checked::{Digest, Sha1};
        use std::io::Write;
        let (tmp, _) = fixture();
        let wanted = vec![b'a'; 256 * 1024];
        let actual = vec![b'b'; wanted.len()];
        let repo = Repository::open(tmp.path()).unwrap();
        let oid = repo.blob(&wanted).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("file", oid, 0o100644).unwrap();
        let tree = builder.write().unwrap();
        let objects = repo.path().join("objects");
        std::fs::remove_file(
            objects
                .join(&oid.to_string()[..2])
                .join(&oid.to_string()[2..]),
        )
        .unwrap();
        // A valid one-object pack/CRC/checksum containing B, with an index
        // claiming its ID is A. Inflation and header checks alone accept it.
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
        let mut size = actual.len();
        let mut byte = 0x30 | (size as u8 & 15);
        size >>= 4;
        while size != 0 {
            pack.push(byte | 128);
            byte = size as u8 & 127;
            size >>= 7;
        }
        pack.push(byte);
        let mut compressed =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        compressed.write_all(&actual).unwrap();
        pack.extend(compressed.finish().unwrap());
        let checksum = crate::object::finish_sha1(Sha1::new().chain_update(&pack)).unwrap();
        pack.extend(checksum);
        let mut child = std::process::Command::new("git")
            .args(["-c", "pack.indexVersion=2", "index-pack", "--stdin"])
            .current_dir(tmp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&pack).unwrap();
        assert!(child.wait().unwrap().success());
        let index = objects
            .join("pack")
            .join(format!("pack-{}.idx", Oid::from_bytes(&checksum).unwrap()));
        let mut bytes = std::fs::read(&index).unwrap();
        assert_eq!(bytes.len(), 1100);
        assert_eq!(&bytes[..8], b"\xfftOc\0\0\0\x02");
        for bucket in 0..256 {
            bytes[8 + bucket * 4..12 + bucket * 4]
                .copy_from_slice(&u32::from(bucket >= oid.as_bytes()[0] as usize).to_be_bytes());
        }
        bytes[1032..1052].copy_from_slice(oid.as_bytes());
        let checksum =
            crate::object::finish_sha1(Sha1::new().chain_update(&bytes[..1080])).unwrap();
        bytes[1080..].copy_from_slice(&checksum);
        let replacement = index.with_extension("replacement");
        std::fs::write(&replacement, bytes).unwrap();
        std::fs::rename(replacement, index).unwrap();
        drop(builder);
        drop(repo);
        let snapshot =
            Snapshot::load(tmp.path(), OsStr::new(&tree.to_string()), "native-corrupt").unwrap();
        let probe = crate::object::HashProbe::start(oid);
        let error = snapshot.read(oid, 0, 1).unwrap_err();
        assert!(format!("{error:#}").contains("hash mismatch"), "{error:#}");
        assert_eq!(probe.bytes(), if native { 0 } else { actual.len() });
        assert_eq!(snapshot.metrics().blob_loads, 0);
        assert_eq!(snapshot.metrics().retained_raw_blob_bytes, 0);
    }
    #[test]
    fn reads_objects_modes_and_links_from_an_unchanged_repository() {
        let (tmp, oid) = fixture();
        std::fs::write(tmp.path().join("script"), b"dirty checkout").unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "test-pin").unwrap();
        assert_eq!(snap.tree_oid, oid);
        for path in ["naïve space", "script"] {
            let entry = snap.entry(Path::new(path)).unwrap().unwrap();
            assert_eq!(entry.size, Some(9));
            assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"original\n");
        }
        assert_eq!(
            snap.entry(Path::new("script")).unwrap().unwrap().mode,
            0o100755
        );
        let link = snap.entry(Path::new("link")).unwrap().unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(snap.read_link(link.oid).unwrap(), b"script");
        let repo = Repository::open(tmp.path()).unwrap();
        assert!(repo.find_reference("refs/git-on/test-pin").is_err());
        let shared = snap.clone();
        drop(snap);
        assert_eq!(shared.read_link(link.oid).unwrap(), b"script");
        assert_eq!(
            std::fs::read(tmp.path().join("script")).unwrap(),
            b"dirty checkout"
        );
    }
    #[test]
    fn accepts_raw_tree_and_subtree() {
        let (tmp, oid) = fixture();
        let snap = Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "raw").unwrap();
        assert_eq!(snap.tree_oid, oid);
        let sub = Snapshot::load(tmp.path(), OsStr::new("HEAD:dir"), "sub").unwrap();
        assert!(sub.entry(Path::new("child")).unwrap().is_some());
        assert!(sub.entry(Path::new("dir")).unwrap().is_none());
        assert!(sub.entry(Path::new("")).unwrap().is_some());
    }
    #[test]
    fn rejects_lfs_submodules_on_demand_not_at_startup() {
        for submodule in [false, true] {
            let (tmp, _) = fixture();
            let repo = Repository::open(tmp.path()).unwrap();
            let oid = if submodule {
                repo.head().unwrap().target().unwrap()
            } else {
                repo.blob(
                    format!(
                        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                        "a".repeat(64)
                    )
                    .as_bytes(),
                )
                .unwrap()
            };
            let ordinary = repo.blob(b"works").unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder
                .insert(
                    "unsupported",
                    oid,
                    if submodule { 0o160000 } else { 0o100644 },
                )
                .unwrap();
            builder.insert("ordinary", ordinary, 0o100644).unwrap();
            let tree = builder.write().unwrap();
            let snap = Snapshot::load(tmp.path(), OsStr::new(&tree.to_string()), "demand").unwrap();
            assert_eq!(snap.metrics().blob_loads, 0);
            assert!(snap.entry(Path::new("ordinary")).unwrap().is_some());
            let directory = snap.directory(Path::new("")).unwrap();
            let index = directory.index(OsStr::new("unsupported")).unwrap();
            let child = snap.directory_entry(&directory, index, Path::new("unsupported"));
            if submodule {
                assert!(snap.entry(Path::new("unsupported")).is_err());
                assert!(child.is_err());
            } else {
                let entry = snap.entry(Path::new("unsupported")).unwrap().unwrap();
                assert_eq!(child.unwrap().oid, entry.oid);
                assert!(snap.read(entry.oid, 20, 1).is_err());
            }
        }
    }
    #[test]
    fn lfs_detection_requires_complete_valid_pointer_structure() {
        let hash = "0123456789abcdef".repeat(4);
        let version = "version https://git-lfs.github.com/spec/v1\n";
        let pointer = format!("{version}oid sha256:{hash}\nsize 123\n");
        for valid in [
            pointer.clone(),
            pointer.replace('\n', "\r\n"),
            pointer.trim_end_matches('\n').to_owned(),
            pointer.replace("git-lfs", "hawser"),
            pointer.replace("https://git-lfs.github.com/spec/v1", "http://git-media.io/v/2"),
            pointer.replace("size 123", "size 0"),
            format!("{version}ext-0-compress sha256:{hash}\next-1-encrypt sha256:{hash}\noid sha256:{hash}\nsize 123\n"),
            format!("{pointer}x-note preserved metadata\n"),
        ] {
            assert!(is_lfs_pointer(valid.as_bytes()), "{valid:?}");
            assert!(State::validate_blob(valid.as_bytes()).is_err());
        }
        for ordinary in [
            String::new(),
            version.to_owned(),
            format!("{version}This is documentation, not a pointer.\n"),
            format!("{version}oid sha256:{hash}\n"),
            format!("{version}size 123\n"),
            pointer.replace(&hash, "abc"),
            pointer.replace(&hash, &"a".repeat(65)),
            pointer.replace(&hash, &hash.to_uppercase()),
            pointer.replace("sha256:", "sha1:"),
            pointer.replace("size 123", "size -1"),
            pointer.replace("size 123", "size 1.5"),
            pointer.replace("size 123", "size 9223372036854775808"),
            pointer.replace("size 123", "size 123 bytes"),
            pointer.replace("size 123", "size  123"),
            pointer.replace("size 123", "size\t123"),
            pointer.replace("/spec/v1", "/spec/v2"),
            format!("{version}size 123\noid sha256:{hash}\n"),
            format!("{version}oid sha256:{hash}\noid sha256:{hash}\nsize 123\n"),
            format!("{pointer}size 123\n"),
            format!("{pointer}This is documentation, not a pointer.\n"),
            format!("{version}ext-0-compress sha256:abc\noid sha256:{hash}\nsize 123\n"),
            format!("{version}ext-0-compress sha256:{hash}\next-0-encrypt sha256:{hash}\noid sha256:{hash}\nsize 123\n"),
            format!("{pointer}x-note contains\rcarriage return\n"),
            format!("{pointer}x-note nul\0byte\n"),
            format!("{pointer}x-note {}\n", "x".repeat(1024)),
        ] {
            assert!(!is_lfs_pointer(ordinary.as_bytes()), "{ordinary:?}");
            assert!(State::validate_blob(ordinary.as_bytes()).is_ok());
        }
        for length in [1023, 1024] {
            let prefix = format!("{pointer}x-note ");
            let boundary = format!("{prefix}{}\n", "x".repeat(length - prefix.len() - 1));
            assert_eq!(boundary.len(), length);
            assert_eq!(is_lfs_pointer(boundary.as_bytes()), length < 1024);
        }
        let mut binary = pointer.into_bytes();
        binary.extend_from_slice(b"x-note invalid UTF-8: \xff\n");
        assert!(!is_lfs_pointer(&binary));
        assert!(State::validate_blob(&binary).is_ok());
    }
    #[test]
    fn reopened_small_blob_does_not_reload_spill_for_repeated_policy_check() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let bytes = vec![b'v'; 512];
        let oid = repo.blob(&bytes).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "policy-reopen").unwrap();
        assert_eq!(snap.read(oid, 0, 1).unwrap(), b"v");
        {
            let mut state = snap.state().unwrap();
            state
                .raw_blobs
                .as_mut()
                .unwrap()
                .evict_all_for_test()
                .unwrap();
        }
        let authenticated = || {
            snap.state()
                .unwrap()
                .raw_blobs
                .as_ref()
                .unwrap()
                .tested_authenticated_bytes()
        };
        assert_eq!(authenticated(), 0);
        // A newly opened reader does not request data yet. The immutable
        // object's successful policy check must not read a spilled payload.
        let reader = snap.open(oid).unwrap();
        assert_eq!(
            authenticated(),
            0,
            "reader reopen reauthenticated the whole small spill for LFS policy"
        );
        assert_eq!(reader.read(511, 1).unwrap(), b"v");
        assert_eq!(
            authenticated(),
            512,
            "the actual data read must still authenticate its block"
        );
        drop(reader);
        assert_eq!(snap.metrics().blob_loads, 1);
    }
    #[test]
    fn lfs_prefix_in_ordinary_blobs_is_readable_through_both_read_paths() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let text =
            b"version https://git-lfs.github.com/spec/v1\nThis is documentation, not a pointer.\n";
        let ordinary = repo.blob(text).unwrap();
        let pointer = repo
            .blob(
                format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 123\n",
                    "a".repeat(64)
                )
                .as_bytes(),
            )
            .unwrap();
        for first_open in [false, true] {
            let snap = Snapshot::load(
                tmp.path(),
                OsStr::new("HEAD"),
                if first_open { "lfs-open" } else { "lfs-read" },
            )
            .unwrap();
            if first_open {
                assert_eq!(snap.open(ordinary).unwrap().read(0, 4096).unwrap(), text);
            } else {
                assert_eq!(snap.read(ordinary, 0, 4096).unwrap(), text);
            }
            // Both the uncached and cached paths preserve the original bytes.
            assert_eq!(snap.read(ordinary, 0, 4096).unwrap(), text);
            assert_eq!(snap.open(ordinary).unwrap().read(0, 4096).unwrap(), text);
            assert!(snap.read(pointer, 0, 4096).is_err());
            assert!(snap.open(pointer).is_err());
            assert_eq!(snap.read(ordinary, 0, 4096).unwrap(), text);
        }
    }
    #[test]
    fn invocation_from_worktree_subdirectory() {
        let (tmp, oid) = fixture();
        let parent = TempDir::new().unwrap();
        let repository = parent.path().join("repository\nname");
        std::fs::rename(tmp.path(), &repository).unwrap();
        let directory = repository.join("some/deep/path");
        std::fs::create_dir_all(&directory).unwrap();
        let snap = Snapshot::load(&directory, OsStr::new("HEAD"), "discovery").unwrap();
        assert_eq!(snap.tree_oid, oid);
    }
    #[test]
    fn native_remote_selection_needs_no_private_pin() {
        let (tmp, oid) = fixture();
        let run = TempDir::new().unwrap();
        let repo = Repository::open(tmp.path()).unwrap();
        repo.config()
            .unwrap()
            .set_bool("uploadpack.allowFilter", true)
            .unwrap();
        repo.config()
            .unwrap()
            .set_bool("uploadpack.allowAnySHA1InWant", true)
            .unwrap();
        let private = run.path().canonicalize().unwrap().join("remote-repository");
        let snapshot = Snapshot::load_source(
            tmp.path(),
            Some(tmp.path().as_os_str()),
            OsStr::new("HEAD"),
            false,
            false,
            run.path(),
        )
        .unwrap();
        assert_eq!(snapshot.tree_oid, oid);
        for repository in [repo, Repository::open_bare(private).unwrap()] {
            assert!(repository
                .references_glob("refs/git-on/*")
                .unwrap()
                .next()
                .is_none());
        }
    }
    #[test]
    fn startup_listing_lookup_and_read_are_separately_lazy() {
        let (tmp, _) = fixture();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "lazy").unwrap();
        assert_eq!(snap.metrics().tree_reads, 0);
        assert_eq!(snap.metrics().header_reads, 0);
        assert_eq!(snap.metrics().blob_loads, 0);
        let root = Path::new("");
        let directory = snap.directory(root).unwrap();
        let listing = Snapshot::directory_children(&directory, root)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(listing.len(), 4);
        assert!(listing
            .iter()
            .zip(directory.children())
            .all(|((listed, _), (stored, _, _))| std::ptr::eq(*listed, stored)));
        assert!(listing
            .iter()
            .filter(|(_, e)| e.kind != EntryKind::Directory)
            .all(|(_, e)| e.size.is_none()));
        assert_eq!(snap.metrics().tree_reads, 1);
        assert_eq!(snap.metrics().header_reads, 0);
        assert_eq!(snap.metrics().blob_loads, 0);
        let entry = snap.entry(Path::new("dir/child")).unwrap().unwrap();
        assert_eq!(snap.metrics().tree_reads, 2);
        assert_eq!(snap.metrics().header_reads, 1);
        assert_eq!(snap.metrics().blob_loads, 0);
        assert_eq!(snap.read(entry.oid, 2, 3).unwrap(), b"igi");
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().blob_bytes, 9);
        assert_eq!(snap.read(entry.oid, 8, 100).unwrap(), b"\n");
        assert!(snap.read(entry.oid, u64::MAX, 100).unwrap().is_empty());
        assert_eq!(snap.metrics().blob_loads, 1);
        snap.entry(Path::new("dir/child")).unwrap();
        assert_eq!(snap.metrics().header_reads, 1);
        assert_eq!(snap.metrics().tree_reads, 2);
    }
    #[test]
    fn unvisited_missing_objects_do_not_block_startup_or_other_reads() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let data = repo.blob(b"this object will disappear").unwrap();
        let mut child = repo.treebuilder(None).unwrap();
        child.insert("missing", data, 0o100644).unwrap();
        let missing_tree = child.write().unwrap();
        let good = repo.blob(b"good").unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("unvisited", missing_tree, 0o040000).unwrap();
        root.insert("good", good, 0o100644).unwrap();
        let oid = root.write().unwrap();
        for remove in [data, missing_tree] {
            let hex = remove.to_string();
            std::fs::remove_file(repo.path().join("objects").join(&hex[..2]).join(&hex[2..]))
                .unwrap();
        }
        let snap = Snapshot::load(tmp.path(), OsStr::new(&oid.to_string()), "missing").unwrap();
        assert_eq!(snap.children(Path::new("")).unwrap().len(), 2);
        let entry = snap.entry(Path::new("good")).unwrap().unwrap();
        assert_eq!(snap.read(entry.oid, 0, 100).unwrap(), b"good");
        assert!(snap.children(Path::new("unvisited")).is_err());
    }
    #[test]
    fn directory_metadata_is_prepared_once_for_sibling_and_absent_lookups() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        for index in 0..40 {
            tree.insert(format!("file-{index:03}"), blob, 0o100644)
                .unwrap();
        }
        let tree = tree.write().unwrap();
        let snapshot = Snapshot::load(
            tmp.path(),
            OsStr::new(&tree.to_string()),
            "sibling-metadata",
        )
        .unwrap();
        for index in 0..20 {
            assert_eq!(
                snapshot
                    .entry(Path::new(&format!("file-{index:03}")))
                    .unwrap()
                    .unwrap()
                    .size,
                Some(1)
            );
        }
        assert!(snapshot.entry(Path::new("absent")).unwrap().is_none());
        assert_eq!(
            snapshot.metrics().tree_reads,
            1,
            "sibling or absent lookup decoded/scanned the directory again"
        );
        assert_eq!(snapshot.metrics().blob_loads, 0);
    }
    #[test]
    fn fitting_small_working_set_uses_byte_budget_without_early_disk_spill() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "resident-working-set").unwrap();
        for index in 0..160u32 {
            let mut bytes = vec![0; 8192];
            bytes[..4].copy_from_slice(&index.to_le_bytes());
            let oid = repo.blob(&bytes).unwrap();
            assert_eq!(snap.read(oid, 0, 8192).unwrap(), bytes);
        }
        assert_eq!(
            snap.metrics().blob_cache_entries,
            160,
            "an unrelated entry cap evicted fitting blobs"
        );
        assert_eq!(
            snap.metrics().retained_raw_blob_bytes,
            0,
            "resident blobs were written eagerly"
        );
        assert!(snap.metrics().blob_cache_bytes <= snap.metrics().blob_cache_limit);
    }
    #[test]
    fn tiny_reopened_reads_of_spilled_blobs_do_not_refill_entire_objects() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "range-refill").unwrap();
        let oid = repo.blob(&vec![b'z'; 1024 * 1024]).unwrap();
        assert_eq!(snap.read(oid, 0, 1).unwrap(), b"z");
        for index in 0..20u8 {
            let other = repo.blob(&vec![index; 1024 * 1024]).unwrap();
            assert_eq!(snap.read(other, 0, 1).unwrap(), [index]);
        }
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        let before = snap
            .state()
            .unwrap()
            .raw_blobs
            .as_ref()
            .unwrap()
            .tested_authenticated_bytes();
        for offset in [100, 8192, 500_000] {
            assert_eq!(snap.read(oid, offset, 1).unwrap(), b"z");
        }
        let after = snap
            .state()
            .unwrap()
            .raw_blobs
            .as_ref()
            .unwrap()
            .tested_authenticated_bytes();
        assert_eq!(
            after - before,
            2 * crate::raw_blob::BLOCK as u64,
            "tiny reads authenticated whole objects again"
        );
        assert_eq!(
            snap.metrics().blob_loads,
            21,
            "a prepared source was decoded again"
        );
    }
    #[test]
    fn memory_hits_do_not_probe_the_disk_locator_and_evicted_readers_still_verify() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "cached-locator").unwrap();
        let bytes = b"retained memory bytes";
        let oid = repo.blob(bytes).unwrap();
        assert_eq!(snap.read(oid, 0, 1024).unwrap(), bytes);
        assert_eq!(snap.read(oid, 0, 1024).unwrap(), bytes);
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().retained_raw_blob_bytes, 0);
        let reader = snap.open(oid).unwrap();
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        assert_eq!(reader.read(0, 1024).unwrap(), bytes);
        // The retained locator is trusted memory; underlying bytes still pass
        // through block authentication after memory-cache eviction.
        let raw = snap
            .state()
            .unwrap()
            .raw_blobs
            .as_ref()
            .unwrap()
            .get(oid)
            .unwrap()
            .unwrap();
        raw.corrupt_for_test().unwrap();
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        assert!(reader.read(0, 1024).is_err());
        assert!(snap.read(oid, 0, 1024).is_err());
    }
    #[test]
    fn reader_registry_releases_closed_entries_without_another_open() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "reader-slots").unwrap();
        let oid = repo.blob(b"reader").unwrap();
        let reader = snap.open(oid).unwrap();
        assert_eq!(snap.metrics().active_blob_readers, 1);
        drop(reader);
        assert_eq!(snap.metrics().active_blob_readers, 0);
        assert_eq!(snap.state().unwrap().readers.lock().unwrap().len(), 0);
    }
    #[test]
    fn transient_read_does_not_wait_for_the_active_reader_registry() {
        let (tmp, _) = fixture();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "transient-reader").unwrap();
        let oid = snap.entry(Path::new("script")).unwrap().unwrap().oid;
        let registry = snap.state().unwrap().readers.clone();
        let held = registry.lock().unwrap();
        std::thread::scope(|scope| {
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let snapshot = &snap;
            let worker = scope.spawn(move || send.send(snapshot.read(oid, 2, 3)).unwrap());
            let result = receive.recv_timeout(std::time::Duration::from_secs(5));
            // Always release/join before asserting, including the old blocked path.
            drop(held);
            worker.join().unwrap();
            assert_eq!(
                result
                    .expect("transient read waited for unrelated reader bookkeeping")
                    .unwrap(),
                b"igi"
            );
        });
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().active_blob_readers, 0);
        assert!(registry.lock().unwrap().is_empty());
    }
    #[test]
    fn open_small_readers_share_the_payload_budget_and_reuse_evicted_contents() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "active-memory").unwrap();
        let count = BLOB_CACHE_BYTES / crate::raw_blob::BLOCK + 2;
        let mut readers = Vec::new();
        for index in 0..count {
            let mut bytes = vec![b'x'; crate::raw_blob::BLOCK];
            bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
            let oid = repo.blob(&bytes).unwrap();
            readers.push(snap.open(oid).unwrap());
        }
        let metrics = snap.metrics();
        assert_eq!(metrics.active_blob_readers, count);
        assert!(metrics.blob_cache_entries > 0 && metrics.blob_cache_entries < count);
        assert!(
            metrics.retained_raw_blob_bytes > 0,
            "no resident object was evicted"
        );
        assert!(metrics.blob_cache_bytes <= BLOB_CACHE_BYTES);
        for (index, reader) in readers.iter().enumerate() {
            assert_eq!(reader.read(0, 8).unwrap(), (index as u64).to_le_bytes());
        }
        assert_eq!(snap.metrics().blob_loads, count as u64);
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
    }
    #[test]
    fn blob_cache_is_bounded_and_large_partial_reads_bypass_retention() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "bounded").unwrap();
        let mut ids = Vec::new();
        for n in 0..300u32 {
            let mut data = vec![n as u8; crate::raw_blob::BLOCK];
            data[..4].copy_from_slice(&n.to_le_bytes());
            let oid = repo.blob(&data).unwrap();
            ids.push(oid);
            assert_eq!(snap.read(oid, 8192, 1).unwrap(), vec![n as u8]);
            assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        }
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        let loads = snap.metrics().blob_loads;
        snap.read(*ids.last().unwrap(), 0, 1).unwrap();
        assert_eq!(snap.metrics().blob_loads, loads);
        snap.read(ids[0], 0, 1).unwrap();
        assert_eq!(
            snap.metrics().blob_loads,
            loads,
            "eviction must not unpack prepared data again"
        );
        let large = repo.blob(&vec![b'x'; BLOB_CACHE_BYTES + 1]).unwrap();
        let retained = snap.metrics().blob_cache_bytes;
        assert_eq!(snap.read(large, 1000, 5).unwrap(), b"xxxxx");
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        assert!(snap.metrics().blob_cache_bytes <= retained + crate::raw_blob::BLOCK + 512);
        assert!(!snap.open(large).unwrap().0.data.on_disk());
        assert_eq!(snap.metrics().hot_blob_cache_entries, 1);
    }
    #[test]
    fn touched_directories_stay_prepared_without_an_entry_cap() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let storage = TempDir::new().unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "tree-reuse").unwrap();
        snap.set_spool_directory(&storage.path().join("upper"))
            .unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut ids = Vec::new();
        for i in 0..300 {
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert(format!("file-{i}"), blob, 0o100644).unwrap();
            let id = tree.write().unwrap();
            snap.state().unwrap().tree(id).unwrap();
            ids.push(id);
        }
        for id in ids {
            snap.state().unwrap().tree(id).unwrap();
        }
        let stats = snap.metrics();
        assert_eq!(stats.tree_cache_entries, 300);
        assert_eq!(
            stats.tree_reads, 300,
            "already prepared trees were decoded again"
        );
        assert_eq!(stats.prepared_tree_entries, 300);
        let mut directories = vec![storage.path().to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                assert!(
                    entry.file_type().unwrap().is_dir(),
                    "raw trees were retained on disk: {}",
                    entry.path().display()
                );
                directories.push(entry.path());
            }
        }
    }
    #[test]
    fn raw_write_metrics_include_unpublished_partial_writes() {
        const CHILD: &str = "GIT_ON_TEST_PARTIAL_RAW_METRICS";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "snapshot::tests::raw_write_metrics_include_unpublished_partial_writes",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let oid = repo.blob(&vec![b'x'; crate::raw_blob::BLOCK + 1]).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "partial-write-metrics").unwrap();
        let storage = tempfile::tempdir().unwrap();
        snap.set_spool_directory(&storage.path().join("backing"))
            .unwrap();
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let limit = libc::rlimit {
                rlim_cur: 3,
                rlim_max: 3,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
        assert_eq!(snap.open(oid).unwrap().read(0, 1).unwrap(), b"x");
        assert!(snap
            .state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .is_err());
        let metrics = snap.metrics();
        assert_eq!(metrics.retained_raw_blob_bytes, 0);
        assert_eq!(metrics.spool_bytes, 3);
        assert!(metrics.spool_writes >= 2);
    }
    #[test]
    fn active_large_blob_reader_decodes_once_and_releases_active_accounting() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "spool").unwrap();
        let size = BLOB_CACHE_BYTES + 123;
        let oid = repo.blob(&vec![b'x'; size]).unwrap();
        let reader = snap.open(oid).unwrap();
        let second = snap.open(oid).unwrap();
        assert_eq!(snap.metrics().streamed_blob_loads, 1);
        for offset in (0..size as u64).step_by(4096) {
            assert_eq!(
                reader.read(offset, 4096).unwrap(),
                vec![b'x'; (size as u64 - offset).min(4096) as usize]
            );
        }
        assert_eq!(second.read(size as u64 - 2, 100).unwrap(), b"xx");
        assert_eq!(snap.metrics().blob_loads, 1);
        assert_eq!(snap.metrics().spool_bytes, 0);
        assert_eq!(snap.metrics().active_spool_bytes, 0);
        snap.state()
            .unwrap()
            .raw_blobs
            .as_mut()
            .unwrap()
            .evict_all_for_test()
            .unwrap();
        assert_eq!(reader.read(size as u64 - 2, 2).unwrap(), b"xx");
        assert_eq!(snap.metrics().spool_bytes, size as u64);
        assert_eq!(snap.metrics().active_spool_bytes, size as u64);
        assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
        drop(reader);
        assert_eq!(snap.metrics().active_spool_bytes, size as u64);
        drop(second);
        assert_eq!(snap.metrics().active_spool_bytes, 0);
        assert_eq!(snap.metrics().active_blob_readers, 0);
    }
    #[test]
    fn large_blob_preparation_survives_close_and_reopen_with_bounded_memory() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let size = BLOB_CACHE_BYTES + 123;
        let oid = repo.blob(&vec![b'x'; size]).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "reopen").unwrap();
        let mut writes = None;
        for offset in [0, size as u64 - 1, 123] {
            let reader = snap.open(oid).unwrap();
            assert_eq!(reader.read(offset, 1).unwrap(), b"x");
            drop(reader);
            assert_eq!(snap.metrics().active_blob_readers, 0);
            assert!(snap.metrics().blob_cache_bytes <= BLOB_CACHE_BYTES);
            if offset == 0 {
                assert_eq!(snap.metrics().spool_bytes, 0);
                snap.state()
                    .unwrap()
                    .raw_blobs
                    .as_mut()
                    .unwrap()
                    .evict_all_for_test()
                    .unwrap();
            }
            let metrics = snap.metrics();
            assert!(metrics.spool_writes > 0);
            assert_eq!(metrics.spool_bytes, size as u64);
            let first = writes.get_or_insert(metrics.spool_writes);
            assert_eq!(metrics.spool_writes, *first);
            assert_eq!(snap.metrics().blob_loads, 1);
        }
    }
    #[test]
    fn malformed_symlink_targets_are_rejected_on_readlink() {
        let (tmp, _) = fixture();
        let repo = Repository::open(tmp.path()).unwrap();
        let snap = Snapshot::load(tmp.path(), OsStr::new("HEAD"), "links").unwrap();
        for bytes in [b"".as_slice(), b"bad\0target"] {
            assert!(snap.read_link(repo.blob(bytes).unwrap()).is_err());
        }
    }
}
