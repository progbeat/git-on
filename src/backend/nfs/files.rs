use super::{
    attributes::{decode_set, directory_attribute_error, file_type},
    protocol::{Claim, CreateHow, Open, Operation, StateId, Verifier, MAX_LINK, MAX_NAME},
    server::{Context, Server},
    status::*,
    xdr::{Bitmap, Writer, MAX_DATA},
};
use crate::backend::{FileType, SetAttributes};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    time::{Duration, Instant},
};

const MAX_DIRECTORY_VIEWS: usize = 64;
const MAX_DIRECTORY_ROWS: usize = 128;
pub(super) const READ: u32 = 1;
const WRITE: u32 = 2;

pub(super) struct OpenState {
    pub client: u64,
    pub ino: u64,
    pub fh: u64,
    pub access: u32,
    deny: u32,
    owner: Box<[u8]>,
    pub sequence: u32,
}

pub(super) struct DirectoryView {
    fh: u64,
    change: u64,
    verifier: Verifier,
    last_cookie: u64,
    active: Instant,
}

type NfsResult<T> = Result<T, Status>;

pub(super) fn component(bytes: &[u8]) -> NfsResult<&OsStr> {
    if bytes.len() > MAX_NAME {
        return Err(NAMETOOLONG);
    }
    if bytes.is_empty()
        || matches!(bytes, b"." | b"..")
        || bytes.iter().any(|byte| matches!(byte, 0 | b'/'))
    {
        return Err(BADNAME);
    }
    Ok(OsStr::from_bytes(bytes))
}

fn access_flags(access: u32) -> i32 {
    match access & 3 {
        READ => libc::O_RDONLY,
        WRITE => libc::O_WRONLY,
        _ => libc::O_RDWR,
    }
}

pub(super) fn next_sequence(current: u32) -> u32 {
    current.wrapping_add(1).max(1)
}

fn cinfo(w: &mut Writer, before: u64, after: u64) {
    w.boolean(true); // operations and change counters are serialized
    w.u64(before);
    w.u64(after);
}

fn secinfo(w: &mut Writer) {
    w.u32(1);
    w.u32(1); // one security flavor, AUTH_SYS
}

