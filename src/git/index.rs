use super::{run, Git};
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

pub(super) fn resolve(git: &Git, revision: &OsStr, run_directory: &Path) -> Result<Oid> {
    let git_dir = git.repository_dir()?;
    let common = path(git, &["--path-format=absolute", "--git-common-dir"])?;
    let index = git.cwd.join(path(
        git,
        &["--path-format=absolute", "--git-path", "index"],
    )?);
    let (_, output) = super::native_scratch(run_directory, "git-on-index-", 0, |scratch| {
        // Git reads a missing split-index base from beside the primary index,
        // but refreshes only the path in its Git directory. The usual index
        // therefore needs no copy. Preserve git-dir-first lookup for a separate
        // GIT_INDEX_FILE by staging just the base it actually refers to.
        let index_location = index
            .parent()
            .context("native Git returned an index without a parent")?;
        let separate_index = index_location != git_dir;
        let private_index = scratch.join("index");
        let effective_index = if separate_index {
            symlink(&index, &private_index)?;
            &private_index
        } else {
            &index
        };
        let head = git_dir.join("HEAD");
        let private_head = scratch.join("HEAD");
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
        copy_if_present(git_dir, scratch, OsStr::new("config.worktree"))?;

        let worktree = git.worktree_root()?;
        let resolve = || {
            let mut command = git.command(None);
            super::read_only(&mut command)
                .args(["-c", "core.fsmonitor=false"])
                .arg("--git-dir")
                .arg(scratch)
                .env("GIT_COMMON_DIR", &common)
                .env("GIT_INDEX_FILE", effective_index)
                .env("TMPDIR", scratch)
                .env("LC_ALL", "C");
            if let Some(worktree) = &worktree {
                command.arg("--work-tree").arg(worktree);
            }
            command
                .args(["rev-parse", "--verify", "--end-of-options"])
                .arg(revision)
                .stdin(Stdio::null());
            run(command, true)
        };
        let mut output = resolve()?;
        if separate_index && !output.status.success() {
            if let Some(base) = missing_base(&output.stderr, scratch) {
                let destination = scratch.join(&base);
                fs::copy(git_dir.join(&base), &destination).or_else(|error| {
                    match error.kind() {
                        std::io::ErrorKind::NotFound => {
                            fs::copy(index_location.join(base), &destination)
                        }
                        _ => Err(error),
                    }
                })?;
                output = resolve()?;
            }
        }
        Ok(output)
    })?;
    anyhow::ensure!(
        output.status.success(),
        "Git rev-parse failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Oid::from_str(std::str::from_utf8(&output.stdout)?.trim())
        .context("resolve exact SHA-1 revision (SHA-256 repositories unsupported)")
}

fn missing_base(stderr: &[u8], scratch: &Path) -> Option<String> {
    let prefix = scratch.join("sharedindex.");
    let prefix = prefix.as_os_str().as_bytes();
    let remaining = stderr.get(
        stderr
            .windows(prefix.len())
            .position(|part| part == prefix)?
            + prefix.len()..,
    )?;
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
