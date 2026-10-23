//! Eager, byte-preserving non-FUSE execution view for explicit write-policy=all.
use crate::snapshot::{EntryKind, Snapshot};
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};

pub fn materialize(snapshot: &Snapshot, root: &Path) -> Result<()> {
    snapshot.set_spool_directory(root)?;
    directory(snapshot, Path::new(""), root)
}
fn directory(snapshot: &Snapshot, relative: &Path, root: &Path) -> Result<()> {
    let children = snapshot.children(relative)?;
    let mut expected = Vec::new();
    for (name, _) in children {
        if let Some(signal) = crate::git::cancelled_signal() {
            bail!("materialization cancelled by signal {signal}");
        }
        let bytes = name.as_bytes();
        ensure!(
            !bytes.is_empty()
                && bytes != b"."
                && bytes != b".."
                && !bytes.contains(&b'/')
                && !bytes.contains(&0),
            "unsupported snapshot name {:?}",
            name
        );
        let relative = relative.join(&name);
        let path = root.join(&relative);
        let entry = snapshot
            .entry(&relative)?
            .context("snapshot entry disappeared")?;
        let result = (|| -> Result<()> {
            match entry.kind {
                EntryKind::Directory => {
                    fs::create_dir(&path)?;
                    directory(snapshot, &relative, root)?;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                    ensure!(
                        fs::metadata(&path)?.permissions().mode() & 0o777 == 0o755,
                        "host cannot preserve directory permissions"
                    );
                }
                EntryKind::Symlink => {
                    let target = snapshot.read_link(entry.oid)?;
                    symlink(std::ffi::OsStr::from_bytes(&target), &path)?;
                    ensure!(
                        fs::read_link(&path)?.as_os_str().as_bytes() == target,
                        "host cannot preserve symlink target"
                    );
                }
                EntryKind::File => {
                    let mut output = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&path)?;
                    let reader = snapshot.open(entry.oid)?;
                    let size = entry.size.context("missing blob size")?;
                    let mut offset = 0;
                    while offset < size {
                        if let Some(signal) = crate::git::cancelled_signal() {
                            bail!("materialization cancelled by signal {signal}");
                        }
                        let data = reader.read(offset, (size - offset).min(64 * 1024) as u32)?;
                        ensure!(!data.is_empty(), "truncated Git blob");
                        output.write_all(&data)?;
                        offset += data.len() as u64;
                    }
                    // Also validate empty LFS pointers/objects through the ordinary reader.
                    if size == 0 {
                        reader.read(0, 1)?;
                    }
                    let mode = entry.mode & 0o777;
                    output.set_permissions(fs::Permissions::from_mode(mode))?;
                    ensure!(
                        output.metadata()?.permissions().mode() & 0o777 == mode,
                        "host cannot preserve executable/permission bits"
                    );
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
            {
                let existing = fs::read_dir(path.parent().context("missing materialized parent")?)?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                bail!("snapshot path {} collides with existing host entries {:?}; this filesystem cannot represent the snapshot faithfully", relative.display(), existing);
            }
            return Err(error).with_context(|| format!("cannot faithfully materialize {} (unsupported name, symlink/permission limitation, or object error)", relative.display()));
        }
        expected.push(name);
    }
    let mut actual = fs::read_dir(root.join(relative))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    expected.sort();
    actual.sort();
    ensure!(
        actual == expected,
        "host changed snapshot names in {}: expected {:?}, found {:?}",
        relative.display(),
        expected,
        actual
    );
    Ok(())
}