impl Server {
    pub fn filehandle(&self, ino: u64) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..16].copy_from_slice(&self.run);
        bytes[16..].copy_from_slice(&ino.to_be_bytes());
        bytes
    }

    pub(super) fn inode_from_handle(&self, bytes: &[u8]) -> NfsResult<u64> {
        let bytes: &[u8; 24] = bytes.try_into().map_err(|_| BADHANDLE)?;
        let ino = u64::from_be_bytes(bytes[16..].try_into().unwrap());
        if bytes[..16] != self.run || !self.core.nfs_inode_exists(ino) {
            return Err(STALE);
        }
        Ok(ino)
    }

    pub fn state(&self, key: u64, sequence: u32) -> StateId {
        let mut other = [0; 12];
        other[..4].copy_from_slice(&self.run[..4]);
        other[4..].copy_from_slice(&key.to_be_bytes());
        StateId { sequence, other }
    }

    pub fn state_key(&self, id: StateId) -> NfsResult<u64> {
        if id.other[..4] != self.run[..4] {
            return Err(BAD_STATEID);
        }
        Ok(u64::from_be_bytes(id.other[4..].try_into().unwrap()))
    }

    pub fn check_state_sequence(id: StateId, current: u32) -> NfsResult<()> {
        match id.sequence {
            0 => Ok(()),
            actual if actual == current => Ok(()),
            // Sequence comparison is modulo 2^32, with zero reserved.
            actual if (current.wrapping_sub(actual) as i32) > 0 => Err(OLD_STATEID),
            _ => Err(BAD_STATEID),
        }
    }

    pub fn open_for_state(
        &self,
        cx: &Context,
        ino: u64,
        id: StateId,
    ) -> NfsResult<(u64, &OpenState)> {
        let id = cx.state_id(id)?;
        let key = self.state_key(id)?;
        let open = self
            .files
            .get(&key)
            .filter(|open| open.ino == ino && open.client == cx.client)
            .ok_or(BAD_STATEID)?;
        Self::check_state_sequence(id, open.sequence)?;
        Ok((key, open))
    }

    fn lookup(&mut self, cx: &Context, parent: u64, name: &[u8]) -> NfsResult<u64> {
        self.require_access(cx, parent, libc::X_OK)?;
        self.core
            .lookup_inode(parent, component(name)?)
            .map_err(errno)
    }

    pub fn check_file(&mut self, ino: u64) -> NfsResult<()> {
        match self.core.nfs_attributes(ino, false).map_err(errno)?.0.kind {
            FileType::RegularFile => Ok(()),
            FileType::Directory => Err(ISDIR),
            FileType::Symlink => Err(SYMLINK),
            _ => Err(BADTYPE),
        }
    }

    pub fn require_access(&self, cx: &Context, ino: u64, mode: i32) -> NfsResult<()> {
        self.core
            .check_access(
                ino,
                cx.credentials.uid,
                cx.credentials.gid,
                cx.credentials.groups(),
                mode,
            )
            .map_err(errno)
    }

    fn namespace_access(&mut self, cx: &Context, ino: u64, mode: i32) -> NfsResult<()> {
        if self.core.nfs_attributes(ino, false).map_err(errno)?.0.kind != FileType::Directory {
            return Err(NOTDIR);
        }
        self.require_access(cx, ino, libc::X_OK | mode)
    }

    fn io_handle(
        &mut self,
        cx: &Context,
        ino: u64,
        id: StateId,
        access: u32,
    ) -> NfsResult<(u64, bool)> {
        let id = cx.state_id(id)?;
        // RFC 8881 §8.2.3 treats the all-ones stateid as anonymous for writes.
        if id == StateId::ANONYMOUS || id == StateId::READ_BYPASS {
            let bypass = id == StateId::READ_BYPASS && access == READ;
            if !bypass && self.share_conflict(ino, access, 0) {
                return Err(SHARE_DENIED);
            }
            self.require_access(
                cx,
                ino,
                if access == WRITE {
                    libc::W_OK
                } else {
                    libc::R_OK
                },
            )?;
            return Ok((
                self.core
                    .open_file(ino, access_flags(access))
                    .map_err(errno)?,
                true,
            ));
        }
        let key = self.state_key(id)?;
        let open = if self.locks.contains_key(&key) {
            let (_, lock) = self.lock_for_state(cx, ino, id)?;
            self.files.get(&lock.open).ok_or(BAD_STATEID)?
        } else {
            self.open_for_state(cx, ino, id)?.1
        };
        if open.access & access == 0 {
            return Err(OPENMODE);
        }
        Ok((open.fh, false))
    }

    fn share_conflict(&self, ino: u64, access: u32, deny: u32) -> bool {
        self.files
            .values()
            .any(|open| open.ino == ino && (open.access & deny != 0 || open.deny & access != 0))
    }

    fn create(
        &mut self,
        cx: &Context,
        parent: u64,
        name: &OsStr,
        kind: FileType,
        target: &[u8],
        update: SetAttributes,
    ) -> NfsResult<u64> {
        self.namespace_access(cx, parent, libc::W_OK)?;
        if update.owner.is_some_and(|uid| uid != cx.credentials.uid)
            || update.group.is_some_and(|gid| gid != cx.credentials.gid)
        {
            return Err(PERM);
        }
        if update.size.is_some() && kind != FileType::RegularFile {
            return Err(INVAL);
        }
        let attr = self
            .core
            .create_node(
                parent,
                name,
                kind,
                update.mode.unwrap_or(0o777),
                cx.credentials.uid,
                cx.credentials.gid,
                target.to_vec(),
            )
            .map_err(errno)?;
        self.core.forget_inode(attr.ino, 1);
        if update.size.is_some_and(|size| size != 0)
            || update.atime.is_some()
            || update.mtime.is_some()
            || update.crtime.is_some()
        {
            if let Err(error) = self
                .core
                .set_attributes(attr.ino, cx.credentials.uid, update)
            {
                let _ = self.core.remove(parent, name, kind == FileType::Directory);
                return Err(errno(error));
            }
        }
        self.core.nfs_mark_changed(parent);
        Ok(attr.ino)
    }

    fn open(&mut self, cx: &mut Context, request: Open<'_>, w: &mut Writer) -> NfsResult<()> {
        let access = request.access & 3;
        if access == 0 || request.deny > 3 || request.access & !0x0003_ff03 != 0 {
            return Err(INVAL);
        }
        let initial = cx.inode()?;
        let mut change = (0, 0);
        let (attrs, guard, verifier) = match request.create {
            None => (None, false, None),
            Some(CreateHow::Unchecked(attrs)) => (Some(attrs), false, None),
            Some(CreateHow::Guarded(attrs)) => (Some(attrs), true, None),
            Some(CreateHow::Exclusive(verifier, attrs)) => (attrs, true, Some(verifier)),
        };
        let create = attrs.is_some() || verifier.is_some();
        if (access & WRITE != 0 || create) && self.core.read_only() {
            return Err(ROFS);
        }
        let mask = attrs.map_or(Bitmap::default(), |attr| attr.mask);
        let update = attrs.map(decode_set).transpose()?.unwrap_or_default();

        let truncate_on_existing = update.size == Some(0);
        let mut pending_create = None;
        let ino = match request.claim {
            Claim::Handle if !create => initial,
            Claim::Name(name) => {
                self.namespace_access(cx, initial, 0)?;
                let name = component(name)?;
                change.0 = self.core.nfs_change(initial).map_err(errno)?;
                change.1 = change.0;
                match self.core.lookup_inode(initial, name) {
                    Ok(ino) => {
                        if guard
                            && (verifier.is_none()
                                || self.core.nfs_create_verifier(ino) != verifier)
                        {
                            return Err(EXIST);
                        }
                        Some(ino)
                    }
                    Err(libc::ENOENT) if create => {
                        pending_create = Some(name);
                        None
                    }
                    Err(error) => return Err(errno(error)),
                }
                .unwrap_or(0)
            }
            Claim::Reclaim => return Err(NO_GRACE),
            Claim::Delegated => return Err(NOTSUPP),
            _ => return Err(INVAL),
        };

        let existing = self.files.iter().find_map(|(&key, open)| {
            (open.client == cx.client && open.ino == ino && open.owner.as_ref() == request.owner)
                .then_some((key, open.access, open.deny, open.sequence, open.fh))
        });
        let mut attrset = Bitmap::default();
        let ino = if let Some(name) = pending_create {
            let update = SetAttributes {
                mode: Some(update.mode.unwrap_or(0o666)),
                ..update
            };
            let ino = self.create(cx, initial, name, FileType::RegularFile, b"", update)?;
            if let Some(verifier) = verifier {
                self.core.nfs_record_create_verifier(ino, verifier);
            }
            attrset = mask;
            change.1 = self.core.nfs_change(initial).map_err(errno)?;
            ino
        } else {
            self.check_file(ino)?;
            let mut mode = 0;
            if access & READ != 0 {
                mode |= libc::R_OK;
            }
            if access & WRITE != 0 {
                mode |= libc::W_OK;
            }
            // Solaris-compatible mode semantics allow opens for executable
            // pages via a read reservation even on execute-only files.
            if let Err(error) = self.require_access(cx, ino, mode) {
                if !(mode == libc::R_OK && self.require_access(cx, ino, libc::X_OK).is_ok()) {
                    return Err(error);
                }
            }
            ino
        };

        // RFC 8881 §§9.7 and 18.16.3 include the same owner's earlier OPEN
        // when checking the new request's share reservations.
        if self.share_conflict(ino, access, request.deny) {
            return Err(SHARE_DENIED);
        }
        let truncating = pending_create.is_none() && create && !guard && truncate_on_existing;
        if truncating && access & WRITE == 0 {
            return Err(OPENMODE);
        }
        let (key, sequence) = if let Some((key, old_access, old_deny, old_seq, fh)) = existing {
            let sequence = next_sequence(old_seq);
            let access = access | old_access;
            if truncating {
                // OPEN already checked the requested write access. Commit
                // the new reservation only after the truncate succeeds.
                self.core.truncate(ino, 0).map_err(errno)?;
            }
            self.core
                .nfs_set_handle_flags(ino, fh, access_flags(access))
                .map_err(errno)?;
            let open = self.files.get_mut(&key).ok_or(SERVERFAULT)?;
            open.access = access;
            open.deny = old_deny | request.deny;
            open.sequence = sequence;
            (key, sequence)
        } else {
            let flags = access_flags(access) | if truncating { libc::O_TRUNC } else { 0 };
            let fh = self.core.open_file(ino, flags).map_err(errno)?;
            let key = self.id();
            self.files.insert(
                key,
                OpenState {
                    client: cx.client,
                    ino,
                    fh,
                    access,
                    deny: request.deny,
                    owner: request.owner.into(),
                    sequence: 1,
                },
            );
            (key, 1)
        };
        if truncating {
            self.core.nfs_mark_changed(ino);
            attrset.set(4);
        }
        let state = self.state(key, sequence);
        cx.set_inode(ino);
        cx.current_state = Some(state);
        state.encode(w);
        cinfo(w, change.0, change.1);
        w.u32(4 | 8); // POSIX locks and preserved open-unlinked files
        attrset.encode(w);
        w.u32(0); // OPEN_DELEGATE_NONE
        Ok(())
    }

    fn close(&mut self, cx: &mut Context, id: StateId, w: &mut Writer) -> NfsResult<()> {
        let (key, open) = self.open_for_state(cx, cx.inode()?, id)?;
        let (ino, fh) = (open.ino, open.fh);
        // An operation later in this compound can use the same current
        // filehandle, as Darwin does for the GETATTR after CLOSE.
        if self.core.nfs_retain_unlinked_inode(ino).map_err(errno)? {
            cx.closed_inodes.push(ino);
        }
        self.core.release_handle(ino, fh).map_err(errno)?;
        self.files.remove(&key);
        self.locks.retain(|_, lock| lock.open != key);
        StateId::READ_BYPASS.encode(w); // CLOSE invalidates the stateid
        cx.current_state = None;
        Ok(())
    }

    fn downgrade(
        &mut self,
        cx: &mut Context,
        id: StateId,
        access: u32,
        deny: u32,
        w: &mut Writer,
    ) -> NfsResult<()> {
        let (key, open) = self.open_for_state(cx, cx.inode()?, id)?;
        if access == 0 || access & !open.access != 0 || deny & !open.deny != 0 {
            return Err(INVAL);
        }
        self.core
            .nfs_set_handle_flags(open.ino, open.fh, access_flags(access))
            .map_err(errno)?;
        let open = self.files.get_mut(&key).ok_or(SERVERFAULT)?;
        open.access = access;
        open.deny = deny;
        open.sequence = next_sequence(open.sequence);
        let sequence = open.sequence;
        let state = self.state(key, sequence);
        cx.current_state = Some(state);
        state.encode(w);
        Ok(())
    }

    fn read(
        &mut self,
        cx: &Context,
        id: StateId,
        offset: u64,
        count: u32,
        limit: usize,
        w: &mut Writer,
    ) -> NfsResult<()> {
        let ino = cx.inode()?;
        self.check_file(ino)?;
        let offset = i64::try_from(offset).map_err(|_| FBIG)?;
        let data_limit = limit.checked_sub(8).ok_or(REP_TOO_BIG)? & !3;
        let count = (count as usize).min(MAX_DATA).min(data_limit);
        let (fh, temporary) = self.io_handle(cx, ino, id, READ)?;
        let data = self
            .core
            .read_data(ino, fh, offset, count as u32)
            .map_err(errno);
        self.release_temporary(ino, fh, temporary);
        let data = data?;
        let (attr, _) = self.core.nfs_attributes(ino, true).map_err(errno)?;
        w.boolean(offset as u64 + data.len() as u64 >= attr.size);
        w.opaque(&data);
        Ok(())
    }

    fn write(
        &mut self,
        cx: &Context,
        id: StateId,
        offset: u64,
        stability: u32,
        data: &[u8],
        w: &mut Writer,
    ) -> NfsResult<()> {
        let ino = cx.inode()?;
        if self.core.read_only() {
            return Err(ROFS);
        }
        self.check_file(ino)?;
        let offset = i64::try_from(offset).map_err(|_| FBIG)?;
        if stability > 2 {
            return Err(INVAL);
        }
        let (fh, temporary) = self.io_handle(cx, ino, id, WRITE)?;
        let result = self
            .core
            .write_data(ino, fh, offset, data, 0, 0)
            .map_err(|error| self.writeback_error(error))
            .and_then(|written| {
                if written > 0 {
                    self.core.nfs_mark_changed(ino);
                }
                if stability != 0 {
                    self.core
                        .sync_file(ino, fh, stability == 1)
                        .map_err(|error| self.writeback_error(error))?;
                }
                Ok(written)
            });
        self.release_temporary(ino, fh, temporary);
        w.u32(result?);
        w.u32(stability);
        w.fixed(&self.run[..8]);
        Ok(())
    }

    fn release_temporary(&mut self, ino: u64, fh: u64, temporary: bool) {
        if temporary {
            let _ = self.core.release_handle(ino, fh);
        }
    }

    fn drop_directory(&mut self, key: (u64, u64)) {
        if let Some(view) = self.directories.remove(&key) {
            let _ = self.core.release_handle(key.1, view.fh);
        }
    }

    pub fn drop_client_files(&mut self, client: u64) {
        self.files.retain(|_, open| {
            if open.client == client {
                let _ = self.core.release_handle(open.ino, open.fh);
                false
            } else {
                true
            }
        });
        self.locks
            .retain(|_, lock| self.files.contains_key(&lock.open));
        let dirs: Vec<_> = self
            .directories
            .keys()
            .filter(|key| key.0 == client)
            .copied()
            .collect();
        for key in dirs {
            self.drop_directory(key);
        }
    }

    pub fn drop_expired_directories(&mut self, now: Instant, age: Duration) {
        let dirs: Vec<_> = self
            .directories
            .iter()
            .filter(|(_, view)| now.duration_since(view.active) >= age)
            .map(|(&key, _)| key)
            .collect();
        for key in dirs {
            self.drop_directory(key);
        }
    }

    fn directory_view(
        &mut self,
        key: (u64, u64),
        cookie: u64,
        verifier: Verifier,
    ) -> NfsResult<(u64, Verifier)> {
        if cookie != 0 {
            let view = self.directories.get_mut(&key).ok_or(BAD_COOKIE)?;
            if view.verifier != verifier {
                return Err(NOT_SAME);
            }
            if cookie < 3 || cookie > view.last_cookie {
                return Err(BAD_COOKIE);
            }
            view.active = Instant::now();
            return Ok((view.fh, view.verifier));
        }
        let change = self.core.nfs_change(key.1).map_err(errno)?;
        if let Some(view) = self
            .directories
            .get_mut(&key)
            .filter(|view| view.change == change)
        {
            view.active = Instant::now();
            return Ok((view.fh, view.verifier));
        }
        self.drop_directory(key);
        if self.directories.len() >= MAX_DIRECTORY_VIEWS {
            if let Some((&oldest, _)) = self.directories.iter().min_by_key(|(_, view)| view.active)
            {
                self.drop_directory(oldest);
            }
        }
        let fh = self
            .core
            .open_directory(key.1, libc::O_RDONLY)
            .map_err(errno)?;
        let verifier = self.id().to_be_bytes();
        self.directories.insert(
            key,
            DirectoryView {
                fh,
                change,
                verifier,
                last_cookie: 2,
                active: Instant::now(),
            },
        );
        Ok((fh, verifier))
    }

    #[allow(clippy::too_many_arguments)]
    fn read_directory(
        &mut self,
        cx: &Context,
        cookie: u64,
        verifier: Verifier,
        dircount: u32,
        maxcount: usize,
        mask: Bitmap,
        w: &mut Writer,
    ) -> NfsResult<()> {
        let ino = cx.inode()?;
        self.namespace_access(cx, ino, libc::R_OK)?;
        if maxcount < 16 {
            return Err(TOOSMALL);
        }
        let key = (cx.client, ino);
        let (fh, verifier) = self.directory_view(key, cookie, verifier)?;
        let mut rows = Vec::new();
        let mut names_budget = 0;
        let mut eof = true;
        self.core
            .read_directory(ino, fh, cookie.max(2), |child, kind, name, cookie| {
                let size = 12 + name.as_bytes().len().next_multiple_of(4);
                if rows.len() >= MAX_DIRECTORY_ROWS
                    || (!rows.is_empty()
                        && dircount != 0
                        && names_budget + size > dircount as usize)
                    || names_budget + size > maxcount
                {
                    eof = false;
                    return true;
                }
                names_budget += size;
                rows.push((child, kind, name.to_os_string(), cookie));
                false
            })
            .map_err(|error| {
                if error == libc::EINVAL {
                    BAD_COOKIE
                } else {
                    errno(error)
                }
            })?;

        w.fixed(&verifier);
        let mut issued = None;
        for (child, kind, name, row_cookie) in rows {
            let mut row = Writer::default();
            row.boolean(true);
            row.u64(row_cookie);
            row.opaque(name.as_bytes());
            self.directory_attributes(ino, child, kind, &name, mask, &mut row)?;
            if w.len() + row.len() + 8 > maxcount {
                eof = false;
                break;
            }
            w.fixed(row.bytes());
            issued = Some(row_cookie);
        }
        if let Some(last) = issued {
            let view = self.directories.get_mut(&key).ok_or(SERVERFAULT)?;
            view.last_cookie = view.last_cookie.max(last);
        } else if !eof {
            return Err(TOOSMALL);
        }
        w.boolean(false);
        w.boolean(eof);
        Ok(())
    }

    fn directory_attributes(
        &mut self,
        parent: u64,
        child: u64,
        kind: FileType,
        name: &OsString,
        mask: Bitmap,
        w: &mut Writer,
    ) -> NfsResult<()> {
        let cheap = Bitmap::from_indices(&[1, 11, 20, 55]);
        if mask.intersection(cheap) == mask {
            mask.encode(w);
            let mut values = Writer::default();
            for bit in mask.iter() {
                match bit {
                    1 => values.u32(file_type(kind)),
                    11 => values.u32(OK),
                    _ => values.u64(child),
                }
            }
            w.opaque(values.bytes());
            return Ok(());
        }
        let result = self
            .core
            .lookup_inode(parent, name)
            .map_err(errno)
            .and_then(|inode| {
                if inode != child {
                    Err(STALE)
                } else {
                    self.get_attributes(inode, mask, true, w)
                }
            });
        if let Err(error) = result {
            if error == IO {
                self.io_error();
            }
            directory_attribute_error(mask, error, w)?;
        }
        Ok(())
    }

    pub fn filesystem(
        &mut self,
        op: Operation<'_>,
        cx: &mut Context,
        limit: usize,
        w: &mut Writer,
    ) -> NfsResult<()> {
        use Operation::*;
        match op {
            Access(mask) => {
                let ino = cx.inode()?;
                let kind = self.core.nfs_attributes(ino, false).map_err(errno)?.0.kind;
                let supported = mask
                    & if kind == FileType::Directory {
                        0x1f
                    } else {
                        0x2d
                    };
                let mut allowed = 0;
                for (bit, mode) in [
                    (1, libc::R_OK),
                    (2, libc::X_OK),
                    (4, libc::W_OK),
                    (8, libc::W_OK),
                    (16, libc::W_OK | libc::X_OK),
                    (32, libc::X_OK),
                ] {
                    if supported & bit != 0 && self.require_access(cx, ino, mode).is_ok() {
                        allowed |= bit;
                    }
                }
                w.u32(supported);
                w.u32(allowed);
            }
            PutRoot => cx.set_inode(1),
            PutFh(handle) => cx.set_inode(self.inode_from_handle(handle)?),
            GetFh => w.opaque(&self.filehandle(cx.inode()?)),
            Lookup(name) => cx.set_inode(self.lookup(cx, cx.inode()?, name)?),
            LookupParent => {
                let ino = cx.inode()?;
                self.namespace_access(cx, ino, 0)?;
                cx.set_inode(self.core.nfs_parent(ino).map_err(errno)?);
            }
            Save => {
                cx.saved = Some(cx.inode()?);
                cx.saved_state = cx.current_state;
            }
            Restore => {
                let ino = cx.saved.ok_or(RESTOREFH)?;
                if !self.core.nfs_inode_exists(ino) {
                    return Err(STALE);
                }
                cx.fh = cx.saved;
                cx.current_state = cx.saved_state;
            }
            GetAttr(mask) => self.get_attributes(cx.inode()?, mask, false, w)?,
            Verify(attrs, negated) => self.verify_attributes(cx.inode()?, attrs, negated)?,
            Open(open) => self.open(cx, open, w)?,
            Close(id) => self.close(cx, id, w)?,
            Downgrade(id, access, deny) => self.downgrade(cx, id, access, deny, w)?,
            Read(id, offset, count) => self.read(cx, id, offset, count, limit, w)?,
            Write(id, offset, stability, data) => self.write(cx, id, offset, stability, data, w)?,
            Commit => {
                let ino = cx.inode()?;
                self.check_file(ino)?;
                self.core
                    .commit_file(ino)
                    .map_err(|error| self.writeback_error(error))?;
                w.fixed(&self.run[..8]);
            }
            ReadDir {
                cookie,
                verifier,
                dircount,
                maxcount,
                mask,
            } => {
                self.read_directory(
                    cx,
                    cookie,
                    verifier,
                    dircount,
                    (maxcount as usize).min(limit),
                    mask,
                    w,
                )?;
            }
            ReadLink => {
                let ino = cx.inode()?;
                let (attr, _) = self.core.nfs_attributes(ino, true).map_err(errno)?;
                if attr.kind != FileType::Symlink {
                    return Err(INVAL);
                }
                if attr.size > MAX_LINK as u64 {
                    return Err(NAMETOOLONG);
                }
                let bytes = self.core.read_link(ino).map_err(errno)?;
                if 4 + bytes.len().next_multiple_of(4) > limit {
                    return Err(REP_TOO_BIG);
                }
                w.opaque(&bytes);
            }
            Create {
                kind,
                name,
                target,
                attrs,
            } => {
                let parent = cx.inode()?;
                self.namespace_access(cx, parent, libc::W_OK)?;
                let kind = match kind {
                    2 => FileType::Directory,
                    5 if target.is_empty() => return Err(INVAL),
                    5 => FileType::Symlink,
                    6 => FileType::Socket,
                    7 => FileType::NamedPipe,
                    3 | 4 => return Err(PERM),
                    _ => return Err(BADTYPE),
                };
                let before = self.core.nfs_change(parent).map_err(errno)?;
                let ino = self.create(
                    cx,
                    parent,
                    component(name)?,
                    kind,
                    target,
                    decode_set(attrs)?,
                )?;
                cinfo(w, before, self.core.nfs_change(parent).map_err(errno)?);
                attrs.mask.encode(w);
                cx.set_inode(ino);
            }
            SetAttr(id, attrs) => {
                let ino = cx.inode()?;
                self.core.writable(ino).map_err(errno)?;
                let update = decode_set(attrs)?;
                let state = cx.state_id(id)?;
                let (fh, temporary) = if update.size.is_some() {
                    self.check_file(ino)?;
                    let (fh, temporary) = self.io_handle(cx, ino, state, WRITE)?;
                    (Some(fh), temporary)
                } else if !matches!(state, StateId::ANONYMOUS | StateId::READ_BYPASS) {
                    (Some(self.open_for_state(cx, ino, state)?.1.fh), false)
                } else {
                    (None, false)
                };
                let result = self.core.set_attributes(
                    ino,
                    cx.credentials.uid,
                    SetAttributes { fh, ..update },
                );
                if let Some(fh) = fh {
                    self.release_temporary(ino, fh, temporary);
                }
                result.map_err(errno)?;
                self.core.nfs_mark_changed(ino);
                attrs.mask.encode(w);
            }
            Remove(name) => {
                let parent = cx.inode()?;
                self.namespace_access(cx, parent, libc::W_OK)?;
                let name = component(name)?;
                let ino = self.core.lookup_inode(parent, name).map_err(errno)?;
                let is_dir = self.core.nfs_attributes(ino, false).map_err(errno)?.0.kind
                    == FileType::Directory;
                let before = self.core.nfs_change(parent).map_err(errno)?;
                self.core.remove(parent, name, is_dir).map_err(errno)?;
                self.core.nfs_mark_changed(ino);
                self.core.nfs_mark_changed(parent);
                cinfo(w, before, self.core.nfs_change(parent).map_err(errno)?);
            }
            Rename(old, new) => self.rename(cx, old, new, w)?,
            Link(name) => {
                let parent = cx.inode()?;
                let ino = cx.saved.ok_or(NOFILEHANDLE)?;
                self.namespace_access(cx, parent, libc::W_OK)?;
                let before = self.core.nfs_change(parent).map_err(errno)?;
                self.core
                    .link_node(ino, parent, component(name)?)
                    .map_err(errno)?;
                self.core.forget_inode(ino, 1);
                self.core.nfs_mark_changed(ino);
                self.core.nfs_mark_changed(parent);
                cinfo(w, before, self.core.nfs_change(parent).map_err(errno)?);
            }
            SecInfo(name) => {
                self.lookup(cx, cx.inode()?, name)?;
                secinfo(w);
                cx.fh = None;
                cx.current_state = None;
            }
            SecInfoNoName(style) => {
                let ino = cx.inode()?;
                match style {
                    0 => (),
                    1 => {
                        self.core.nfs_parent(ino).map_err(errno)?;
                    }
                    _ => return Err(INVAL),
                }
                secinfo(w);
                cx.fh = None;
                cx.current_state = None;
            }
            Lock {
                range,
                reclaim,
                owner,
            } => self.lock(cx, range, reclaim, owner, w)?,
            LockTest(range, owner) => self.lock_test(cx, range, owner, w)?,
            Unlock(range, state) => self.unlock(cx, range, state, w)?,
            FreeStateId(state) => self.free_lock_state(cx, state)?,
            TestStateId(ids) => {
                w.u32(ids.len() as u32);
                for id in ids {
                    w.u32(self.test_state(cx, id).err().unwrap_or(OK));
                }
            }
            _ => return Err(NOTSUPP),
        }
        Ok(())
    }

    fn rename(&mut self, cx: &Context, old: &[u8], new: &[u8], w: &mut Writer) -> NfsResult<()> {
        let from = cx.saved.ok_or(NOFILEHANDLE)?;
        let to = cx.inode()?;
        self.namespace_access(cx, from, libc::W_OK)?;
        self.namespace_access(cx, to, libc::W_OK)?;
        let old = component(old)?;
        let new = component(new)?;
        let source = self.core.lookup_inode(from, old).map_err(errno)?;
        let replaced = match self.core.lookup_inode(to, new) {
            Ok(ino) => Some(ino),
            Err(libc::ENOENT) => None,
            Err(error) => return Err(errno(error)),
        };
        let from_before = self.core.nfs_change(from).map_err(errno)?;
        let to_before = self.core.nfs_change(to).map_err(errno)?;
        self.core
            .rename_path(from, old, to, new, 0)
            .map_err(errno)?;
        self.core.nfs_mark_changed(source);
        self.core.nfs_mark_changed(from);
        if from != to {
            self.core.nfs_mark_changed(to);
        }
        if let Some(replaced) = replaced.filter(|ino| *ino != source) {
            self.core.nfs_mark_changed(replaced);
        }
        cinfo(w, from_before, self.core.nfs_change(from).map_err(errno)?);
        cinfo(w, to_before, self.core.nfs_change(to).map_err(errno)?);
        Ok(())
    }

    fn test_state(&self, cx: &Context, id: StateId) -> NfsResult<()> {
        let id = cx.state_id(id)?;
        let key = self.state_key(id)?;
        if let Some(open) = self.files.get(&key).filter(|open| open.client == cx.client) {
            return Self::check_state_sequence(id, open.sequence);
        }
        let lock = self.locks.get(&key).ok_or(BAD_STATEID)?;
        self.lock_for_state(cx, lock.ino, id).map(|_| ())
    }
}
