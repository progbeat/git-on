use super::{native_git_error, require_no_lazy_fetch, run_builtin, Git};
use crate::service_storage::Directory;
use anyhow::{Context, Result};
use git2::Oid;
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::symlink,
    },
    path::{Path, PathBuf},
    process::Stdio,
};

pub(super) fn selects_from_index(revision: &OsStr) -> bool {
    let bytes = revision.as_bytes();
    bytes.starts_with(b":") && !bytes.starts_with(b":/")
}

pub(super) fn resolve(git: &Git, revision: &OsStr) -> Result<(PathBuf, Oid)> {
    let git_dir = path(git, &["--absolute-git-dir"])?;
    let common = path(git, &["--path-format=absolute", "--git-common-dir"])?;
    let index = git.cwd.join(path(
        git,
        &["--path-format=absolute", "--git-path", "index"],
    )?);
    let scratch = Directory::new("index-")?;
    // Git reads a missing split-index base from beside the primary index,
    // but refreshes only the path in its Git directory. The usual index
    // therefore needs no copy. Preserve git-dir-first lookup for a separate
    // GIT_INDEX_FILE by staging just the base it actually refers to.
    let index_location = index
        .parent()
        .context("native Git returned an index without a parent")?;
    let separate_index = index_location != git_dir;
    let private_index = scratch.path().join("index");
    let effective_index = if separate_index {
        symlink(&index, &private_index)?;
        &private_index
    } else {
        &index
    };
    let head = git_dir.join("HEAD");
    let private_head = scratch.path().join("HEAD");
    if let Some(reference) = fs::read_link(&head)
        .ok()
        .filter(|target| target.as_os_str().as_bytes().starts_with(b"refs/"))
    {
        let mut contents = b"ref: ".to_vec();
        contents.extend_from_slice(reference.as_os_str().as_bytes());
        contents.push(b'\n');
        fs::write(private_head, contents)?;
    } else {
        fs::copy(head, private_head)?;
    }
    copy_if_present(&git_dir, scratch.path(), OsStr::new("config.worktree"))?;

    let mut worktree = git.command(None);
    worktree
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null());
    let worktree = run_builtin(worktree).map_err(native_git_error)?;
    let worktree = worktree
        .status
        .success()
        .then(|| path_from_output(worktree.stdout));
    let resolve = || {
        let mut command = git.command(None);
        command
            .arg("--no-lazy-fetch")
            .arg("--git-dir")
            .arg(scratch.path())
            .env("GIT_COMMON_DIR", &common)
            .env("GIT_INDEX_FILE", effective_index)
            .env("TMPDIR", scratch.path());
        if let Some(worktree) = &worktree {
            command.arg("--work-tree").arg(worktree);
        }
        command
            .args(["rev-parse", "--verify", "--end-of-options"])
            .arg(revision)
            .stdin(Stdio::null());
        run_builtin(command).map_err(native_git_error)
    };
    let mut output = resolve()?;
    if separate_index && !output.status.success() {
        if let Some(base) = missing_base(&output.stderr, scratch.path()) {
            let destination = scratch.path().join(&base);
            fs::copy(git_dir.join(&base), &destination).or_else(|error| match error.kind() {
                std::io::ErrorKind::NotFound => fs::copy(index_location.join(base), &destination),
                _ => Err(error),
            })?;
            output = resolve()?;
        }
    }
    require_no_lazy_fetch(&output)?;
    anyhow::ensure!(
        output.status.success(),
        "Git rev-parse failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let oid = Oid::from_str(std::str::from_utf8(&output.stdout)?.trim())
        .context("resolve exact SHA-1 revision (SHA-256 repositories unsupported)")?;
    Ok((git_dir, oid))
}

fn missing_base(stderr: &[u8], scratch: &Path) -> Option<String> {
    let prefix = scratch.join("sharedindex.");
    let prefix = prefix.as_os_str().as_bytes();
    let remaining = stderr.get(memchr::memmem::find(stderr, prefix)? + prefix.len()..)?;
    let (oid, boundary) = remaining.split_at_checked(40)?;
    (boundary.starts_with(b":") && oid.iter().all(u8::is_ascii_hexdigit))
        .then(|| format!("sharedindex.{}", std::str::from_utf8(oid).unwrap()))
}

fn path(git: &Git, args: &[&str]) -> Result<PathBuf> {
    let mut operands = vec![OsStr::new("rev-parse")];
    operands.extend(args.iter().map(OsStr::new));
    git.output(None, &operands).map(path_from_output)
}

fn path_from_output(mut bytes: Vec<u8>) -> PathBuf {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    PathBuf::from(OsString::from_vec(bytes))
}

fn copy_if_present(source: &Path, destination: &Path, name: &OsStr) -> std::io::Result<()> {
    let path = source.join(name);
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            fs::copy(path, destination.join(name))?;
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
